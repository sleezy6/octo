//! A mock Deezer for tests: one server stands in for the gateway (`/gw`), the media endpoint
//! (`/media`), the CDN (`/cdn/...`) and the public catalog (everything else), and records what
//! it was asked. The one song it knows is track 42, by `catalog_artist`, called `catalog_title`.

use std::sync::Arc;

use octo_core::settings::{AppSettings, DeezerSettings, SettingsStore};
use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::deezer_decrypted_stream::encrypt_stripes;
use super::deezer_resolver::{DeezerOpenStream, DeezerResolver};
use crate::services::metadata::deezer_metadata_service::DeezerMetadataService;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;

#[derive(Clone)]
pub(crate) struct Scenario {
    pub(crate) payload: Vec<u8>,
    pub(crate) ignore_range: bool,
    pub(crate) reject_primary: bool,
    /// Track 42 has no media of its own; Deezer names 43 in its place.
    pub(crate) fallback_track: bool,
    /// ... and names nothing, so the catalog's copy (44) is found by search.
    pub(crate) no_fallback_id: bool,
    pub(crate) wrong_fallback_version: bool,
    pub(crate) reject_first_url: bool,
    pub(crate) reject_primary_cdn: bool,
    pub(crate) available_formats: Vec<&'static str>,
    /// Who the catalog's one song is by, and what it is called.
    pub(crate) catalog_artist: String,
    pub(crate) catalog_title: String,
    /// What a sign-in says about the plan (`OFFER_NAME`, `COUNTRY`, `web_lossless`, `web_hq`),
    /// when it says.
    pub(crate) plan: Option<(&'static str, &'static str, bool, bool)>,
    /// The gateway answers 503 to the fallback account's sign-in.
    pub(crate) fallback_unanswered: bool,
    /// Formats `pageTrack` says the track has no file in (`FILESIZE_<format>` "0"); the others
    /// it gives a size. Empty: no sizes at all, as an older answer.
    pub(crate) no_file_in: Vec<&'static str>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            payload: b"0123456789ab".to_vec(),
            ignore_range: false,
            reject_primary: false,
            fallback_track: false,
            no_fallback_id: false,
            wrong_fallback_version: false,
            reject_first_url: false,
            reject_primary_cdn: false,
            available_formats: vec!["MP3_128", "MP3_320", "FLAC"],
            catalog_artist: "Artist".into(),
            catalog_title: "Title".into(),
            plan: None,
            fallback_unanswered: false,
            no_file_in: Vec::new(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Seen {
    pub(crate) auth_cookies: Vec<String>,
    pub(crate) page_cookies: Vec<String>,
    pub(crate) page_ids: Vec<String>,
    pub(crate) media_bodies: Vec<String>,
    pub(crate) media_cookies: Vec<String>,
    pub(crate) cdn_ranges: Vec<String>,
    pub(crate) urls_issued: usize,
    /// API tokens the gateway no longer accepts for `pageTrack`, as a sign-in that expired early.
    pub(crate) stale_tokens: Vec<String>,
    /// License tokens `get_url` no longer accepts (error 1000), as a sign-in that expired early.
    pub(crate) expired_licenses: Vec<String>,
    /// The User-Agent of every sign-in.
    pub(crate) user_agents: Vec<String>,
}

struct Deezer {
    scenario: Scenario,
    seen: Arc<Mutex<Seen>>,
    base: Arc<Mutex<String>>,
}

fn header(request: &Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

impl Respond for Deezer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let query = request.url.query().unwrap_or("").to_string();
        let scenario = &self.scenario;
        let mut seen = self.seen.lock();
        if path == "/gw" && query.contains("method=deezer.getUserData") {
            let cookie = header(request, "cookie");
            seen.auth_cookies.push(cookie.clone());
            seen.user_agents.push(header(request, "user-agent"));
            let account = cookie.trim_start_matches("arl=").to_string();
            let rejected = scenario.reject_primary && account == "primary";
            if scenario.fallback_unanswered && account == "fallback" {
                return ResponseTemplate::new(503);
            }
            let mut body = json!({
                "error": [],
                "results": {
                    "checkForm": format!("api-{account}-{}", seen.auth_cookies.iter().filter(|c| **c == cookie).count()),
                    "USER": {
                        "USER_ID": if rejected { 0 } else { 7 },
                        "OPTIONS": { "license_token": format!("{account}-license") },
                    },
                },
            });
            if let Some((offer, country, lossless, high_quality)) = scenario.plan {
                body["results"]["OFFER_NAME"] = json!(offer);
                body["results"]["COUNTRY"] = json!(country);
                body["results"]["USER"]["OPTIONS"]["web_lossless"] = json!(lossless);
                // Written as the gateway sometimes writes a yes or no: a number.
                body["results"]["USER"]["OPTIONS"]["web_hq"] = json!(i32::from(high_quality));
            }
            return ResponseTemplate::new(200)
                .insert_header("Set-Cookie", format!("sid={account}-session; Path=/; HttpOnly"))
                .set_body_json(body);
        }
        if path == "/gw" && query.contains("method=deezer.pageTrack") {
            seen.page_cookies.push(header(request, "cookie"));
            let api_token = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("api_token="))
                .unwrap_or("")
                .to_string();
            if seen.stale_tokens.contains(&api_token) {
                return ResponseTemplate::new(200).set_body_json(
                    json!({ "error": { "VALID_TOKEN_REQUIRED": "Invalid CSRF token" }, "results": {} }),
                );
            }
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let id = body["SNG_ID"].as_str().unwrap_or("").to_string();
            seen.page_ids.push(id.clone());
            let mut data = json!({
                "SNG_ID": id,
                "TRACK_TOKEN": format!("token-{id}"),
                "ART_NAME": scenario.catalog_artist,
                "SNG_TITLE": scenario.catalog_title,
            });
            if id == "43" && scenario.wrong_fallback_version {
                data["VERSION"] = json!("(Live)");
            }
            if !scenario.no_file_in.is_empty() {
                for format in ["FLAC", "MP3_320", "MP3_128"] {
                    let size = if scenario.no_file_in.contains(&format) {
                        "0"
                    } else {
                        "1000"
                    };
                    data[format!("FILESIZE_{format}")] = json!(size);
                }
            }
            if id == "42" && scenario.fallback_track && !scenario.no_fallback_id {
                data["FALLBACK"] = json!({ "SNG_ID": "43" });
            }
            return ResponseTemplate::new(200)
                .set_body_json(json!({ "error": [], "results": { "DATA": data } }));
        }
        if path == "/media" {
            let text = String::from_utf8_lossy(&request.body).to_string();
            seen.media_bodies.push(text.clone());
            seen.media_cookies.push(header(request, "cookie"));
            let body: Value = serde_json::from_str(&text).unwrap_or_default();
            let token = body["track_tokens"][0].as_str().unwrap_or("").to_string();
            let license = body["license_token"].as_str().unwrap_or("").to_string();
            if seen.expired_licenses.contains(&license) {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "data": [{ "errors": [{ "code": 1000, "message": "License token has no sufficient rights on requested media" }] }],
                }));
            }
            if scenario.fallback_track && token == "token-42" {
                return ResponseTemplate::new(200).set_body_json(json!({ "data": [{ "media": [] }] }));
            }
            let track = token.trim_start_matches("token-").to_string();
            let asked: Vec<String> = body["media"][0]["formats"]
                .as_array()
                .map(|formats| {
                    formats
                        .iter()
                        .filter_map(|f| f["format"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            seen.urls_issued += 1;
            let issue = seen.urls_issued;
            let base = self.base.lock().clone();
            let media: Vec<Value> = asked
                .iter()
                .filter(|format| scenario.available_formats.contains(&format.as_str()))
                .map(|format| {
                    json!({
                        "format": format,
                        "sources": [{ "url": format!("{base}/cdn/{license}/{track}/{issue}") }],
                    })
                })
                .collect();
            return ResponseTemplate::new(200).set_body_json(json!({ "data": [{ "media": media }] }));
        }
        if let Some(rest) = path.strip_prefix("/cdn/") {
            let parts: Vec<&str> = rest.split('/').collect();
            let (license, track, issue) = (parts[0], parts[1], parts[2]);
            let range = header(request, "range");
            seen.cdn_ranges.push(range.clone());
            if (scenario.reject_first_url && issue == "1")
                || (scenario.reject_primary_cdn && license == "primary-license")
            {
                return ResponseTemplate::new(403);
            }
            let stored = encrypt_stripes(&scenario.payload, track);
            let total = stored.len();
            let from = range
                .strip_prefix("bytes=")
                .and_then(|r| r.strip_suffix('-'))
                .and_then(|n| n.parse::<usize>().ok());
            return match from {
                Some(from) if !scenario.ignore_range && from >= total => {
                    ResponseTemplate::new(416).insert_header("Content-Range", format!("bytes */{total}"))
                }
                Some(from) if !scenario.ignore_range => ResponseTemplate::new(206)
                    .insert_header("Content-Range", format!("bytes {from}-{}/{total}", total - 1))
                    .set_body_bytes(stored[from..].to_vec()),
                _ => ResponseTemplate::new(200).set_body_bytes(stored),
            };
        }
        // The public catalog.
        if path == "/track/42" {
            return ResponseTemplate::new(200).set_body_json(json!({
                "id": 42, "title": scenario.catalog_title, "artist": { "name": scenario.catalog_artist },
            }));
        }
        if path == "/search" {
            return ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "id": 42, "readable": false, "title": scenario.catalog_title,
                        "artist": { "name": scenario.catalog_artist },
                    },
                    {
                        "id": 44, "readable": true, "title": scenario.catalog_title,
                        "artist": { "name": scenario.catalog_artist },
                    },
                ],
            }));
        }
        ResponseTemplate::new(404)
    }
}

