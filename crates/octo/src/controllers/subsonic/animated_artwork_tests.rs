//! octoAnimatedArtwork as the Octo apps see it: the extension listed while a server is set,
//! getAnimatedArtwork, and the `animatedArtwork` getAlbum carries.

use axum::body::Body;
use axum::http::header;
use axum::response::Response;
use http_body_util::BodyExt;
use octo_core::settings::AppSettings;
use serde_json::{Value, json};
use wiremock::matchers::{any, method, path, query_param};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::animated_artwork::with_album_artwork;
use super::test_app::*;
use crate::app::AppState;
use crate::http::pipeline::App;

const SQUARE: &str = "https://mvod.itunes.apple.com/itunes-assets/square.m3u8";
const TALL: &str = "https://mvod.itunes.apple.com/itunes-assets/tall.m3u8";

/// Navidrome as these calls need it: a ping that takes the token "good", and a short
/// extension list.
struct Navidrome;

impl Respond for Navidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        if path.ends_with("/rest/ping") {
            return if query_of(request).get("t").map(String::as_str) == Some("good") {
                navidrome_json(NAVIDROME_OK)
            } else {
                navidrome_json(NAVIDROME_WRONG_PASSWORD)
            };
        }
        if path.ends_with("/rest/getOpenSubsonicExtensions") {
            return navidrome_json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[{"name":"formPost","versions":[1]}]}}"#,
            );
        }
        ResponseTemplate::new(404)
    }
}

struct Fixture {
    state: AppState,
    app: App,
    _navidrome: MockServer,
    artwork: MockServer,
}

async fn fixture(with_server: bool) -> Fixture {
    let navidrome = MockServer::start().await;
    Mock::given(any()).respond_with(Navidrome).mount(&navidrome).await;
    let artwork = MockServer::start().await;
    let mut settings: AppSettings = settings(&navidrome.uri());
    if with_server {
        settings.metadata.animated_artwork_url = artwork.uri();
    }
    let state = state_with(settings, |_| {});
    Fixture {
        app: app(&state),
        state,
        _navidrome: navidrome,
        artwork,
    }
}

async fn artwork_has(server: &MockServer, artist: &str, album: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/artwork/search"))
        .and(query_param("artist", artist))
        .and(query_param("album", album))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": SQUARE, "url_tall": TALL, "artist": artist, "album": album, "isCached": true,
        })))
        .mount(server)
        .await;
}

fn extension_names(envelope: &Value) -> Vec<String> {
    envelope["openSubsonicExtensions"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|e| e["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn the_extension_is_listed_only_while_a_server_is_set() {
    let off = fixture(false).await;
    let reply = get(
        &off.app,
        "/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=octo",
    )
    .await;
    assert!(!extension_names(&reply.envelope()).contains(&"octoAnimatedArtwork".to_string()));

    let on = fixture(true).await;
    let reply = get(
        &on.app,
        "/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=octo",
    )
    .await;
    let envelope = reply.envelope();
    let names = extension_names(&envelope);
    assert!(names.contains(&"octoAnimatedArtwork".to_string()), "{envelope}");
    assert!(names.contains(&"formPost".to_string()));
}

#[tokio::test]
async fn get_animated_artwork_by_artist_and_album() {
    let t = fixture(true).await;
    artwork_has(&t.artwork, "Grimes", "Art Angels").await;
    let reply = get(
        &t.app,
        &format!(
            "/rest/getAnimatedArtwork.view?f=json&artist=Grimes&album=Art%20Angels&{}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "ok", "{envelope}");
    // Only links through this server, relative to its address: Apple's never leave Octo.
    let square = envelope["animatedArtwork"]["url"].as_str().expect("url");
    let tall = envelope["animatedArtwork"]["urlTall"].as_str().expect("urlTall");
    assert!(
        square.starts_with("animated-artwork/") && square.ends_with("/square.m3u8"),
        "{square}"
    );
    assert!(
        tall.starts_with("animated-artwork/") && tall.ends_with("/tall.m3u8"),
        "{tall}"
    );
    assert_eq!(
        square.trim_end_matches("square.m3u8"),
        tall.trim_end_matches("tall.m3u8")
    );
    assert!(!reply.text().contains("itunes.apple.com"), "{envelope}");
}

#[tokio::test]
async fn an_album_with_no_animated_cover_answers_ok_without_one() {
    let t = fixture(true).await;
    Mock::given(path("/api/v1/artwork/search"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&t.artwork)
        .await;
    let reply = get(
        &t.app,
        &format!(
            "/rest/getAnimatedArtwork.view?f=json&artist=Grimes&album=Halfaxa&{}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "ok", "{envelope}");
    assert!(envelope.get("animatedArtwork").is_none(), "{envelope}");
}

#[tokio::test]
async fn get_animated_artwork_needs_a_sign_in_and_a_server() {
    let t = fixture(true).await;
    let reply = get(
        &t.app,
        &format!(
            "/rest/getAnimatedArtwork.view?f=json&artist=Grimes&album=Visions&{}",
            auth("alice", "bad", "octo")
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "failed");
    assert!(t.artwork.received_requests().await.unwrap_or_default().is_empty());

    let off = fixture(false).await;
    let reply = get(
        &off.app,
        &format!(
            "/rest/getAnimatedArtwork.view?f=json&artist=Grimes&album=Visions&{}",
            auth("alice", "good", "octo")
        ),
    )
    .await;
    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "failed");
    assert_eq!(
        envelope["error"]["message"],
        "Animated artwork is not set up on this server"
    );
}

fn album_answer(artist: &str, album: &str) -> Response {
    let body = json!({ "subsonic-response": {
        "status": "ok", "version": "1.16.1",
        "album": { "id": "al-1", "name": album, "artist": artist, "song": [] },
    }});
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.to_string().len())
        .body(Body::from(body.to_string()))
        .expect("a response")
}

async fn read(response: Response) -> Value {
    let bytes = response.into_body().collect().await.expect("a body").to_bytes();
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn get_album_carries_the_animated_cover_when_there_is_one() {
    let t = fixture(true).await;
    artwork_has(&t.artwork, "Grimes", "Art Angels").await;
    let answer = read(with_album_artwork(&t.state, album_answer("Grimes", "Art Angels")).await).await;
    let album = &answer["subsonic-response"]["album"];
    let url = album["animatedArtwork"]["url"].as_str().expect("url");
    assert!(
        url.starts_with("animated-artwork/") && url.ends_with("/square.m3u8"),
        "{url}"
    );
    let tall = album["animatedArtwork"]["urlTall"].as_str().expect("urlTall");
    assert!(tall.ends_with("/tall.m3u8"), "{tall}");
    assert!(!answer.to_string().contains("itunes.apple.com"), "{answer}");
    assert_eq!(album["name"], "Art Angels");
}

#[tokio::test]
async fn get_album_is_unchanged_without_a_server_or_a_cover() {
    let off = fixture(false).await;
    let answer = read(with_album_artwork(&off.state, album_answer("Grimes", "Visions")).await).await;
    assert!(
        answer["subsonic-response"]["album"]
            .get("animatedArtwork")
            .is_none()
    );

    let t = fixture(true).await;
    Mock::given(path("/api/v1/artwork/search"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&t.artwork)
        .await;
    let answer = read(with_album_artwork(&t.state, album_answer("Grimes", "Visions")).await).await;
    assert!(
        answer["subsonic-response"]["album"]
            .get("animatedArtwork")
            .is_none()
    );
    assert_eq!(answer["subsonic-response"]["album"]["name"], "Visions");
}
