//! The MusicBrainz client, its HTTP half: the one-a-second gate, the cache and the "musicbrainz"
//! client (base address https://musicbrainz.org/ws/2/, a 10 s timeout, Octo's User-Agent). The
//! query builders and the readers are in `octo_core::fingerprint::music_brainz_client`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::{SongIdentity, dotnet, octo_user_agent};
use octo_core::fingerprint::music_brainz_client as pure;
use octo_core::fingerprint::recording_identity::RecordingFacts;
use octo_core::json::element::ElementError;
use octo_core::tagging::{ReleaseDetails, ReleaseLookup};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::time::Instant;
use tracing::{debug, warn};
use url::Url;

use crate::services::framework::http::{client_builder, parse_json, resolve};
use crate::services::framework::{HttpAnswer, MemoryCache};

/// The client's errors, which callers catch.
#[derive(Debug, thiserror::Error)]
pub enum MusicBrainzError {
    /// The client's timeout elapsed. It reaches the caller rather than reading as "nothing to
    /// say".
    #[error("No answer came within 10 seconds, so Octo stopped waiting.")]
    TimedOut,
    /// The answer could not be read: a field of the wrong kind.
    #[error(transparent)]
    Malformed(#[from] ElementError),
}

#[derive(Clone)]
enum Cached {
    Release(Arc<ReleaseDetails>),
    Json(Value),
    Text(String),
}

/// How MusicBrainz answered one question.
#[derive(Debug)]
enum Asked {
    /// A 200 with a readable body.
    Answer(Value),
    /// A 404, or a 400 saying the id is not a MusicBrainz id: MusicBrainz knows nothing by it.
    NotKnown,
    /// Any other 4xx but 429: MusicBrainz does not take the question as it was asked.
    Refused,
    /// A 429, a 5xx, a body that could not be read, or no answer at all: worth asking again.
    Busy,
}

impl Asked {
    /// The answer, or None for every way of having none.
    fn found(self) -> Option<Value> {
        match self {
            Asked::Answer(doc) => Some(doc),
            Asked::NotKnown | Asked::Refused | Asked::Busy => None,
        }
    }
}

/// What MusicBrainz said about one code.
#[derive(Debug, Clone, PartialEq)]
pub enum IsrcLookup {
    /// The recordings that carry the code, as `{"isrc": <code>, "recordings": [...]}`.
    Found(Value),
    /// MusicBrainz knows no recording with the code.
    NotKnown,
    /// MusicBrainz could not be asked: busy or unreachable. Worth asking again.
    Unanswered,
    /// MusicBrainz refused the question as Octo asked it: a definite answer, never worth
    /// asking again the same way.
    Refused,
}

/// The releases kept per recording of a code: the candidate reader takes no more.
const MOST_RELEASES_PER_RECORDING: usize = 25;

/// One question for MusicBrainz, asked only when a person keeps a track AcoustID had never heard
/// of: which recording is this? That is the MusicBrainz id a confirmed fingerprint needs before
/// AcoustID will take it (#47). And one more, asked only while verifying a download whose
/// fingerprint named a recording that reads differently from the request: which ISRCs does
/// that recording carry?
///
/// MusicBrainz allows about one request a second and wants a User-Agent naming the application,
/// and it holds near-duplicate recordings (two "Teardrop" by Massive Attack, 27 ms apart), so an
/// answer is only returned when exactly one recording fits. Anything else is a guess, and a
/// guess is never submitted with someone's name on it.
pub struct MusicBrainzClient {
    client: reqwest::Client,
    base: Url,
    /// One call at a time, and when the last one was made: held across the wait and the call. The
    /// background lane only ever tries it (see [`Self::get_background`]).
    gate: tokio::sync::Mutex<Option<LastCall>>,
    /// How long the background lane waits when the gate is busy.
    background_backoff: Duration,
    /// Release lookups are remembered a day, since an album's tracks ask for the same
    /// release one by one; searches six hours. Every entry counts as one, so the limit is a count.
    cache: MemoryCache<Cached>,
    /// Exact-name artist answers by lowercase name, kept for the life of the process.
    artists: Mutex<HashMap<String, Vec<pure::MusicBrainzArtist>>>,
    /// Recording credits by lowercase recording id, kept for the life of the process.
    credits: Mutex<HashMap<String, Vec<pure::CreditPart>>>,
}

impl MusicBrainzClient {
    pub const CLIENT_NAME: &'static str = "musicbrainz";
    pub const BASE_ADDRESS: &'static str = "https://musicbrainz.org/ws/2/";
    const TIMEOUT: Duration = Duration::from_secs(10);
    const MINIMUM_GAP: Duration = Duration::from_millis(1100);
    const RELEASE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    const SEARCH_TTL: Duration = Duration::from_secs(6 * 60 * 60);
    const ARTIST_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    const BACKGROUND_BACKOFF: Duration = Duration::from_secs(5);

