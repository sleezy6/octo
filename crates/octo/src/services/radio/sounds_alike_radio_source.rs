//! Sounds alike as a radio source: library songs that sound like the seed, from octo-sonic's
//! analysis, or (`RadioSources:SoundsAlikeFrom` = `navidrome`) from Navidrome's own
//! getSimilarSongs, which a plugin such as AudioMuse-AI can answer. Only for library seeds.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use octo_core::models::domain::Song;
use octo_core::radio::radio_provider::SOUNDS_ALIKE;
use octo_core::radio::{RadioAnswer, RadioMatch, RadioSeed};
use octo_core::settings::SettingsStore;
use serde_json::Value;
use tracing::debug;

use super::radio_source::IRadioSource;
use crate::services::sonic::SonicStore;
use crate::services::subsonic::NavidromeIdentityService;

pub struct SoundsAlikeRadioSource {
    store: Arc<SonicStore>,
    /// The radio source settings.
    settings: Arc<SettingsStore>,
    /// For Navidrome's getSimilarSongs.
    http: reqwest::Client,
    /// Octo's own sign-in to Navidrome, for a station refresh, which has no listener's.
    identity: Option<NavidromeIdentityService>,
}

/// The keys of a Subsonic sign-in, all a call made as the listener carries over.
const SIGN_IN_KEYS: [&str; 7] = ["u", "p", "t", "s", "apiKey", "v", "c"];

impl SoundsAlikeRadioSource {
    pub fn new(store: Arc<SonicStore>, settings: Arc<SettingsStore>) -> Self {
        let http = crate::services::framework::http::client_builder()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("the Sounds alike client builds");
        SoundsAlikeRadioSource {
            store,
            settings,
            http,
            identity: None,
        }
    }

    /// Asks Navidrome as Octo itself when a call carries no listener's sign-in.
    pub fn with_identity(mut self, identity: NavidromeIdentityService) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Navidrome's similar songs for a library song, asked as the listener; nothing when
    /// Navidrome has none or cannot be asked.
    async fn navidrome_similar(
        &self,
        song_id: &str,
        count: usize,
        auth: &IndexMap<String, String>,
    ) -> Vec<Song> {
        let base = self.settings.current().subsonic.url.clone().unwrap_or_default();
        if base.trim().is_empty() || count == 0 {
            return Vec::new();
        }
        let mut query: Vec<(String, String)> = auth
            .iter()
            .filter(|(key, _)| SIGN_IN_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !query.iter().any(|(key, _)| key == "u" || key == "apiKey") {
            let Some(identity) = &self.identity else {
                return Vec::new();
            };
            if identity.get_scan_auth().is_none() {
                let _ = identity.ensure_admin_jwt().await;
            }
            let Some((u, t, s)) = identity.get_scan_auth() else {
                debug!("Navidrome's similar songs need a sign-in; Octo has no Navidrome admin sign-in yet");
                return Vec::new();
            };
            query = vec![
                ("u".into(), u),
                ("t".into(), t),
                ("s".into(), s),
                ("v".into(), "1.16.1".into()),
                ("c".into(), "octo".into()),
            ];
        }
        query.extend([
            ("id".into(), song_id.to_string()),
            ("count".into(), count.to_string()),
            ("f".into(), "json".into()),
        ]);
        let url = format!("{}/rest/getSimilarSongs", base.trim_end_matches('/'));
        let body: Option<Value> = match self.http.get(&url).query(&query).send().await {
            Ok(response) => response.json().await.ok(),
            Err(e) => {
                debug!("Navidrome's similar songs for {song_id} could not be asked: {e}");
                None
            }
        };
        similar_songs(body.as_ref())
    }
}

/// The songs of a getSimilarSongs answer, as library songs; none for an error.
pub fn similar_songs(body: Option<&Value>) -> Vec<Song> {
    let Some(response) = body.and_then(|body| body.get("subsonic-response")) else {
        return Vec::new();
    };
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        return Vec::new();
    }
    let text = |song: &Value, name: &str| song.get(name).and_then(Value::as_str).unwrap_or("").to_string();
    response
        .pointer("/similarSongs/song")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|song| !text(song, "id").is_empty())
        .map(|song| Song {
            id: text(song, "id"),
            title: text(song, "title"),
            artist: text(song, "artist"),
            album: text(song, "album"),
            duration: song
                .get("duration")
                .and_then(Value::as_i64)
                .and_then(|d| i32::try_from(d).ok()),
            is_local: true,
            ..Default::default()
        })
        .collect()
}

