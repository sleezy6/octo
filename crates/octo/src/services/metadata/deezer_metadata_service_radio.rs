//! Deezer's own radios, as song radio's Deezer source asks them: the track mix (Deezer's songs
//! like one song, the list behind the radio button in Deezer's player, which the public API does
//! not offer) and an artist's radio (public API). Both work with no account. The track mix is the
//! web player's gateway call, made with an anonymous session: `deezer.getUserData` with no
//! sign-in hands out a session cookie and a token, and the call needs both. Ported from
//! am-yy/octo's `DeezerMetadataService.Radio`.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet::is_blank;
use reqwest::header::{COOKIE, SET_COOKIE};
use serde_json::{Value, json};
use tracing::{debug, info};

use super::{Cached, DeezerMetadataService, NEGATIVE_TTL};
use crate::services::metadata::deezer_requests::{DeezerRequestCounter, DeezerUse};

/// A mix is Deezer's answer for the day; asking again gives about the same songs.
const RADIO_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long an anonymous session is kept before a new one is asked for. Deezer's own players
/// keep theirs about this long.
const SESSION_FOR: Duration = Duration::from_secs(5 * 60);
/// The gateway's path on www.deezer.com.
const GATEWAY_PATH: &str = "/ajax/gw-light.php";
/// The most songs an artist radio is asked for.
const ARTIST_RADIO_LIMIT: i32 = 100;

/// One song of a Deezer radio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioTrack {
    pub track_id: String,
    pub artist: String,
    /// With its version ("Radio Edit") when Deezer names one apart.
    pub title: String,
    pub seconds: Option<i32>,
}

/// The anonymous gateway session: its token and the cookies that go with it.
#[derive(Debug, Clone)]
pub(super) struct GatewaySession {
    token: String,
    cookies: String,
    until: tokio::time::Instant,
}

impl DeezerMetadataService {
    /// Deezer's track mix for a catalog track, in Deezer's order, without the track itself.
    /// None when Deezer did not answer this time (nothing is remembered then); empty when it
    /// answered with no songs.
    pub async fn track_mix(&self, track_id: &str) -> Option<Arc<Vec<RadioTrack>>> {
        if is_blank(track_id) {
            return Some(Arc::new(Vec::new()));
        }
        let key = format!("mix|{track_id}");
        if let Some(Cached::Mix(cached)) = self.get_cached(&key) {
            return Some(cached);
        }
        let body = json!({ "sng_id": track_id, "start_with_input_track": false });
        for attempt in 0..2 {
            let session = self.gateway_session(attempt > 0).await?;
            let answer = self
                .gateway("song.getSearchTrackMix", &session, Some(&body))
                .await?;
            if let Some(error) = answer
                .get("error")
                .filter(|e| e.as_object().is_some_and(|o| !o.is_empty()))
            {
                // A session Deezer let go of: one fresh one, then give up for this call.
                if error.get("VALID_TOKEN_REQUIRED").is_some() && attempt == 0 {
                    continue;
                }
                info!("Deezer's track mix for {track_id} was refused: {error}");
                return None;
            }
            let tracks: Vec<RadioTrack> = gateway_tracks(&answer)
                .into_iter()
                .filter(|track| track.track_id != track_id)
                .collect();
            let tracks = Arc::new(tracks);
            let ttl = if tracks.is_empty() {
                NEGATIVE_TTL
            } else {
                RADIO_TTL
            };
            self.put(key, Cached::Mix(Arc::clone(&tracks)), ttl);
            return Some(tracks);
        }
        None
    }

    /// Deezer's radio for a catalog artist: their songs and similar artists'. None when Deezer
    /// did not answer this time.
    pub async fn artist_radio(&self, artist_id: &str) -> Option<Arc<Vec<RadioTrack>>> {
        if is_blank(artist_id) {
            return Some(Arc::new(Vec::new()));
        }
        let key = format!("artist-radio|{artist_id}");
        if let Some(Cached::Mix(cached)) = self.get_cached(&key) {
            return Some(cached);
        }
        let url = format!(
            "{}/artist/{}/radio?limit={ARTIST_RADIO_LIMIT}",
            self.base(),
            octo_core::common::dotnet::escape_data_string(artist_id)
        );
        let answer = self.get_json(&url, false).await;
        if answer.transient {
            return None;
        }
        let tracks = Arc::new(answer.doc.as_ref().map(public_tracks).unwrap_or_default());
        let ttl = if tracks.is_empty() {
            NEGATIVE_TTL
        } else {
            RADIO_TTL
        };
        self.put(key, Cached::Mix(Arc::clone(&tracks)), ttl);
        Some(tracks)
    }