    pub fn new() -> Self {
        Self::with_base_url(Url::parse(Self::BASE_ADDRESS).expect("the MusicBrainz base address parses"))
    }

    /// A client for another base address: a test's mock server.
    pub fn with_base_url(base: Url) -> Self {
        let client = client_builder()
            .timeout(Self::TIMEOUT)
            .user_agent(octo_user_agent::value())
            .build()
            .expect("the MusicBrainz HTTP client builds");
        Self {
            client,
            base,
            gate: tokio::sync::Mutex::new(None),
            background_backoff: Self::BACKGROUND_BACKOFF,
            cache: MemoryCache::new(2000),
            artists: Mutex::new(HashMap::new()),
            credits: Mutex::new(HashMap::new()),
        }
    }

    /// One release in full: its label and catalog number, barcode, status, country, date, its
    /// group's kind and first release date, every track's position and id, and the genres people
    /// voted on. Remembered a day, since an album's tracks ask one by one.
    pub async fn lookup_release(
        &self,
        release_id: &str,
    ) -> Result<Option<Arc<ReleaseDetails>>, MusicBrainzError> {
        self.lookup_release_on(release_id, false).await
    }

    /// [`Self::lookup_release`] on the background lane, for the library sweep: it steps back
    /// whenever a download is asking.
    pub async fn lookup_release_in_background(
        &self,
        release_id: &str,
    ) -> Result<Option<Arc<ReleaseDetails>>, MusicBrainzError> {
        self.lookup_release_on(release_id, true).await
    }

    async fn lookup_release_on(
        &self,
        release_id: &str,
        background: bool,
    ) -> Result<Option<Arc<ReleaseDetails>>, MusicBrainzError> {
        if dotnet::is_blank(release_id) {
            return Ok(None);
        }
        let key = format!("release|{}", dotnet::to_lower_invariant(release_id.trim()));
        if let Some(Cached::Release(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }

        let url = format!(
            "release/{}?inc=labels+release-groups+artist-credits+recordings+isrcs+genres&fmt=json",
            dotnet::escape_data_string(release_id.trim())
        );
        let asked = if background {
            self.get_background(&url, "release lookup").await?
        } else {
            self.get(&url, "release lookup").await?
        };
        let Some(doc) = asked else {
            return Ok(None);
        };
        let details = ReleaseDetails::parse(&doc).map(Arc::new);
        if let Some(details) = &details {
            self.cache
                .set(key, Cached::Release(Arc::clone(details)), 1, Self::RELEASE_TTL);
        }
        Ok(details)
    }

    /// Recordings by name and length, for a download the fingerprint service could not name. Up
    /// to 25, each with its releases and their groups.
    pub async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> Result<Option<Value>, MusicBrainzError> {
        if dotnet::is_blank(artist) && dotnet::is_blank(title) {
            return Ok(None);
        }
        let key = format!(
            "search|{}|{duration_seconds}",
            SongIdentity::match_key(artist, title)
        );
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }

        let url = pure::build_recording_search_url(artist, title, duration_seconds);
        let Some(doc) = self.get(&url, "recording search").await? else {
            return Ok(None);
        };
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(Some(doc))
    }

    /// The ISRCs of the recordings that are `title` by `artist` (at about `seconds` long when
    /// known), best match first: see [`pure::isrcs_of_recordings`]. One search, inside the same
    /// one-a-second budget, remembered six hours.
    pub async fn isrcs_for(
        &self,
        artist: &str,
        title: &str,
        seconds: Option<i32>,
    ) -> Result<Vec<String>, MusicBrainzError> {
        if dotnet::is_blank(artist) || dotnet::is_blank(title) {
            return Ok(Vec::new());
        }
        let Some(doc) = self
            .search_recordings(artist, title, seconds.filter(|s| *s > 0).unwrap_or(0))
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(pure::isrcs_of_recordings(&doc, artist, title, seconds))
    }