#[async_trait]
impl IRadioSource for SoundsAlikeRadioSource {
    fn provider(&self) -> &'static str {
        SOUNDS_ALIKE
    }

    fn available(&self) -> bool {
        self.settings.current().radio_sources.sounds_alike
    }

    async fn songs_like(
        &self,
        seed: &RadioSeed,
        count: i32,
        auth: &IndexMap<String, String>,
    ) -> anyhow::Result<RadioAnswer> {
        let Some(song) = seed.library_song.as_ref().filter(|song| !song.id.is_empty()) else {
            return Ok(RadioAnswer::nothing(SOUNDS_ALIKE));
        };
        if seed.is_artist() {
            return Ok(RadioAnswer::nothing(SOUNDS_ALIKE));
        }
        let count = count.max(0) as usize;
        let songs = if self
            .settings
            .current()
            .radio_sources
            .sounds_alike_from_navidrome()
        {
            self.navidrome_similar(&song.id, count, auth)
                .await
                .into_iter()
                .filter(|similar| similar.id != song.id)
                .collect()
        } else {
            self.sonic_similar(&song.id, count)
        };
        let r#match = if songs.is_empty() {
            RadioMatch::None
        } else {
            RadioMatch::Song
        };
        let mut answer = RadioAnswer::new(SOUNDS_ALIKE, r#match, Vec::new(), songs);
        answer.library_songs_complete = false;
        Ok(answer)
    }
}

impl SoundsAlikeRadioSource {
    /// octo-sonic's nearest songs to a library song.
    fn sonic_similar(&self, song_id: &str, count: usize) -> Vec<Song> {
        let near = self.store.nearest(song_id, count);
        // Stubs: SongRadioService reads each picked song again with the listener's own sign-in.
        near.into_iter()
            .map(|(id, song, _)| Song {
                id,
                title: song.title,
                artist: song.artist,
                album: song.album.unwrap_or_default(),
                duration: song.duration,
                is_local: true,
                ..Default::default()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn similar_songs_are_read_from_a_get_similar_songs_answer() {
        let body = json!({"subsonic-response": {"status": "ok", "similarSongs": {"song": [
            {"id": "a1", "title": "Archangel", "artist": "Burial", "album": "Untrue", "duration": 238},
            {"title": "No id"}
        ]}}});
        let songs = similar_songs(Some(&body));
        assert_eq!(songs.len(), 1);
        assert_eq!(
            (songs[0].id.as_str(), songs[0].title.as_str(), songs[0].duration),
            ("a1", "Archangel", Some(238))
        );
        assert!(songs[0].is_local);
    }

    #[test]
    fn an_error_or_an_empty_answer_gives_no_songs() {
        let failed = json!({"subsonic-response": {"status": "failed", "error": {"code": 70}}});
        let empty = json!({"subsonic-response": {"status": "ok", "similarSongs": {}}});
        assert!(similar_songs(Some(&failed)).is_empty());
        assert!(similar_songs(Some(&empty)).is_empty());
        assert!(similar_songs(None).is_empty());
    }

    /// With Sounds alike from Navidrome, the seed's similar songs are Navidrome's, asked with
    /// the listener's own sign-in, and the seed itself is left out.
    #[tokio::test]
    async fn from_navidrome_the_listeners_sign_in_asks_navidromes_similar_songs() {
        use octo_core::settings::AppSettings;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let navidrome = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/getSimilarSongs"))
            .and(query_param("id", "seed"))
            .and(query_param("u", "alice"))
            .and(query_param("t", "tok"))
            .and(query_param("f", "json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"subsonic-response": {
                "status": "ok", "similarSongs": {"song": [
                    {"id": "seed", "title": "Archangel", "artist": "Burial"},
                    {"id": "n1", "title": "Near Dark", "artist": "Burial", "duration": 236}
                ]}}})),
            )
            .expect(1)
            .mount(&navidrome)
            .await;
        let mut settings = AppSettings::default();
        settings.subsonic.url = Some(navidrome.uri());
        settings.radio_sources.sounds_alike_from = "navidrome".into();
        let dir = tempfile::tempdir().expect("a temp dir");
        let source = SoundsAlikeRadioSource::new(
            Arc::new(SonicStore::new(dir.path().join("sonic.json"))),
            Arc::new(SettingsStore::for_tests(settings)),
        );
        let seed = RadioSeed::new(
            "Burial",
            "Archangel",
            Some(238),
            Some(Song {
                id: "seed".into(),
                is_local: true,
                ..Default::default()
            }),
        );
        let auth: IndexMap<String, String> = [("u", "alice"), ("t", "tok"), ("s", "salt"), ("x", "dropped")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let answer = source.songs_like(&seed, 10, &auth).await.expect("an answer");

        let ids: Vec<&str> = answer.library_songs.iter().map(|song| song.id.as_str()).collect();
        assert_eq!(ids, ["n1"]);
    }

    #[test]
    fn sounds_alike_is_from_octo_sonic_unless_navidrome_is_chosen() {
        use octo_core::settings::radio_source::RadioSourceSettings;
        let from = |value: &str| {
            RadioSourceSettings {
                sounds_alike_from: value.into(),
                ..Default::default()
            }
            .sounds_alike_from_navidrome()
        };
        assert!(!RadioSourceSettings::default().sounds_alike_from_navidrome());
        assert!(from(" Navidrome "));
        assert!(!from("octo-sonic"));
        assert!(!from("audiomuse"));
    }
}
