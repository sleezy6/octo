//! A listener's favorites as Last.fm loves. A song favorited in any app is loved on the
//! listener's Last.fm and a favorite taken back is unloved, through the same link scrobbling
//! uses. The favorites a listener already had are loved once, when the link is first used after
//! being made, and remembered in `lastfm-loves.json` by session so a restart does not love them
//! all again (which would also undo loves the listener took back on last.fm since).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet;
use octo_core::last_fm::{LastFmLoves, LastFmSentLove};
use parking_lot::Mutex;
use serde_json::Value;
use tracing::{debug, info, warn};

use super::LastFmScrobbleService;

/// What one love or unlove came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoveSent {
    /// Last.fm took it.
    Done,
    /// Last.fm refused this one song; the rest may still go.
    Skipped,
    /// Worth sending again later: unreachable, busy, or Octo is calling too often.
    Retry,
    /// The listener's link is gone or refused: nothing more goes for them.
    Unlinked,
}

/// What the service keeps for loves.
#[derive(Default)]
pub(super) struct Loves {
    /// Where the sessions whose favorites were loved are kept; unset in tests that keep none.
    file: OnceLock<PathBuf>,
    /// Listener (lower case) to the session key their favorites were loved with.
    pub(super) synced: Mutex<HashMap<String, String>>,
    /// Listeners whose favorites are being loved right now.
    pub(super) syncing: Mutex<HashSet<String>>,
    /// Listener (lower case) to the latest love or unlove Last.fm took for them.
    pub(super) last: Mutex<HashMap<String, LastFmSentLove>>,
}

impl LastFmScrobbleService {
    /// How many times one love is sent before it is given up on.
    const LOVE_ATTEMPTS: u32 = 3;
    /// The wait before a love is sent again, times the attempt.
    const LOVE_RETRY: Duration = Duration::from_secs(30);
    /// Between the loves of a listener's existing favorites: Last.fm asks for no more than five
    /// calls a second.
    const LOVE_SPACING: Duration = Duration::from_millis(300);

    /// Keeps the sessions whose favorites were loved in `path`, read now.
    pub fn set_loves_file(&self, path: PathBuf) {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(saved) = serde_json::from_str::<HashMap<String, String>>(&text)
        {
            let mut synced = self.loves.synced.lock();
            for (user, key) in saved {
                synced.insert(user.trim().to_lowercase(), key);
            }
        }
        let _ = self.loves.file.set(path);
    }

    /// True when this listener's favorites would be loved on Last.fm: loves are on (the
    /// dashboard's "Love favorites on Last.fm"), the API key and secret are saved and their link
    /// is usable. Scrobbling need not be on.
    pub fn loves_for(&self, username: &str) -> bool {
        let settings = self.last_fm();
        settings.love_favorites
            && super::is_ready_with(&settings)
            && self.active_session(&settings, username).is_some()
    }

    /// How this listener's favorites stand as loves, for the dashboard.
    pub(super) fn loves_of(&self, username: &str, session_key: &str) -> LastFmLoves {
        let lower = username.trim().to_lowercase();
        LastFmLoves {
            favorites_loved: self.loves.synced.lock().get(&lower).map(String::as_str) == Some(session_key),
            loving: self.loves.syncing.lock().contains(&lower),
            last: self.loves.last.lock().get(&lower).cloned(),
        }
    }

    fn remember_love(&self, user: &str, artist: &str, title: &str, loved: bool, at_utc: DateTime<Utc>) {
        self.loves.last.lock().insert(
            user.trim().to_lowercase(),
            LastFmSentLove {
                artist: artist.trim().to_string(),
                title: title.trim().to_string(),
                loved,
                at_utc,
            },
        );
    }

    /// Loves (or with `loved` false, unloves) one song on the listener's Last.fm, in the
    /// background, sent again a couple of times when Last.fm cannot take it right then.
    pub fn love(self: &Arc<Self>, username: &str, artist: &str, title: &str, loved: bool) {
        if dotnet::is_blank(artist) || dotnet::is_blank(title) || !self.loves_for(username) {
            return;
        }
        let service = Arc::clone(self);
        let (user, artist, title) = (username.trim().to_string(), artist.to_string(), title.to_string());
        tokio::spawn(async move {
            let sent = service.love_with_retries(&user, &artist, &title, loved).await;
            if sent != LoveSent::Done {
                debug!(
                    "Last.fm {} of '{artist} - {title}' for {user} did not go: {sent:?}",
                    if loved { "love" } else { "unlove" }
                );
            }
        });
    }