    /// Every recording of one song by one artist, whatever its version or length: the version
    /// check reads which of them a file's length is. On the background lane; answers are kept
    /// as long as a search's.
    pub async fn song_recordings_in_background(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<Option<Value>, MusicBrainzError> {
        if dotnet::is_blank(artist) || dotnet::is_blank(title) {
            return Ok(None);
        }
        let key = format!("song|{}", SongIdentity::match_key(artist, title));
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }
        let url = pure::build_recording_search_url(artist, title, 0);
        let Some(doc) = self.get_background(&url, "song recordings search").await? else {
            return Ok(None);
        };
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(Some(doc))
    }

    /// The recordings that carry one code, each with its releases, their groups and the code's
    /// track on them: a search by code, since the code lookup itself leaves the releases out and
    /// refuses their groups. A recording the search lists without the code, and a DJ-mix segment,
    /// are left out. Only a found answer is remembered.
    pub async fn lookup_isrc(&self, isrc: &str) -> Result<IsrcLookup, MusicBrainzError> {
        self.code_search(isrc).await
    }

    /// Every recording that carries one code, without its releases, asked on the background
    /// lane for work nobody waits on: the code lookup, which knows a code the moment it is
    /// entered and lists each recording once. A DJ-mix segment is left out. Only a found answer
    /// is remembered.
    pub async fn isrc_recordings_in_background(&self, isrc: &str) -> Result<IsrcLookup, MusicBrainzError> {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return Ok(IsrcLookup::NotKnown);
        };
        let key = format!("isrc-recordings|{code}");
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(IsrcLookup::Found(cached));
        }
        let lookup = format!("isrc/{code}?inc=artist-credits&fmt=json");
        let recordings = match self.ask_on(&lookup, "code lookup", true).await? {
            Asked::Answer(doc) => recordings_of(&doc, |_| true),
            Asked::NotKnown => Vec::new(),
            Asked::Refused => return Ok(IsrcLookup::Refused),
            Asked::Busy => return Ok(IsrcLookup::Unanswered),
        };
        if recordings.is_empty() {
            return Ok(IsrcLookup::NotKnown);
        }
        let doc = serde_json::json!({ "isrc": code, "recordings": recordings });
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(IsrcLookup::Found(doc))
    }

    async fn code_search(&self, isrc: &str) -> Result<IsrcLookup, MusicBrainzError> {
        let Some(code) = SongIdentity::normalize_isrc(isrc) else {
            return Ok(IsrcLookup::NotKnown);
        };
        let key = format!("isrc|{code}");
        if let Some(Cached::Json(cached)) = self.cache.get(&key) {
            return Ok(IsrcLookup::Found(cached));
        }
        let search = format!("recording?query=isrc:{code}&limit=100&fmt=json");
        let recordings = match self.ask(&search, "code search").await? {
            Asked::Answer(doc) => recordings_of(&doc, |recording| carries(recording, &code)),
            Asked::NotKnown => Vec::new(),
            Asked::Refused => return Ok(IsrcLookup::Refused),
            Asked::Busy => return Ok(IsrcLookup::Unanswered),
        };
        if recordings.is_empty() {
            return Ok(IsrcLookup::NotKnown);
        }
        let doc = serde_json::json!({ "isrc": code, "recordings": recordings });
        self.cache
            .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
        Ok(IsrcLookup::Found(doc))
    }

    /// The one recording that is this song, this version, by this artist, at this length.
    pub async fn find_recording(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> Result<Option<String>, MusicBrainzError> {
        let Some(url) = pure::find_recording_url(artist, title, duration_seconds) else {
            return Ok(None);
        };
        match self.get(&url, "recording search").await? {
            Some(doc) => Ok(pure::pick(&doc, artist, title, duration_seconds)?),
            None => Ok(None),
        }
    }

    /// The ISRCs MusicBrainz lists for one recording: empty when it lists none, None when it
    /// could not be asked. One lookup by id with inc=isrcs, inside the same one-a-second budget
    /// as the search above.
    pub async fn fetch_isrcs(&self, recording_id: &str) -> Result<Option<Vec<String>>, MusicBrainzError> {
        if dotnet::is_blank(recording_id) {
            return Ok(None);
        }
        let url = format!(
            "recording/{}?inc=isrcs&fmt=json",
            dotnet::escape_data_string(recording_id)
        );
        match self.get(&url, "ISRC lookup").await? {
            Some(doc) => Ok(Some(pure::parse_isrcs(&doc)?)),
            None => Ok(None),
        }
    }

    /// What the music database says one recording is: its disambiguation and every release it
    /// is on. None when it could not be asked. `background` asks on the background lane, for
    /// the library sweep. Answers are remembered a day, since a recording's releases rarely
    /// change.
    pub async fn recording_facts(
        &self,
        recording_id: &str,
        background: bool,
    ) -> Result<Option<RecordingFacts>, MusicBrainzError> {
        let Some(url) = pure::recording_facts_url(recording_id) else {
            return Ok(None);
        };
        let key = format!("facts|{}", dotnet::to_lower_invariant(recording_id.trim()));
        let doc = match self.cache.get(&key) {
            Some(Cached::Json(cached)) => cached,
            _ => {
                let asked = if background {
                    self.get_background(&url, "recording lookup").await?
                } else {
                    self.get(&url, "recording lookup").await?
                };
                let Some(doc) = asked else {
                    return Ok(None);
                };
                self.cache
                    .set(key, Cached::Json(doc.clone()), 1, Self::ARTIST_TTL);
                doc
            }
        };
        Ok(Some(pure::parse_recording_facts(&doc)?))
    }

    /// The release group of the oldest official studio album a song appears on, or of a
    /// soundtrack when no studio album has it. None when MusicBrainz knows neither.
    pub async fn find_studio_album(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<Option<String>, MusicBrainzError> {
        let Some((url, plain_title)) = pure::studio_album_search(artist, title) else {
            return Ok(None);
        };
        match self.get(&url, "studio album search").await? {
            Some(doc) => Ok(pure::pick_studio_album(&doc, &plain_title)?),
            None => Ok(None),
        }
    }

    /// The barcodes of an album's official releases, digital ones first (see
    /// [`pure::barcodes_of_releases`]). On the background lane; remembered six hours.
    pub async fn album_barcodes(&self, artist: &str, album: &str) -> Result<Vec<String>, MusicBrainzError> {
        let Some(url) = pure::album_releases_url(artist, album) else {
            return Ok(Vec::new());
        };
        let key = format!("album-barcodes|{}", SongIdentity::match_key(artist, album));
        let doc = match self.cache.get(&key) {
            Some(Cached::Json(cached)) => cached,
            _ => {
                let Some(doc) = self.get_background(&url, "album releases search").await? else {
                    return Ok(Vec::new());
                };
                self.cache
                    .set(key, Cached::Json(doc.clone()), 1, Self::SEARCH_TTL);
                doc
            }
        };
        Ok(pure::barcodes_of_releases(&doc, artist, album)?)
    }

    /// Where the artist with this MusicBrainz id is from: a two-letter country, empty when
    /// MusicBrainz names none, None when it could not be asked. On the background lane;
    /// answers are remembered a day.
    pub async fn artist_country(&self, mbid: &str) -> Result<Option<String>, MusicBrainzError> {
        let Some(url) = pure::artist_lookup_url(mbid) else {
            return Ok(None);
        };
        let key = format!("artist|{}", dotnet::to_lower_invariant(mbid.trim()));
        if let Some(Cached::Text(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }
        let Some(doc) = self.get_background(&url, "artist lookup").await? else {
            return Ok(None);
        };
        let country = pure::artist_country(&doc);
        self.cache
            .set(key, Cached::Text(country.clone()), 1, Self::ARTIST_TTL);
        Ok(Some(country))
    }

    /// Where the artist of this name is from, by a name search that takes only the same name
    /// (`pure::searched_artist_country`): a two-letter country, empty when no result is that
    /// artist or it names no country, None when MusicBrainz could not be asked. On the
    /// background lane; answers are remembered a day.
    pub async fn search_artist_country(&self, name: &str) -> Result<Option<String>, MusicBrainzError> {
        let Some(url) = pure::artist_search_url(name) else {
            return Ok(None);
        };
        let key = format!("artist-search|{}", SongIdentity::key(name));
        if let Some(Cached::Text(cached)) = self.cache.get(&key) {
            return Ok(Some(cached));
        }
        let Some(doc) = self.get_background(&url, "artist search").await? else {
            return Ok(None);
        };
        let country = pure::searched_artist_country(&doc, name);
        self.cache
            .set(key, Cached::Text(country.clone()), 1, Self::ARTIST_TTL);
        Ok(Some(country))
    }

    /// The MusicBrainz artists named exactly `name`, by name or alias, case and accents aside:
    /// empty when there is none, None when MusicBrainz could not be asked. Answers are kept for
    /// the life of the process, so a second preview asks nothing it asked before; a failed
    /// request is not kept, so it is "could not ask", never "no such artist".
    pub async fn find_artists_named(
        &self,
        name: &str,
    ) -> Result<Option<Vec<pure::MusicBrainzArtist>>, MusicBrainzError> {
        let wanted = name.trim();
        if wanted.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if let Some(known) = self.known_artists_named(wanted) {
            return Ok(Some(known));
        }
        let Some(doc) = self
            .get(&pure::artists_named_url(wanted), "artist search")
            .await?
        else {
            return Ok(None);
        };
        let found = pure::parse_artists_named(&doc, wanted);
        let mut artists = self.artists.lock();
        make_room(&mut artists, 1, KNOWN_MAX);
        artists.insert(wanted.to_lowercase(), found.clone());
        Ok(Some(found))
    }

    /// The answer [`Self::find_artists_named`] already has for `name`, without asking.
    pub fn known_artists_named(&self, name: &str) -> Option<Vec<pure::MusicBrainzArtist>> {
        self.artists.lock().get(&name.trim().to_lowercase()).cloned()
    }

    /// The artist credits of these recordings, asked 100 to a search: every id asked gets an
    /// answer, empty when MusicBrainz does not know the recording. None when it could not be
    /// asked. Answers are kept for the life of the process.
    pub async fn recording_credits(
        &self,
        ids: &[String],
    ) -> Result<Option<HashMap<String, Vec<pure::CreditPart>>>, MusicBrainzError> {
        let wanted: Vec<String> = ids
            .iter()
            .map(|id| id.trim().to_lowercase())
            .filter(|id| pure::is_mbid(id) && self.known_credits(id).is_none())
            .collect();
        // Room for every answer this call adds, so none of them is dropped before it is read.
        make_room(&mut self.credits.lock(), wanted.len(), KNOWN_MAX);
        for batch in wanted.chunks(100) {
            let Some(url) = pure::recording_credits_url(batch) else {
                continue;
            };
            let Some(doc) = self.get(&url, "recording credits").await? else {
                return Ok(None);
            };
            let found = pure::parse_recording_credits(&doc);
            let mut credits = self.credits.lock();
            for id in batch {
                credits.insert(id.clone(), found.get(id).cloned().unwrap_or_default());
            }
        }
        let credits = self.credits.lock();
        Ok(Some(
            ids.iter()
                .map(|id| id.trim().to_lowercase())
                .filter_map(|id| credits.get(&id).map(|parts| (id, parts.clone())))
                .collect(),
        ))
    }

    /// The credit [`Self::recording_credits`] already has for one recording, without asking.
    pub fn known_credits(&self, id: &str) -> Option<Vec<pure::CreditPart>> {
        self.credits.lock().get(&id.trim().to_lowercase()).cloned()
    }

    /// One request, spaced at least [`Self::MINIMUM_GAP`] after the last. None on any
    /// failure, which every caller reads as "MusicBrainz had nothing to say"; the timeout is
    /// the one error that reaches the caller.
    async fn get(&self, relative: &str, what: &str) -> Result<Option<Value>, MusicBrainzError> {
        Ok(self.ask(relative, what).await?.found())
    }

    /// One request on the background lane, for work nobody is waiting on (see [`Self::ask_background`]).
    async fn get_background(&self, relative: &str, what: &str) -> Result<Option<Value>, MusicBrainzError> {
        Ok(self.ask_background(relative, what).await?.found())
    }

    /// [`Self::ask`] or [`Self::ask_background`].
    async fn ask_on(&self, relative: &str, what: &str, background: bool) -> Result<Asked, MusicBrainzError> {
        if background {
            self.ask_background(relative, what).await
        } else {
            self.ask(relative, what).await
        }
    }

    /// [`Self::get`], telling apart how MusicBrainz answered.
    async fn ask(&self, relative: &str, what: &str) -> Result<Asked, MusicBrainzError> {
        let mut last_call = self.gate.lock().await;
        if let Some(last) = *last_call {
            let due = last.at + Self::MINIMUM_GAP;
            if due > Instant::now() {
                tokio::time::sleep_until(due).await;
            }
        }
        *last_call = Some(LastCall {
            at: Instant::now(),
            background: false,
        });
        self.send(relative, what).await
    }

    /// One request on the background lane, for work nobody is waiting on. It never queues on
    /// the gate: while the gate is held, or a call that is not its own went out less than
    /// [`Self::MINIMUM_GAP`] ago, it steps back for [`Self::BACKGROUND_BACKOFF`] and tries
    /// again, so a download being verified goes first. It takes its turn and lets go of the gate
    /// before it sends, so a caller arriving meanwhile waits one gap at most.
    async fn ask_background(&self, relative: &str, what: &str) -> Result<Asked, MusicBrainzError> {
        loop {
            let wait = match self.gate.try_lock() {
                Err(_) => self.background_backoff,
                Ok(mut last_call) => {
                    let now = Instant::now();
                    match *last_call {
                        Some(last) if last.at + Self::MINIMUM_GAP > now => {
                            if last.background {
                                last.at + Self::MINIMUM_GAP - now
                            } else {
                                self.background_backoff
                            }
                        }
                        _ => {
                            *last_call = Some(LastCall {
                                at: now,
                                background: true,
                            });
                            break;
                        }
                    }
                }
            };
            tokio::time::sleep(wait).await;
        }
        self.send(relative, what).await
    }

    /// The request itself, once its turn has come, and how MusicBrainz answered it. A refused
    /// question is one MusicBrainz does not take as Octo asked it, so it is logged as a warning
    /// naming the question.
    async fn send(&self, relative: &str, what: &str) -> Result<Asked, MusicBrainzError> {
        let attempt = async {
            let url = resolve(&self.base, relative)?;
            let response = self.client.get(url).send().await?;
            let answer = HttpAnswer::read(response).await?;
            if answer.is_success() {
                return anyhow::Ok(Asked::Answer(parse_json(&answer.body)?));
            }
            let status = answer.status.as_u16();
            let said: String = String::from_utf8_lossy(&answer.body).chars().take(300).collect();
            Ok(match status {
                404 => Asked::NotKnown,
                // An id that is not a MusicBrainz id, such as a junk tag: nothing is known by it.
                400 if said.contains("Invalid mbid") || asks_by_a_non_mbid(relative) => Asked::NotKnown,
                429 => Asked::Busy,
                400..=499 => {
                    warn!("MusicBrainz refused the {what} question {relative} ({status}): {said}");
                    Asked::Refused
                }
                _ => {
                    debug!("musicbrainz {what} answered {status}");
                    Asked::Busy
                }
            })
        };
        match attempt.await {
            Ok(asked) => Ok(asked),
            Err(e)
                if e.downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_timeout) =>
            {
                Err(MusicBrainzError::TimedOut)
            }
            Err(e) => {
                debug!("musicbrainz {what} failed: {e}");
                Ok(Asked::Busy)
            }
        }
    }
}

/// Whether a question looks something up by an id that is not a MusicBrainz id
/// (`release/<id>`, `recording/<id>`, `artist/<id>`, ...): MusicBrainz has nothing by it.
fn asks_by_a_non_mbid(relative: &str) -> bool {
    let path = relative.split('?').next().unwrap_or("");
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(resource), Some(id), None)
            if !id.is_empty()
                && matches!(
                    resource,
                    "release" | "recording" | "artist" | "release-group" | "label" | "work"
                ) =>
        {
            !pure::is_mbid(&dotnet::unescape_data_string(id))
        }
        _ => false,
    }
}

