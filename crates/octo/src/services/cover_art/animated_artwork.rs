//! Apple Music's animated album covers, through a server of m8tec/apple-music-animated-artworks
//! the owner runs (`Metadata:AnimatedArtworkUrl`).
//!
//! That server reads Apple Music's pages and answers an album, by artist and album name, with
//! the HLS streams (`.m3u8`) of its animated cover: a square one and a tall one, the full-screen
//! kind. It keeps its own cache, so Apple is asked at most once per album. Octo asks it at most
//! once per album too, and keeps every answer in `animated-artwork.json`, a "no animated cover"
//! included, so an album the apps open again costs nothing.
//!
//! The server matches names loosely (a search for an album may return its Deluxe edition, and
//! two-way substring matching may return another record), so an answer whose artist does not
//! agree with the one asked for, or whose album is another title once edition words are set
//! aside, is taken as "none".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet::escape_data_string;
use octo_core::common::song_identity::{ArtistAgreement, SongIdentity};
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::services::framework::InFlight;
use crate::services::library::AlbumOwnership;
use crate::services::state_file;

/// One album's animated cover: the square stream, the tall one, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnimatedCover {
    /// The square cover's HLS playlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The tall cover's HLS playlist (lock screen, expanded player).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_tall: Option<String>,
}

/// What a lookup found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AnimatedLookup {
    Found(AnimatedCover),
    /// The album has no animated cover (or none that is this album's).
    None,
    /// Not set up, or the server did not answer this time; nothing is remembered.
    #[default]
    Unavailable,
}

/// An answer as `animated-artwork.json` keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Known {
    url: Option<String>,
    url_tall: Option<String>,
    /// When it was asked, in seconds since 1970.
    checked: i64,
    /// The cover's key under `/animated-artwork/`, where Octo relays its streams: random, so
    /// only an app Octo told of the cover can ask for it.
    #[serde(default)]
    relay: Option<String>,
}

impl Known {
    fn cover(&self) -> Option<AnimatedCover> {
        (self.url.is_some() || self.url_tall.is_some()).then(|| AnimatedCover {
            url: self.url.clone(),
            url_tall: self.url_tall.clone(),
        })
    }
}

/// The server's answer for a found album.
#[derive(Debug, Deserialize)]
struct Answer {
    url: Option<String>,
    url_tall: Option<String>,
    artist: Option<String>,
    album: Option<String>,
}

pub struct AnimatedArtwork {
    settings: Arc<SettingsStore>,
    client: reqwest::Client,
    /// For the relay: no overall timeout (a video file is read as the player reads it).
    relay_client: reqwest::Client,
    /// Tests only: the origin the relay may fetch from in place of Apple's video host.
    relay_test_origin: Option<String>,
    path: PathBuf,
    known: Mutex<HashMap<String, Known>>,
    flights: InFlight<AnimatedLookup>,
}

impl AnimatedArtwork {
    /// How long one question to the server may take: it may have to read several of Apple's
    /// pages for an album it has not seen.
    pub const TIMEOUT: Duration = Duration::from_secs(15);
    /// How long a found cover is kept before it is asked again.
    pub const FOUND_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    /// How long "no animated cover" is kept: Apple adds them to albums now and then.
    pub const NONE_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    /// The most albums kept; past it, the ones asked longest ago go.
    pub const MAX_KNOWN: usize = 20_000;

