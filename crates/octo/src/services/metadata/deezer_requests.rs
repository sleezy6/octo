//! How many requests Octo makes of Deezer's public API, and for what: what the dashboard's
//! Deezer page shows and what an hourly line in the log says, so the quota's spending can be
//! seen (and what a change saves, measured).
//!
//! The purpose comes from the code that asked: [`DeezerUse::scope`] marks a piece of work, and
//! every metered request made inside it counts for that purpose (a task it spawns is marked
//! again with [`DeezerUse::carry`]). A request made outside any mark counts by what it asked
//! for: an artist's page, an album's, a song looked up, or something else.

use std::future::Future;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tracing::info;

/// What a request to Deezer was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeezerUse {
    /// A search someone typed, and the work its answer finishes afterwards.
    Search,
    /// A song's length looked up for display.
    Lengths,
    /// An artist's page: the artist, their top songs, albums and similar artists.
    ArtistPages,
    /// An album's page and tracklist.
    AlbumPages,
    /// A cover or artist picture looked up.
    Covers,
    /// Finding the song a play, a heart or Find songs fetches from Deezer.
    Playback,
    /// A song's facts looked up by artist and title, outside any of the above.
    SongLookups,
    /// Charts, genres and anything else.
    Other,
}

impl DeezerUse {
    pub const ALL: [DeezerUse; 8] = [
        DeezerUse::Search,
        DeezerUse::Lengths,
        DeezerUse::ArtistPages,
        DeezerUse::AlbumPages,
        DeezerUse::Covers,
        DeezerUse::Playback,
        DeezerUse::SongLookups,
        DeezerUse::Other,
    ];

    /// Words for the log.
    pub fn words(self) -> &'static str {
        match self {
            DeezerUse::Search => "search",
            DeezerUse::Lengths => "lengths",
            DeezerUse::ArtistPages => "artist pages",
            DeezerUse::AlbumPages => "album pages",
            DeezerUse::Covers => "covers",
            DeezerUse::Playback => "playback",
            DeezerUse::SongLookups => "song lookups",
            DeezerUse::Other => "other",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|u| *u == self)
            .unwrap_or(Self::ALL.len() - 1)
    }

    /// Runs `work` with every Deezer request inside it counted for this purpose. A mark made
    /// inside it wins for its own part.
    pub async fn scope<F: Future>(self, work: F) -> F::Output {
        CURRENT.scope(self, work).await
    }

    /// `work`, marked with the purpose of the work that makes it: for a task about to be
    /// spawned, which does not inherit the mark by itself.
    pub fn carry<F: Future>(work: F) -> impl Future<Output = F::Output> {
        let current = Self::current();
        async move {
            match current {
                Some(purpose) => purpose.scope(work).await,
                None => work.await,
            }
        }
    }

    /// The mark on the work running now, if any.
    pub fn current() -> Option<DeezerUse> {
        CURRENT.try_with(|purpose| *purpose).ok()
    }

    /// The purpose of an unmarked request, from its path.
    pub fn of_path(path: &str) -> DeezerUse {
        let path = path.trim_start_matches('/');
        let first = path.split('/').next().unwrap_or("");
        match first {
            "artist" => DeezerUse::ArtistPages,
            "album" => DeezerUse::AlbumPages,
            "track" => DeezerUse::SongLookups,
            "search" => match path.split('/').nth(1) {
                Some("artist") => DeezerUse::ArtistPages,
                Some("album") => DeezerUse::AlbumPages,
                _ => DeezerUse::SongLookups,
            },
            _ => DeezerUse::Other,
        }
    }
}

tokio::task_local! {
    static CURRENT: DeezerUse;
}

const PURPOSES: usize = DeezerUse::ALL.len();
const MINUTES_KEPT: usize = 60;
const LOG_EVERY: Duration = Duration::from_secs(60 * 60);

/// One minute's requests by purpose and lane.
#[derive(Debug, Clone, Copy, Default)]
struct Minute {
    /// Minutes since the counter began; tells a stale slot of the ring from this hour's.
    at: u64,
    interactive: [u64; PURPOSES],
    background: [u64; PURPOSES],
    turned_away: u64,
    searches_skipped: u64,
}

/// Every count since Octo started, and the last hour's by minute.
#[derive(Debug)]
pub struct DeezerRequestCounter {
    started: Instant,
    state: Mutex<CounterState>,
}

#[derive(Debug)]
struct CounterState {
    total: Minute,
    ring: [Minute; MINUTES_KEPT],
    last_logged: Instant,
}

/// One purpose's count, as the dashboard reads it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UseCount {
    #[serde(rename = "use")]
    pub purpose: DeezerUse,
    pub interactive: u64,
    pub background: u64,
}

/// The counts for one stretch of time.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RequestCounts {
    pub total: u64,
    pub uses: Vec<UseCount>,
    /// Requests the rate limiter turned away because its queue was full.
    pub turned_away: u64,
    /// Plays and hearts that went straight to a Deezer id kept on the song, with no search.
    pub searches_skipped: u64,
}

