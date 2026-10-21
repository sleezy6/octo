//! The catalog's own id for a song, for the callers that fetch the song itself from Deezer
//! (`services::deezer`): outside songs are known to Octo by artist and title, and Deezer's
//! media endpoints by track id. Adapted from am-yy/octo's `DeezerMetadataService`
//! (`GetTrackIdentityByIdAsync`, `FindAlternativeTrackIdAsync`, `RecordingMatches`).

use std::collections::HashSet;

use octo_core::common::SongIdentity;
use octo_core::common::dotnet::{escape_data_string, is_blank};
use octo_core::json::element::{ElementResult, str_prop, try_get_property};
use serde_json::Value;
use tracing::debug;

use super::{
    Cached, DeezerMetadataService, FieldVerdict, MATCH_CANDIDATES, NEGATIVE_TTL, POSITIVE_TTL, all_matches,
    compare_artists, compare_titles, number_id,
};

/// How many plain searches look for another copy of a recording.
const ALTERNATIVE_SEARCHES: usize = 2;

/// The same recording: every field the request states agrees, and it states at least one.
/// Stricter than a search match, which lets an absent field pass: a wrong song here is the
/// wrong audio under the right name.
pub fn recording_matches(
    expected_artist: Option<&str>,
    expected_title: Option<&str>,
    candidate_artist: Option<&str>,
    candidate_title: Option<&str>,
) -> bool {
    let artist = expected_artist.filter(|a| !is_blank(a));
    let title = expected_title.filter(|t| !is_blank(t));
    if artist.is_none() && title.is_none() {
        return false;
    }
    let artist = artist.map(|a| compare_artists(a, candidate_artist));
    let title = title.map(|t| compare_titles(t, candidate_title));
    // Each field given agrees, and one of them by the same name: "Grimes" held in "Gary
    // Grimes" and "Visions" in "Starhand Visions" is another record.
    artist.is_none_or(|v| v.agrees())
        && title.is_none_or(|v| v.agrees())
        && [artist, title]
            .into_iter()
            .flatten()
            .any(|v| v == FieldVerdict::Match)
}

/// A track record's artist and title, the title with its version when the record names one
/// apart ("title_version").
pub(super) fn identity_of(track: &Value) -> ElementResult<Option<(String, String)>> {
    let artist = match try_get_property(track, "artist")? {
        Some(credit) => str_prop(credit, "name")?,
        None => None,
    };
    let title = str_prop(track, "title")?;
    let (Some(artist), Some(title)) = (artist.filter(|a| !is_blank(a)), title.filter(|t| !is_blank(t)))
    else {
        return Ok(None);
    };
    Ok(Some((artist.to_string(), title.to_string())))
}

/// A track record the catalog marks as not playable (`readable: false`). A record that does
/// not say is taken as playable.
pub(super) fn unreadable(track: &Value) -> bool {
    matches!(track.get("readable"), Some(Value::Bool(false)))
}

fn playable_key(artist: &str, title: &str, isrc: Option<&str>) -> String {
    format!(
        "playable|{}|{}|{}",
        SongIdentity::key(artist),
        SongIdentity::key(title),
        isrc.unwrap_or("")
    )
}

impl DeezerMetadataService {
    /// The catalog id of this song: the track its ISRC names when that is the same recording,
    /// else the track search's match for the artist and title, else the track another catalog's
    /// ISRC for the song names (see [`super::deezer_metadata_service::IsrcSource`]; `seconds`,
    /// when known, tells apart two songs of one name). None when the catalog has no such song
    /// or did not answer; only an answer is remembered.
    pub async fn find_track_id(
        &self,
        artist: &str,
        title: &str,
        isrc: Option<&str>,
        seconds: Option<i32>,
    ) -> Option<String> {
        if is_blank(artist) && is_blank(title) {
            return None;
        }
        let isrc = isrc.and_then(SongIdentity::normalize_isrc);
        let key = playable_key(artist, title, isrc.as_deref());
        if let Some(Cached::Id(id)) = self.get_cached(&key) {
            return id;
        }

        let mut transient = false;
        let mut found = None;
        if let Some(code) = &isrc {
            let (id, busy) = self.track_id_by_isrc(code, artist, title).await;
            found = id;
            transient |= busy;
        }
        if found.is_none() {
            match self.find_track(artist, title, false).await {
                Ok((busy, hit)) => {
                    transient |= busy;
                    found = hit.and_then(|(track, _)| number_id(&track).ok().flatten());
                }
                Err(e) => {
                    debug!("deezer track id for '{artist} - {title}' could not be read: {e}");
                    transient = true;
                }
            }
        }
        // Search can miss a song the catalog has: the copy it finds is one not sold where Octo
        // runs, or the artist shares a name. Another catalog's ISRC names the recording exactly.
        if found.is_none()
            && !transient
            && !is_blank(artist)
            && !is_blank(title)
            && let Some(source) = self.inner.isrc_source.get().cloned()
        {
            for code in source
                .isrcs_for(artist, title, seconds)
                .await
                .into_iter()
                .filter(|code| isrc.as_deref() != Some(code.as_str()))
                .take(3)
            {
                let (id, busy) = self.track_id_by_isrc(&code, artist, title).await;
                transient |= busy;
                if id.is_some() {
                    debug!("deezer found '{artist} - {title}' by its ISRC {code}");
                    found = id;
                    break;
                }
            }
        }

        if found.is_some() || !transient {
            let ttl = if found.is_some() {
                POSITIVE_TTL
            } else {
                NEGATIVE_TTL
            };
            self.put(key, Cached::Id(found.clone()), ttl);
        }
        found
    }