    /// The anonymous session, a kept one unless `fresh`. None when Deezer did not hand one out.
    async fn gateway_session(&self, fresh: bool) -> Option<GatewaySession> {
        let mut kept = self.inner.gateway_session.lock().await;
        if !fresh && let Some(session) = kept.as_ref().filter(|s| tokio::time::Instant::now() < s.until) {
            return Some(session.clone());
        }
        *kept = None;
        let url = format!(
            "{}{GATEWAY_PATH}?method=deezer.getUserData&input=3&api_version=1.0&api_token=",
            self.inner.gateway
        );
        DeezerRequestCounter::global().count(DeezerUse::Radio, false);
        let response = match self.inner.gateway_client.get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                debug!("Deezer's gateway gave no session: HTTP {}", response.status());
                return None;
            }
            Err(e) => {
                debug!("Deezer's gateway gave no session: {e}");
                return None;
            }
        };
        let cookies = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split(';').next())
            .map(str::trim)
            .filter(|pair| !pair.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        let doc: Value = response.json().await.ok()?;
        let token = doc
            .get("results")
            .and_then(|results| results.get("checkForm"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())?
            .to_string();
        let session = GatewaySession {
            token,
            cookies,
            until: tokio::time::Instant::now() + SESSION_FOR,
        };
        *kept = Some(session.clone());
        Some(session)
    }

    /// One gateway call with the session. None when Deezer did not answer with a document.
    async fn gateway(&self, method: &str, session: &GatewaySession, body: Option<&Value>) -> Option<Value> {
        let url = format!(
            "{}{GATEWAY_PATH}?method={method}&input=3&api_version=1.0&api_token={}",
            self.inner.gateway,
            octo_core::common::dotnet::escape_data_string(&session.token)
        );
        // The gateway is not the public API, so the rate limiter does not count it; the
        // dashboard's request counts do.
        DeezerRequestCounter::global().count(DeezerUse::Radio, false);
        let mut request = match body {
            Some(body) => self.inner.gateway_client.post(&url).json(body),
            None => self.inner.gateway_client.get(&url),
        };
        if !session.cookies.is_empty() {
            request = request.header(COOKIE, &session.cookies);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                response.json::<Value>().await.ok().filter(Value::is_object)
            }
            Ok(response) => {
                debug!("Deezer's gateway {method} answered HTTP {}", response.status());
                None
            }
            Err(e) => {
                debug!("Deezer's gateway {method} failed: {e}");
                None
            }
        }
    }
}

/// The songs of a gateway answer (`results.data`).
pub(super) fn gateway_tracks(answer: &Value) -> Vec<RadioTrack> {
    let Some(rows) = answer
        .get("results")
        .and_then(|results| results.get("data"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let text = |row: &Value, name: &str| -> Option<String> {
        match row.get(name)? {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    };
    rows.iter()
        .filter_map(|row| {
            let track_id = text(row, "SNG_ID")?;
            let mut title = text(row, "SNG_TITLE")?;
            let artist = text(row, "ART_NAME")?;
            // The gateway keeps "Radio Edit" and the like apart from the title; the public API
            // joins them.
            if let Some(version) = text(row, "VERSION") {
                title = format!("{title} {version}");
            }
            let seconds = text(row, "DURATION")
                .and_then(|d| d.parse::<i32>().ok())
                .filter(|s| *s > 0);
            Some(RadioTrack {
                track_id,
                artist,
                title,
                seconds,
            })
        })
        .collect()
}

/// The songs of a public API list (`data`, each with `id`, `title`, `artist.name`, `duration`).
pub(super) fn public_tracks(doc: &Value) -> Vec<RadioTrack> {
    let Some(rows) = doc.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let track_id = match row.get("id")? {
                Value::Number(n) => n.to_string(),
                Value::String(s) if !s.is_empty() => s.clone(),
                _ => return None,
            };
            let title = row
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !is_blank(t))?;
            let artist = row
                .get("artist")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .filter(|a| !is_blank(a))?;
            let seconds = row
                .get("duration")
                .and_then(Value::as_i64)
                .and_then(|d| i32::try_from(d).ok())
                .filter(|s| *s > 0);
            Some(RadioTrack {
                track_id,
                artist: artist.trim().to_string(),
                title: title.trim().to_string(),
                seconds,
            })
        })
        .collect()
}
