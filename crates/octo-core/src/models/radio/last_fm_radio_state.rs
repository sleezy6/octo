//! What `lastfm-radio-state.json` holds, per listener, for Last.fm radio, and the dashboard's
//! summary of a listener.
//!
//! The file is written indented by `LastFmRadioStateStore` (`octo::services::last_fm`). Field
//! names are PascalCase; each field's default is what a missing field reads as, including the
//! ones that are "now" (the current UTC time).

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::json::datetime;
use crate::last_fm::last_fm_radio_recommendation_service::{IgnoreCaseKeys, artist_key};
use crate::models::null_as_default;

/// The whole file: a version and the listeners, keyed by trimmed lower-cased username.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioStateDocument {
    pub version: i32,
    #[serde(deserialize_with = "null_as_default")]
    pub users: IndexMap<String, LastFmRadioUserState>,
}

impl Default for LastFmRadioStateDocument {
    fn default() -> Self {
        LastFmRadioStateDocument {
            version: 1,
            users: IndexMap::new(),
        }
    }
}

/// One listener's plays, stations and unplayable tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioUserState {
    #[serde(deserialize_with = "null_as_default")]
    pub username: String,
    #[serde(with = "datetime::utc")]
    pub last_seen_utc: DateTime<Utc>,
    pub new_plays_since_refresh: i32,
    #[serde(deserialize_with = "null_as_default")]
    pub plays: Vec<LastFmRadioPlay>,
    #[serde(deserialize_with = "null_as_default")]
    pub stations: Vec<LastFmRadioStation>,
    #[serde(deserialize_with = "null_as_default")]
    pub unavailable_tracks: Vec<LastFmRadioUnavailableTrack>,
    /// Songs the listener rated one star: they never play on their radio.
    #[serde(deserialize_with = "null_as_default")]
    pub radio_bans: Vec<LastFmRadioBan>,
    /// Artists the listener rated one star, and automatic artist bans they lifted. See
    /// [`LastFmRadioUserState::banned_artists`]. Absent from the file until there is one.
    #[serde(deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub artist_bans: Vec<LastFmRadioArtistBan>,
    #[serde(with = "datetime::utc_option")]
    pub last_refresh_attempt_utc: Option<DateTime<Utc>>,
    #[serde(with = "datetime::utc_option")]
    pub last_refresh_success_utc: Option<DateTime<Utc>>,
    pub last_refresh_error: Option<String>,
    pub refreshing: bool,
    /// When the dashboard last cleared this listener's history. From then on radio learns only
    /// from new plays, never from a starting profile. Absent from the file until set.
    #[serde(with = "datetime::utc_option", skip_serializing_if = "Option::is_none")]
    pub history_cleared_utc: Option<DateTime<Utc>>,
}

impl Default for LastFmRadioUserState {
    fn default() -> Self {
        LastFmRadioUserState {
            username: String::new(),
            last_seen_utc: Utc::now(),
            new_plays_since_refresh: 0,
            plays: Vec::new(),
            stations: Vec::new(),
            unavailable_tracks: Vec::new(),
            radio_bans: Vec::new(),
            artist_bans: Vec::new(),
            last_refresh_attempt_utc: None,
            last_refresh_success_utc: None,
            last_refresh_error: None,
            refreshing: false,
            history_cleared_utc: None,
        }
    }
}

/// A track that failed to play, held out of the listener's stations until `retry_after_utc`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioUnavailableTrack {
    #[serde(deserialize_with = "null_as_default")]
    pub key: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    #[serde(with = "datetime::utc")]
    pub failed_at_utc: DateTime<Utc>,
    #[serde(with = "datetime::utc")]
    pub retry_after_utc: DateTime<Utc>,
}

impl Default for LastFmRadioUnavailableTrack {
    fn default() -> Self {
        let now = Utc::now();
        LastFmRadioUnavailableTrack {
            key: String::new(),
            artist: String::new(),
            title: String::new(),
            failed_at_utc: now,
            retry_after_utc: now + TimeDelta::hours(24),
        }
    }
}