    /// The track with this ISRC when it is the recording asked for, and whether the catalog
    /// failed to answer.
    async fn track_id_by_isrc(&self, isrc: &str, artist: &str, title: &str) -> (Option<String>, bool) {
        let r = self
            .get_json(
                &format!("{}/track/isrc:{}", self.base(), escape_data_string(isrc)),
                false,
            )
            .await;
        if r.transient {
            return (None, true);
        }
        let Some(track @ Value::Object(_)) = r.doc.as_ref() else {
            return (None, false);
        };
        let read = || -> ElementResult<Option<String>> {
            let Some((got_artist, got_title)) = identity_of(track)? else {
                return Ok(None);
            };
            if !recording_matches(Some(artist), Some(title), Some(&got_artist), Some(&got_title)) {
                return Ok(None);
            }
            number_id(track)
        };
        (read().unwrap_or(None), false)
    }

    /// The catalog id of the track with this ISRC, whatever it is called. The caller checks it
    /// is the recording it wants.
    pub async fn track_id_for_isrc(&self, isrc: &str) -> Option<String> {
        let code = SongIdentity::normalize_isrc(isrc)?;
        let r = self
            .get_json(
                &format!("{}/track/isrc:{}", self.base(), escape_data_string(&code)),
                false,
            )
            .await;
        match r.doc.as_ref() {
            Some(track @ Value::Object(_)) => number_id(track).unwrap_or(None),
            _ => None,
        }
    }

    /// The artist and title of the track with this catalog id.
    pub async fn track_identity_by_id(&self, id: &str) -> Option<(String, String)> {
        let r = self
            .get_json(
                &format!("{}/track/{}", self.base(), escape_data_string(id)),
                false,
            )
            .await;
        match r.doc.as_ref() {
            Some(track @ Value::Object(_)) => identity_of(track).unwrap_or(None),
            _ => None,
        }
    }

    /// Another playable copy of the same recording, by the same identity guard, leaving out
    /// the ids already tried.
    pub async fn find_alternative_track_id(
        &self,
        artist: &str,
        title: &str,
        excluded: &HashSet<String>,
    ) -> Option<String> {
        if is_blank(artist) || is_blank(title) {
            return None;
        }
        for variant in SongIdentity::query_variants(title, artist)
            .into_iter()
            .take(ALTERNATIVE_SEARCHES)
        {
            let url = format!(
                "{}/search?q={}&limit={}",
                self.base(),
                escape_data_string(&variant.text()),
                MATCH_CANDIDATES * 3
            );
            let r = self.get_json(&url, false).await;
            if r.transient {
                return None;
            }
            let hits = match all_matches(r.doc.as_ref(), artist, title, None) {
                Ok(hits) => hits,
                Err(e) => {
                    debug!("deezer alternatives for '{artist} - {title}' could not be read: {e}");
                    return None;
                }
            };
            for hit in hits {
                if unreadable(&hit) {
                    continue;
                }
                let Ok(Some(id)) = number_id(&hit) else {
                    continue;
                };
                if excluded.contains(&id) {
                    continue;
                }
                let Ok(Some((got_artist, got_title))) = identity_of(&hit) else {
                    continue;
                };
                if recording_matches(Some(artist), Some(title), Some(&got_artist), Some(&got_title)) {
                    return Some(id);
                }
            }
        }
        None
    }
}