    pub fn new(settings: Arc<SettingsStore>, path: PathBuf) -> Self {
        let known = match state_file::read_text(&path) {
            Ok(Some(text)) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(
                    "{} could not be read ({e}); animated covers are asked again",
                    path.display()
                );
                HashMap::new()
            }),
            Ok(None) => HashMap::new(),
            Err(e) => {
                warn!(
                    "{} could not be read ({e}); animated covers are asked again",
                    path.display()
                );
                HashMap::new()
            }
        };
        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .expect("a plain HTTP client builds");
        let relay_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .build()
            .expect("a plain HTTP client builds");
        Self {
            settings,
            client,
            relay_client,
            relay_test_origin: None,
            path,
            known: Mutex::new(known),
            flights: InFlight::new(),
        }
    }

    /// The client the relay asks Apple's video host with.
    pub fn relay_client(&self) -> &reqwest::Client {
        &self.relay_client
    }

    /// Tests only: the relay fetches from `origin` (a mock server) instead of Apple.
    pub fn relay_test_origin(&self) -> Option<&str> {
        self.relay_test_origin.as_deref()
    }

    #[cfg(test)]
    pub fn with_relay_test_origin(mut self, origin: &str) -> Self {
        self.relay_test_origin = Some(origin.trim_end_matches('/').to_string());
        self
    }

    /// Tests only: a found cover, kept as if the server had answered it. Its relay key.
    #[cfg(test)]
    pub fn seed(&self, artist: &str, album: &str, url: Option<&str>, url_tall: Option<&str>) -> String {
        let key = Self::key(artist, album).expect("an artist and an album");
        let relay = Self::new_relay_key();
        self.known.lock().insert(
            key,
            Known {
                url: url.map(str::to_string),
                url_tall: url_tall.map(str::to_string),
                checked: Self::now(),
                relay: Some(relay.clone()),
            },
        );
        relay
    }

    /// The server's address, when one is set.
    fn base(&self) -> Option<String> {
        self.settings.current().metadata.animated_artwork_base()
    }

    /// Whether a server is set, so the apps may be told of animated covers.
    pub fn enabled(&self) -> bool {
        self.base().is_some()
    }

    fn key(artist: &str, album: &str) -> Option<String> {
        let (artist, album) = (SongIdentity::key(artist), SongIdentity::key(album));
        (!artist.is_empty() && !album.is_empty()).then(|| format!("{artist}|{album}"))
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// What is already known of the album, without asking: `None` when it has to be asked.
    pub fn known(&self, artist: &str, album: &str) -> Option<AnimatedLookup> {
        if !self.enabled() {
            return Some(AnimatedLookup::Unavailable);
        }
        let key = Self::key(artist, album)?;
        let known = self.known.lock().get(&key).cloned()?;
        let age = Self::now().saturating_sub(known.checked).max(0) as u64;
        Some(match known.cover() {
            Some(cover) if age < Self::FOUND_FOR.as_secs() => AnimatedLookup::Found(cover),
            None if age < Self::NONE_FOR.as_secs() => AnimatedLookup::None,
            _ => return None,
        })
    }

    /// The album's animated cover, asking the server when it is not known. One question per
    /// album at a time, and it finishes (and is kept) even when nobody waits for it any more.
    pub async fn find(self: &Arc<Self>, artist: &str, album: &str) -> AnimatedLookup {
        if let Some(known) = self.known(artist, album) {
            return known;
        }
        let Some(key) = Self::key(artist, album) else {
            return AnimatedLookup::None;
        };
        let (this, artist, album) = (Arc::clone(self), artist.to_string(), album.to_string());
        self.flights
            .run(&key, async move { this.ask(&artist, &album).await })
            .await
    }

    /// [`Self::find`], waiting at most `wait`; what comes back later is kept for the next time.
    pub async fn find_within(self: &Arc<Self>, artist: &str, album: &str, wait: Duration) -> AnimatedLookup {
        tokio::time::timeout(wait, self.find(artist, album))
            .await
            .unwrap_or(AnimatedLookup::Unavailable)
    }

    async fn ask(&self, artist: &str, album: &str) -> AnimatedLookup {
        let Some(base) = self.base() else {
            return AnimatedLookup::Unavailable;
        };
        let url = format!(
            "{base}/api/v1/artwork/search?artist={}&album={}",
            escape_data_string(artist),
            escape_data_string(album)
        );
        let response = match self.client.get(&url).send().await {
            Ok(response) => response,
            Err(e) => {
                debug!("Animated artwork server did not answer for '{artist} - {album}': {e}");
                return AnimatedLookup::Unavailable;
            }
        };
        let status = response.status();
        let found = if status == reqwest::StatusCode::NOT_FOUND {
            AnimatedLookup::None
        } else if status.is_success() {
            match response.json::<Answer>().await {
                Ok(answer) => Self::judge(artist, album, answer),
                Err(e) => {
                    debug!("Animated artwork answer for '{artist} - {album}' could not be read: {e}");
                    return AnimatedLookup::Unavailable;
                }
            }
        } else {
            // Starting up (503), slowed down by Apple or by its own limit (429), or failing.
            debug!("Animated artwork server answered {status} for '{artist} - {album}'");
            return AnimatedLookup::Unavailable;
        };
        if let AnimatedLookup::Found(_) = &found {
            info!("Animated cover found for '{artist} - {album}'");
        }
        self.remember(artist, album, &found);
        found
    }

    /// The server's answer as this album's, when it is this album: the same artist, and the same
    /// title once edition words ("Deluxe Edition", "Remastered") are set aside. Only https
    /// streams are passed on.
    fn judge(artist: &str, album: &str, answer: Answer) -> AnimatedLookup {
        let artist_agrees = answer.artist.as_deref().is_none_or(|got| {
            matches!(
                SongIdentity::compare_artists(artist, got),
                ArtistAgreement::Agree | ArtistAgreement::Loose | ArtistAgreement::Conflict
            )
        });
        let album_agrees = answer.album.as_deref().is_none_or(|got| {
            SongIdentity::key(got) == SongIdentity::key(album)
                || AlbumOwnership::base_key(Some(got)) == AlbumOwnership::base_key(Some(album))
        });
        if !artist_agrees || !album_agrees {
            debug!(
                "Animated artwork server answered '{} - {}' for '{artist} - {album}'; taken as none",
                answer.artist.as_deref().unwrap_or(""),
                answer.album.as_deref().unwrap_or("")
            );
            return AnimatedLookup::None;
        }
        let secure = |url: Option<String>| {
            url.filter(|u| reqwest::Url::parse(u).is_ok_and(|parsed| parsed.scheme() == "https"))
        };
        let cover = AnimatedCover {
            url: secure(answer.url),
            url_tall: secure(answer.url_tall),
        };
        if cover.url.is_none() && cover.url_tall.is_none() {
            AnimatedLookup::None
        } else {
            AnimatedLookup::Found(cover)
        }
    }

    fn new_relay_key() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    /// The relay key of the album's found cover (one is made for a cover kept before keys
    /// were), or `None` when no cover is known for it.
    pub fn relay_key(&self, artist: &str, album: &str) -> Option<String> {
        let key = Self::key(artist, album)?;
        let (relay, text) = {
            let mut known = self.known.lock();
            let entry = known.get_mut(&key).filter(|k| k.cover().is_some())?;
            if let Some(relay) = &entry.relay {
                return Some(relay.clone());
            }
            let relay = Self::new_relay_key();
            entry.relay = Some(relay.clone());
            (relay, serde_json::to_string(&*known).unwrap_or_default())
        };
        if let Err(e) = state_file::save_atomic(&self.path, &text) {
            warn!("{} could not be saved: {e}", self.path.display());
        }
        Some(relay)
    }

    /// The cover a relay key stands for: its square and tall playlists. `None` for a key
    /// Octo never handed out.
    pub fn relayed_cover(&self, relay: &str) -> Option<AnimatedCover> {
        if relay.is_empty() || !self.enabled() {
            return None;
        }
        self.known
            .lock()
            .values()
            .find(|k| k.relay.as_deref() == Some(relay))
            .and_then(Known::cover)
    }

    fn remember(&self, artist: &str, album: &str, found: &AnimatedLookup) {
        let Some(key) = Self::key(artist, album) else {
            return;
        };
        let entry = match found {
            AnimatedLookup::Found(cover) => Known {
                url: cover.url.clone(),
                url_tall: cover.url_tall.clone(),
                checked: Self::now(),
                relay: Some(Self::new_relay_key()),
            },
            AnimatedLookup::None => Known {
                url: None,
                url_tall: None,
                checked: Self::now(),
                relay: None,
            },
            AnimatedLookup::Unavailable => return,
        };
        let text = {
            let mut known = self.known.lock();
            // A cover found again keeps its relay key, so links an app already holds still work.
            let mut entry = entry;
            if entry.relay.is_some()
                && let Some(previous) = known.get(&key).and_then(|k| k.relay.clone())
            {
                entry.relay = Some(previous);
            }
            known.insert(key, entry);
            if known.len() > Self::MAX_KNOWN {
                let mut by_age: Vec<(String, i64)> =
                    known.iter().map(|(k, v)| (k.clone(), v.checked)).collect();
                by_age.sort_by_key(|(_, checked)| *checked);
                let over = known.len() - Self::MAX_KNOWN;
                for (key, _) in by_age.into_iter().take(over) {
                    known.remove(&key);
                }
            }
            serde_json::to_string(&*known).unwrap_or_default()
        };
        if let Err(e) = state_file::save_atomic(&self.path, &text) {
            warn!("{} could not be saved: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
#[path = "animated_artwork_tests.rs"]
mod tests;