/// The listener's own word on an artist for their radio: rated one star (`banned`), or a ban
/// lifted (`banned` false): a rating other than one star, or Lift on the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioArtistBan {
    /// [`artist_key`] of the name.
    #[serde(deserialize_with = "null_as_default")]
    pub key: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    pub banned: bool,
    /// For a lifted ban: how many of the artist's songs were rated one star when it was lifted.
    /// The automatic ban stays lifted until another of their songs is.
    pub song_bans_then: i32,
    #[serde(with = "datetime::utc")]
    pub at_utc: DateTime<Utc>,
}

impl Default for LastFmRadioArtistBan {
    fn default() -> Self {
        LastFmRadioArtistBan {
            key: String::new(),
            artist: String::new(),
            banned: false,
            song_bans_then: 0,
            at_utc: Utc::now(),
        }
    }
}

/// An artist the listener's radio leaves out, as the dashboard lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BannedArtist {
    pub artist: String,
    /// Rated one star themselves, rather than banned for their songs' one stars.
    pub rated: bool,
    /// How many of their songs the listener rated one star.
    pub songs: i32,
}

impl LastFmRadioUserState {
    /// How many of each artist's songs (by [`artist_key`]) the listener rated one star, with
    /// the name of the latest.
    fn song_bans_by_artist(&self) -> IndexMap<String, (String, i32)> {
        let mut counts: IndexMap<String, (String, i32)> = IndexMap::new();
        for ban in &self.radio_bans {
            let key = artist_key(&ban.artist);
            if key.is_empty() {
                continue;
            }
            counts.entry(key).or_insert_with(|| (ban.artist.clone(), 0)).1 += 1;
        }
        counts
    }

    /// The artists this listener's radio, stations and mixes leave out: each they rated one
    /// star, and each with `after` or more songs rated one star (none for `after` 0) unless
    /// they lifted that ban and no further song of theirs was rated one star since.
    pub fn banned_artists(&self, after: i32) -> Vec<BannedArtist> {
        let counts = self.song_bans_by_artist();
        let mut out: Vec<BannedArtist> = Vec::new();
        for rule in self.artist_bans.iter().filter(|rule| rule.banned) {
            out.push(BannedArtist {
                artist: rule.artist.clone(),
                rated: true,
                songs: counts.get(&rule.key).map_or(0, |(_, n)| *n),
            });
        }
        if after > 0 {
            for (key, (artist, songs)) in &counts {
                if *songs < after {
                    continue;
                }
                let ruled = self.artist_bans.iter().find(|rule| &rule.key == key);
                match ruled {
                    Some(rule) if rule.banned => continue,
                    Some(rule) if *songs <= rule.song_bans_then => continue,
                    _ => {}
                }
                out.push(BannedArtist {
                    artist: artist.clone(),
                    rated: false,
                    songs: *songs,
                });
            }
        }
        out
    }

    /// [`Self::banned_artists`] as [`artist_key`]s.
    pub fn banned_artist_keys(&self, after: i32) -> IgnoreCaseKeys {
        self.banned_artists(after)
            .iter()
            .map(|banned| artist_key(&banned.artist))
            .collect()
    }

    /// Bans the artist from this listener's radio (`banned`), or lifts a ban on them, whether
    /// they were rated one star or banned for their songs. True when anything changed.
    pub fn set_artist_ban(&mut self, artist: &str, banned: bool, now: DateTime<Utc>) -> bool {
        let key = artist_key(artist);
        if key.is_empty() {
            return false;
        }
        let songs = self.song_bans_by_artist().get(&key).map_or(0, |(_, n)| *n);
        let rule = LastFmRadioArtistBan {
            key: key.clone(),
            artist: artist.trim().to_string(),
            banned,
            song_bans_then: if banned { 0 } else { songs },
            at_utc: now,
        };
        match self.artist_bans.iter_mut().find(|r| r.key == key) {
            Some(existing)
                if existing.banned == rule.banned && existing.song_bans_then == rule.song_bans_then =>
            {
                false
            }
            Some(existing) => {
                *existing = rule;
                true
            }
            None if !banned && songs == 0 => false,
            None => {
                self.artist_bans.insert(0, rule);
                true
            }
        }
    }