/// What `GET /api/admin/deezer/requests` answers.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RequestReport {
    pub last_hour: RequestCounts,
    pub since_start: RequestCounts,
    pub minutes_since_start: u64,
}

impl Default for DeezerRequestCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl DeezerRequestCounter {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            state: Mutex::new(CounterState {
                total: Minute::default(),
                ring: [Minute::default(); MINUTES_KEPT],
                last_logged: now,
            }),
        }
    }

    /// The one counter every Deezer client adds to.
    pub fn global() -> &'static DeezerRequestCounter {
        static GLOBAL: LazyLock<DeezerRequestCounter> = LazyLock::new(DeezerRequestCounter::new);
        &GLOBAL
    }

    fn minute_now(&self) -> u64 {
        self.started.elapsed().as_secs() / 60
    }

    fn with_minute(&self, change: impl Fn(&mut Minute)) {
        let at = self.minute_now();
        let report = {
            let mut state = self.state.lock();
            change(&mut state.total);
            let slot = &mut state.ring[usize::try_from(at).unwrap_or(0) % MINUTES_KEPT];
            if slot.at != at {
                *slot = Minute {
                    at,
                    ..Minute::default()
                };
            }
            change(slot);
            (state.last_logged.elapsed() >= LOG_EVERY).then(|| {
                state.last_logged = Instant::now();
                hour_of(&state.ring, at)
            })
        };
        if let Some(hour) = report {
            info!("{}", log_line(&counts_of(&hour)));
        }
    }

    /// One request sent to Deezer's API for `purpose`, on the background lane or not.
    pub fn count(&self, purpose: DeezerUse, background: bool) {
        let i = purpose.index();
        self.with_minute(|m| {
            if background {
                m.background[i] += 1;
            } else {
                m.interactive[i] += 1;
            }
        });
    }

    /// One request the rate limiter turned away.
    pub fn turned_away(&self) {
        self.with_minute(|m| m.turned_away += 1);
    }

    /// One play or heart that used a kept Deezer id instead of searching.
    pub fn search_skipped(&self) {
        self.with_minute(|m| m.searches_skipped += 1);
    }

    pub fn report(&self) -> RequestReport {
        let at = self.minute_now();
        let state = self.state.lock();
        RequestReport {
            last_hour: counts_of(&hour_of(&state.ring, at)),
            since_start: counts_of(&state.total),
            minutes_since_start: at,
        }
    }
}

/// The sum of the ring's minutes within the hour up to `at`. A slot not written since Octo
/// started reads as minute 0 and holds nothing, so it adds nothing.
fn hour_of(ring: &[Minute; MINUTES_KEPT], at: u64) -> Minute {
    let mut sum = Minute::default();
    for minute in ring
        .iter()
        .filter(|m| m.at + (MINUTES_KEPT as u64) > at && m.at <= at)
    {
        for i in 0..PURPOSES {
            sum.interactive[i] += minute.interactive[i];
            sum.background[i] += minute.background[i];
        }
        sum.turned_away += minute.turned_away;
        sum.searches_skipped += minute.searches_skipped;
    }
    sum
}

fn counts_of(minute: &Minute) -> RequestCounts {
    let uses: Vec<UseCount> = DeezerUse::ALL
        .iter()
        .map(|purpose| UseCount {
            purpose: *purpose,
            interactive: minute.interactive[purpose.index()],
            background: minute.background[purpose.index()],
        })
        .collect();
    RequestCounts {
        total: uses.iter().map(|u| u.interactive + u.background).sum(),
        uses,
        turned_away: minute.turned_away,
        searches_skipped: minute.searches_skipped,
    }
}

/// "Deezer requests in the last hour: 412 (search 120, lengths 250, ...)".
pub fn log_line(counts: &RequestCounts) -> String {
    let parts: Vec<String> = counts
        .uses
        .iter()
        .filter(|u| u.interactive + u.background > 0)
        .map(|u| {
            let n = u.interactive + u.background;
            if u.background > 0 {
                format!("{} {n} ({} in the background)", u.purpose.words(), u.background)
            } else {
                format!("{} {n}", u.purpose.words())
            }
        })
        .collect();
    let mut line = format!("Deezer requests in the last hour: {}", counts.total);
    if !parts.is_empty() {
        line.push_str(&format!(" ({})", parts.join(", ")));
    }
    if counts.turned_away > 0 {
        line.push_str(&format!("; {} turned away by the rate limit", counts.turned_away));
    }
    if counts.searches_skipped > 0 {
        line.push_str(&format!(
            "; {} plays and hearts needed no search",
            counts.searches_skipped
        ));
    }
    line
}

#[cfg(test)]
#[path = "deezer_requests_tests.rs"]
mod tests;