/// The recordings of an answer that `keep` lets through, without DJ-mix segments, each with at
/// most [`MOST_RELEASES_PER_RECORDING`] releases.
fn recordings_of(doc: &Value, keep: impl Fn(&Value) -> bool) -> Vec<Value> {
    doc.get("recordings")
        .and_then(Value::as_array)
        .map(|recordings| {
            recordings
                .iter()
                .filter(|recording| keep(recording) && !is_dj_mix_segment(recording))
                .map(|recording| {
                    let mut recording = recording.clone();
                    if let Some(releases) = recording.get_mut("releases").and_then(Value::as_array_mut) {
                        releases.truncate(MOST_RELEASES_PER_RECORDING);
                    }
                    recording
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether one recording carries the code.
fn carries(recording: &Value, code: &str) -> bool {
    recording
        .get("isrcs")
        .and_then(Value::as_array)
        .is_some_and(|codes| {
            codes
                .iter()
                .filter_map(Value::as_str)
                .any(|carried| carried.eq_ignore_ascii_case(code))
        })
}

/// A recording MusicBrainz keeps as one segment of a DJ mix ("part of … DJ-mix", however the
/// hyphen or the space is written): the song inside someone's mix, never the song itself.
fn is_dj_mix_segment(recording: &Value) -> bool {
    recording
        .get("disambiguation")
        .and_then(Value::as_str)
        .is_some_and(|said| {
            let squeezed: String = said
                .to_lowercase()
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '-' && !('\u{2010}'..='\u{2015}').contains(c))
                .collect();
            squeezed.contains("djmix")
        })
}

/// When the last request went out, and whether the background lane sent it.
#[derive(Debug, Clone, Copy)]
struct LastCall {
    at: Instant,
    background: bool,
}

/// The music database calls the release identifier makes. A timeout is the error the
/// identifier sees; every other failure is already None.
#[async_trait::async_trait]
impl ReleaseLookup for MusicBrainzClient {
    async fn lookup_release(&self, release_id: &str) -> anyhow::Result<Option<ReleaseDetails>> {
        Ok(MusicBrainzClient::lookup_release(self, release_id)
            .await?
            .map(|details| (*details).clone()))
    }

    async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> anyhow::Result<Option<Value>> {
        Ok(MusicBrainzClient::search_recordings(self, artist, title, duration_seconds).await?)
    }

    async fn lookup_isrc(&self, isrc: &str) -> anyhow::Result<Option<Value>> {
        match MusicBrainzClient::lookup_isrc(self, isrc).await? {
            IsrcLookup::Found(doc) => Ok(Some(doc)),
            IsrcLookup::NotKnown => Ok(None),
            IsrcLookup::Unanswered => anyhow::bail!("MusicBrainz could not be asked about the code"),
            IsrcLookup::Refused => anyhow::bail!("MusicBrainz refused the question about the code"),
        }
    }
}

impl Default for MusicBrainzClient {
    fn default() -> Self {
        Self::new()
    }
}

/// The most artist searches, and the most recording credits, kept in memory.
const KNOWN_MAX: usize = 10_000;

/// Drops every kept answer when `adding` more would pass `max`: answers MusicBrainz gives again,
/// so the map stays bounded however long the process runs.
fn make_room<K, V>(map: &mut HashMap<K, V>, adding: usize, max: usize) {
    if map.len() + adding > max {
        map.clear();
    }
}

#[cfg(test)]
#[path = "music_brainz_client_tests.rs"]
mod tests;

#[async_trait::async_trait]
impl crate::services::metadata::deezer_metadata_service::IsrcSource for MusicBrainzClient {
    async fn isrcs_for(&self, artist: &str, title: &str, seconds: Option<i32>) -> Vec<String> {
        match MusicBrainzClient::isrcs_for(self, artist, title, seconds).await {
            Ok(codes) => codes,
            Err(e) => {
                tracing::debug!("MusicBrainz could not name ISRCs for '{artist} - {title}': {e}");
                Vec::new()
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::services::cover_art::BarcodeSource for MusicBrainzClient {
    async fn barcodes_for(&self, artist: &str, album: &str) -> Vec<String> {
        match self.album_barcodes(artist, album).await {
            Ok(codes) => codes,
            Err(e) => {
                tracing::debug!("MusicBrainz could not name barcodes for '{artist} - {album}': {e}");
                Vec::new()
            }
        }
    }
}