    /// Takes the banned artists' songs off the stations the listener has now. How many went.
    pub fn drop_banned_artists_from_stations(&mut self, after: i32, now: DateTime<Utc>) -> usize {
        let banned = self.banned_artist_keys(after);
        if banned.is_empty() {
            return 0;
        }
        let mut removed = 0;
        for station in &mut self.stations {
            let before = station.tracks.len();
            station
                .tracks
                .retain(|track| !banned.contains(&artist_key(&track.artist)));
            if before != station.tracks.len() {
                removed += before - station.tracks.len();
                station.changed_utc = now;
            }
        }
        removed
    }
}

/// A song the listener rated one star: it never plays on their radio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioBan {
    #[serde(deserialize_with = "null_as_default")]
    pub key: String,
    #[serde(deserialize_with = "null_as_default")]
    pub song_id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    #[serde(with = "datetime::utc")]
    pub banned_at_utc: DateTime<Utc>,
}

impl Default for LastFmRadioBan {
    fn default() -> Self {
        LastFmRadioBan {
            key: String::new(),
            song_id: String::new(),
            artist: String::new(),
            title: String::new(),
            banned_at_utc: Utc::now(),
        }
    }
}

/// One play the radio learns from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioPlay {
    #[serde(deserialize_with = "null_as_default")]
    pub song_id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration: Option<i32>,
    pub is_local: bool,
    pub hearted: bool,
    pub learned_signal: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub source: String,
    #[serde(with = "datetime::utc")]
    pub played_at_utc: DateTime<Utc>,
}

impl Default for LastFmRadioPlay {
    fn default() -> Self {
        LastFmRadioPlay {
            song_id: String::new(),
            artist: String::new(),
            title: String::new(),
            album: None,
            genre: None,
            duration: None,
            is_local: false,
            hearted: false,
            learned_signal: true,
            source: "scrobble".to_string(),
            played_at_utc: Utc::now(),
        }
    }
}

/// What a station is built from. Written as its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum LastFmRadioStationKind {
    #[default]
    Starter = 0,
    YourMix = 1,
    Discovery = 2,
    Artist = 3,
    Genre = 4,
    Pinned = 5,
}

/// One station: a read-only playlist of Last.fm suggestions for one listener.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioStation {
    #[serde(deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(deserialize_with = "null_as_default")]
    pub key: String,
    #[serde(deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(deserialize_with = "null_as_default")]
    pub owner: String,
    pub kind: LastFmRadioStationKind,
    pub personalized: bool,
    pub definition_version: i32,
    #[serde(with = "datetime::utc")]
    pub created_utc: DateTime<Utc>,
    #[serde(with = "datetime::utc")]
    pub changed_utc: DateTime<Utc>,
    #[serde(with = "datetime::utc")]
    pub valid_until_utc: DateTime<Utc>,
    #[serde(deserialize_with = "null_as_default")]
    pub seeds: Vec<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub tracks: Vec<LastFmRadioTrack>,
    /// Further suggestions, in the walk's order, that stand in for a track the station may not
    /// use once it is resolved (a library song past Discovery Radio's share). Never listed.
    #[serde(deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub reserve: Vec<LastFmRadioTrack>,
}