pub(crate) struct Fixture {
    pub(crate) resolver: Arc<DeezerResolver>,
    pub(crate) settings: Arc<SettingsStore>,
    pub(crate) seen: Arc<Mutex<Seen>>,
    pub(crate) scenario: Scenario,
    pub(crate) _server: MockServer,
}

pub(crate) fn deezer(arl: &str, fallback: Option<&str>) -> AppSettings {
    AppSettings {
        deezer: DeezerSettings {
            arl: Some(arl.to_string()),
            arl_fallback: fallback.map(str::to_string),
            stream_quality: "MP3_320".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

impl Fixture {
    pub(crate) async fn new(scenario: Scenario) -> Fixture {
        Self::with_accounts(scenario, "primary", None).await
    }

    pub(crate) async fn with_accounts(scenario: Scenario, arl: &str, fallback: Option<&str>) -> Fixture {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Seen::default()));
        Mock::given(wiremock::matchers::any())
            .respond_with(Deezer {
                scenario: scenario.clone(),
                seen: seen.clone(),
                base: Arc::new(Mutex::new(server.uri())),
            })
            .mount(&server)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(deezer(arl, fallback)));
        let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
        let catalog = Arc::new(DeezerMetadataService::with_base_url(
            http,
            settings.clone(),
            server.uri(),
        ));
        let resolver = Arc::new(DeezerResolver::with_endpoints(
            settings.clone(),
            catalog,
            &format!("{}/gw", server.uri()),
            &format!("{}/media", server.uri()),
            true,
        ));
        Fixture {
            resolver,
            settings,
            seen,
            scenario,
            _server: server,
        }
    }

    pub(crate) async fn open(&self, range: Option<&str>) -> Option<DeezerOpenStream> {
        self.resolver.open_stream("42", range, "MP3_320").await
    }
}