    /// Loves every song in `favorites` once for this listener, the first time their link is used
    /// after it was made. Nothing when they were already loved with this session, a pass is
    /// running, or the link is not usable; when `favorites` cannot be read, a later call tries
    /// again.
    pub fn love_favorites_once<F>(self: &Arc<Self>, username: &str, favorites: F)
    where
        F: Future<Output = Option<Vec<(String, String)>>> + Send + 'static,
    {
        if !self.favorites_unloved(username) {
            return;
        }
        let user = username.trim().to_string();
        let lower = user.to_lowercase();
        if !self.loves.syncing.lock().insert(lower.clone()) {
            return;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let key = service
                .active_session(&service.last_fm(), &user)
                .map(|session| session.session_key);
            let finished = match (key, favorites.await) {
                (Some(key), Some(songs)) => service.love_all(&user, &songs).await.then_some(key),
                (_, None) => {
                    debug!("{user}'s favorites could not be read to love on Last.fm; trying again later");
                    None
                }
                _ => None,
            };
            if let Some(key) = finished {
                service.loves.synced.lock().insert(lower.clone(), key);
                service.save_loves();
            }
            service.loves.syncing.lock().remove(&lower);
        });
    }

    /// True when this listener has a usable link whose favorites were not yet loved: cheap, so
    /// a caller can ask before reading the favorites.
    pub fn favorites_unloved(&self, username: &str) -> bool {
        let settings = self.last_fm();
        if !settings.love_favorites || !super::is_ready_with(&settings) {
            return false;
        }
        let Some(session) = self.active_session(&settings, username) else {
            return false;
        };
        let lower = username.trim().to_lowercase();
        self.loves.synced.lock().get(&lower) != Some(&session.session_key)
            && !self.loves.syncing.lock().contains(&lower)
    }

    /// Loves each song, spaced out. False when the pass stopped before the end.
    async fn love_all(&self, user: &str, songs: &[(String, String)]) -> bool {
        let mut loved = 0usize;
        for (artist, title) in songs {
            if dotnet::is_blank(artist) || dotnet::is_blank(title) {
                continue;
            }
            match self.love_with_retries(user, artist, title, true).await {
                LoveSent::Done => loved += 1,
                LoveSent::Skipped => {}
                LoveSent::Retry | LoveSent::Unlinked => {
                    warn!("Loving {user}'s favorites on Last.fm stopped after {loved}; it starts over later");
                    return false;
                }
            }
            (self.time.sleep)(Self::LOVE_SPACING).await;
        }
        info!("Loved {loved} of {user}'s favorites on Last.fm");
        true
    }

    async fn love_with_retries(&self, user: &str, artist: &str, title: &str, loved: bool) -> LoveSent {
        let mut sent = LoveSent::Retry;
        for attempt in 1..=Self::LOVE_ATTEMPTS {
            sent = self.send_love(user, artist, title, loved).await;
            if sent != LoveSent::Retry || attempt == Self::LOVE_ATTEMPTS {
                break;
            }
            (self.time.sleep)(Self::LOVE_RETRY * attempt).await;
        }
        sent
    }

    /// One `track.love` or `track.unlove`.
    pub(super) async fn send_love(&self, user: &str, artist: &str, title: &str, loved: bool) -> LoveSent {
        let settings = self.last_fm();
        let Some(session) = self
            .active_session(&settings, user)
            .filter(|_| settings.love_favorites)
        else {
            return LoveSent::Unlinked;
        };
        if self.now() < self.gate.lock().paused_until {
            return LoveSent::Retry;
        }
        let method = if loved { "track.love" } else { "track.unlove" };
        let reply = self
            .call(
                vec![
                    ("method", method.to_string()),
                    ("api_key", settings.api_key.trim().to_string()),
                    ("sk", session.session_key.trim().to_string()),
                    ("artist", artist.trim().to_string()),
                    ("track", title.trim().to_string()),
                ],
                settings.api_secret.trim(),
            )
            .await;
        if reply.ok() {
            self.accepted(user, &session.session_key);
            let now = self.now();
            self.remember_love(user, artist, title, loved, now);
            return LoveSent::Done;
        }
        if reply.error == Self::ERROR_INVALID_SESSION {
            self.mark_disconnected(user, &session.session_key, &reply.message);
            return LoveSent::Unlinked;
        }
        if reply.error == Self::ERROR_RATE_LIMITED {
            let now = self.now();
            self.gate.lock().pause(now, self.tuning.rate_limit_pause);
            return LoveSent::Retry;
        }
        if reply.retryable {
            return LoveSent::Retry;
        }
        debug!(
            "Last.fm refused {method} of '{artist} - {title}' for {user}: {}",
            super::describe(&reply)
        );
        LoveSent::Skipped
    }

    fn save_loves(&self) {
        let Some(path) = self.loves.file.get() else {
            return;
        };
        let synced = self.loves.synced.lock().clone();
        let written = serde_json::to_string_pretty(&synced)
            .map_err(std::io::Error::other)
            .and_then(|text| std::fs::write(path, text));
        if let Err(e) = written {
            warn!("{} could not be saved: {e}", path.display());
        }
    }
}

/// The songs, as artist and title, in a getStarred2 answer.
pub fn starred_songs(body: &[u8]) -> Option<Vec<(String, String)>> {
    let document: Value = serde_json::from_slice(body).ok()?;
    let response = document.get("subsonic-response")?;
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    let songs = response
        .pointer("/starred2/song")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|song| {
            let text = |name: &str| song.get(name).and_then(Value::as_str).map(str::to_string);
            Some((text("artist")?, text("title")?))
        })
        .collect();
    Some(songs)
}