impl Default for LastFmRadioStation {
    fn default() -> Self {
        let now = Utc::now();
        LastFmRadioStation {
            id: String::new(),
            key: String::new(),
            name: String::new(),
            owner: String::new(),
            kind: LastFmRadioStationKind::Starter,
            personalized: false,
            definition_version: 0,
            created_utc: now,
            changed_utc: now,
            valid_until_utc: now,
            seeds: Vec::new(),
            tracks: Vec::new(),
            reserve: Vec::new(),
        }
    }
}

/// One suggestion on a station.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LastFmRadioTrack {
    #[serde(deserialize_with = "null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "null_as_default")]
    pub title: String,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration: Option<i32>,
    pub year: Option<i32>,
    pub score: f64,
    #[serde(deserialize_with = "null_as_default")]
    pub source: String,
    pub resolved_id: Option<String>,
    pub is_local: bool,
    pub external_provider: Option<String>,
    pub you_tube_id: Option<String>,
}

/// The dashboard's line for one listener.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastFmRadioUserSummary {
    pub username: String,
    pub play_count: i32,
    pub station_count: i32,
    pub new_plays_since_refresh: i32,
    #[serde(with = "datetime::utc_option")]
    pub last_refresh_success_utc: Option<DateTime<Utc>>,
    pub last_refresh_error: Option<String>,
    pub refreshing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/lastfm-radio-state.json"
        ))
        .expect("the fixture is in the repo")
    }

    fn song_ban(artist: &str, title: &str) -> LastFmRadioBan {
        LastFmRadioBan {
            key: format!("{artist}|{title}"),
            artist: artist.into(),
            title: title.into(),
            ..LastFmRadioBan::default()
        }
    }

    fn names(user: &LastFmRadioUserState, after: i32) -> Vec<(String, bool, i32)> {
        user.banned_artists(after)
            .into_iter()
            .map(|b| (b.artist, b.rated, b.songs))
            .collect()
    }

    #[test]
    fn enough_songs_rated_one_star_ban_their_artist_by_name() {
        let mut user = LastFmRadioUserState {
            radio_bans: vec![
                song_ban("Nickelback", "Photograph"),
                song_ban("NICKELBACK", "Rockstar"),
            ],
            ..LastFmRadioUserState::default()
        };
        assert!(names(&user, 3).is_empty());
        user.radio_bans
            .push(song_ban("Nickelback feat. Someone", "How You Remind Me"));
        assert_eq!(names(&user, 3), [("Nickelback".to_string(), false, 3)]);
        assert!(user.banned_artist_keys(3).contains("nickelback"));
        // 0 turns the automatic ban off.
        assert!(names(&user, 0).is_empty());
    }

    #[test]
    fn a_lifted_automatic_ban_holds_until_another_song_is_rated_one_star() {
        let now = Utc::now();
        let mut user = LastFmRadioUserState {
            radio_bans: vec![song_ban("Creed", "One"), song_ban("Creed", "Higher")],
            ..LastFmRadioUserState::default()
        };
        assert_eq!(names(&user, 2).len(), 1);
        assert!(user.set_artist_ban("Creed", false, now));
        assert!(names(&user, 2).is_empty());
        assert!(!user.set_artist_ban("Creed", false, now), "already lifted");
        user.radio_bans.push(song_ban("Creed", "Arms Wide Open"));
        assert_eq!(names(&user, 2), [("Creed".to_string(), false, 3)]);
    }

    #[test]
    fn an_artist_rated_one_star_is_banned_at_once_and_another_rating_lifts_it() {
        let now = Utc::now();
        let mut user = LastFmRadioUserState::default();
        assert!(!user.set_artist_ban("Muse", false, now), "nothing to lift");
        assert!(user.set_artist_ban("Muse", true, now));
        assert_eq!(names(&user, 3), [("Muse".to_string(), true, 0)]);
        assert!(
            !user.set_artist_ban("muse", true, now),
            "the same name in other case"
        );
        user.stations = vec![LastFmRadioStation {
            tracks: vec![
                LastFmRadioTrack {
                    artist: "Muse".into(),
                    title: "Uprising".into(),
                    ..LastFmRadioTrack::default()
                },
                LastFmRadioTrack {
                    artist: "Radiohead".into(),
                    title: "Reckoner".into(),
                    ..LastFmRadioTrack::default()
                },
            ],
            ..LastFmRadioStation::default()
        }];
        assert_eq!(user.drop_banned_artists_from_stations(3, now), 1);
        assert_eq!(user.stations[0].tracks[0].artist, "Radiohead");
        assert!(user.set_artist_ban("Muse", false, now));
        assert!(names(&user, 3).is_empty());
    }

    #[test]
    fn artist_bans_are_left_out_of_the_file_until_there_is_one() {
        let user = LastFmRadioUserState::default();
        assert!(!crate::json::to_string(&user).contains("ArtistBans"));
        let mut banned = user.clone();
        banned.set_artist_ban("Muse", true, Utc::now());
        let json = crate::json::to_string(&banned);
        assert!(
            json.contains(r#""ArtistBans":[{"Key":"muse","Artist":"Muse","Banned":true"#),
            "{json}"
        );
        let back: LastFmRadioUserState = serde_json::from_str(&json).expect("reads");
        // The time is written to the tick, so it is compared without its last digits.
        assert_eq!(back.artist_bans[0].key, "muse");
        assert!(back.artist_bans[0].banned);
        assert_eq!(back.banned_artists(3), banned.banned_artists(3));
    }

    /// The file reads and writes back byte for byte, indented.
    #[test]
    fn the_fixture_round_trips_byte_for_byte() {
        let text = fixture();
        let document: LastFmRadioStateDocument = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(document.version, 1);
        let user = &document.users["brandon"];
        assert_eq!(user.stations[0].kind, LastFmRadioStationKind::Artist);
        assert!(!user.plays[1].learned_signal);
        assert_eq!(crate::json::to_string_indented(&document), text);
    }

    /// Each field's default is what a missing field reads as.
    #[test]
    fn missing_fields_read_as_their_defaults() {
        let before = Utc::now();
        let document: LastFmRadioStateDocument = serde_json::from_str(
            r#"{"Users":{"a":{"Plays":[{}],"Stations":[{"Tracks":[{}]}],"UnavailableTracks":[{}]}}}"#,
        )
        .expect("reads");
        assert_eq!(document.version, 1);
        let user = &document.users["a"];
        assert!(user.last_seen_utc >= before);
        let play = &user.plays[0];
        assert!(play.learned_signal);
        assert_eq!(play.source, "scrobble");
        assert!(play.played_at_utc >= before);
        let unavailable = &user.unavailable_tracks[0];
        assert_eq!(
            unavailable.retry_after_utc - unavailable.failed_at_utc,
            TimeDelta::hours(24)
        );
        assert_eq!(user.stations[0].tracks[0].source, "");
    }

    /// A `null` in a string or list field reads as empty.
    #[test]
    fn nulls_in_string_and_list_fields_are_read() {
        let document: LastFmRadioStateDocument = serde_json::from_str(
            r#"{"Version":1,"Users":{"a":{"Username":null,"Plays":null,"Stations":[{"Seeds":null,"Tracks":null}]}}}"#,
        )
        .expect("reads");
        let user = &document.users["a"];
        assert_eq!(user.username, "");
        assert!(user.plays.is_empty());
        assert!(user.stations[0].tracks.is_empty());
        let empty: LastFmRadioStateDocument = serde_json::from_str(r#"{"Users":null}"#).expect("reads");
        assert!(empty.users.is_empty());
    }

    /// An empty document is written indented, with its version.
    #[test]
    fn an_empty_document_is_written_indented() {
        assert_eq!(
            crate::json::to_string_indented(&LastFmRadioStateDocument::default()),
            "{\n  \"Version\": 1,\n  \"Users\": {}\n}"
        );
    }
}
