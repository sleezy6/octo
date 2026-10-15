use std::sync::Arc;

use octo_core::settings::{AppSettings, MetadataSettings, SettingsStore};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const SQUARE: &str = "https://mvod.itunes.apple.com/itunes-assets/square.m3u8";
const TALL: &str = "https://mvod.itunes.apple.com/itunes-assets/tall.m3u8";

fn settings(url: &str) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        metadata: MetadataSettings {
            animated_artwork_url: url.into(),
            ..Default::default()
        },
        ..Default::default()
    }))
}

fn service(url: &str, dir: &tempfile::TempDir) -> Arc<AnimatedArtwork> {
    Arc::new(AnimatedArtwork::new(
        settings(url),
        dir.path().join("animated-artwork.json"),
    ))
}

async fn answers(server: &MockServer, artist: &str, album: &str, reply: ResponseTemplate, times: u64) {
    Mock::given(method("GET"))
        .and(path("/api/v1/artwork/search"))
        .and(query_param("artist", artist))
        .and(query_param("album", album))
        .respond_with(reply)
        .expect(times)
        .mount(server)
        .await;
}

fn found(artist: &str, album: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "url": SQUARE, "url_tall": TALL, "artist": artist, "album": album, "isCached": true,
    }))
}

#[tokio::test]
async fn with_no_server_set_nothing_is_asked() {
    let dir = tempfile::tempdir().unwrap();
    let artwork = service("", &dir);
    assert!(!artwork.enabled());
    assert_eq!(
        artwork.find("Grimes", "Visions").await,
        AnimatedLookup::Unavailable
    );
}

#[tokio::test]
async fn a_found_cover_is_asked_once_and_kept_across_a_restart() {
    let server = MockServer::start().await;
    answers(
        &server,
        "Linkin Park",
        "Living Things",
        found("LINKIN PARK", "LIVING THINGS"),
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    let expected = AnimatedLookup::Found(AnimatedCover {
        url: Some(SQUARE.into()),
        url_tall: Some(TALL.into()),
    });
    assert_eq!(artwork.find("Linkin Park", "Living Things").await, expected);
    assert_eq!(artwork.find("Linkin Park", "Living Things").await, expected);

    let restarted = service(&server.uri(), &dir);
    assert_eq!(restarted.known("Linkin Park", "Living Things"), Some(expected));
}

#[tokio::test]
async fn no_animated_cover_is_remembered_too() {
    let server = MockServer::start().await;
    answers(
        &server,
        "Grimes",
        "Halfaxa",
        ResponseTemplate::new(404).set_body_json(json!({ "message": "No animated artwork found." })),
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    assert_eq!(artwork.find("Grimes", "Halfaxa").await, AnimatedLookup::None);
    assert_eq!(artwork.find("Grimes", "Halfaxa").await, AnimatedLookup::None);
}

#[tokio::test]
async fn a_server_that_is_not_ready_is_asked_again_next_time() {
    let server = MockServer::start().await;
    answers(&server, "Grimes", "Visions", ResponseTemplate::new(503), 2).await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    assert_eq!(
        artwork.find("Grimes", "Visions").await,
        AnimatedLookup::Unavailable
    );
    assert_eq!(
        artwork.find("Grimes", "Visions").await,
        AnimatedLookup::Unavailable
    );
    assert_eq!(artwork.known("Grimes", "Visions"), None);
}

#[tokio::test]
async fn another_artists_record_is_not_this_albums_cover() {
    // The server's two-way substring matching can answer "Visions" by Grimes with Gary
    // Grimes' "Starhand Visions".
    let server = MockServer::start().await;
    answers(
        &server,
        "Grimes",
        "Visions",
        found("Gary Grimes", "Starhand Visions"),
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    assert_eq!(artwork.find("Grimes", "Visions").await, AnimatedLookup::None);
}

#[tokio::test]
async fn an_edition_of_the_same_album_counts() {
    let server = MockServer::start().await;
    answers(
        &server,
        "Gorillaz",
        "Plastic Beach",
        found("Gorillaz", "Plastic Beach (Deluxe Version)"),
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    assert!(matches!(
        artwork.find("Gorillaz", "Plastic Beach").await,
        AnimatedLookup::Found(_)
    ));
}

#[tokio::test]
async fn only_https_streams_are_passed_on() {
    let server = MockServer::start().await;
    answers(
        &server,
        "Grimes",
        "Art Angels",
        ResponseTemplate::new(200).set_body_json(json!({
            "url": "http://example.com/a.m3u8", "url_tall": TALL,
            "artist": "Grimes", "album": "Art Angels",
        })),
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    assert_eq!(
        artwork.find("Grimes", "Art Angels").await,
        AnimatedLookup::Found(AnimatedCover {
            url: None,
            url_tall: Some(TALL.into()),
        })
    );
}

#[tokio::test]
async fn apps_asking_at_once_share_one_question() {
    let server = MockServer::start().await;
    answers(
        &server,
        "Grimes",
        "Miss Anthropocene",
        found("Grimes", "Miss Anthropocene").set_delay(std::time::Duration::from_millis(200)),
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    let (a, b) = tokio::join!(
        artwork.find("Grimes", "Miss Anthropocene"),
        artwork.find("Grimes", "Miss Anthropocene"),
    );
    assert!(matches!(a, AnimatedLookup::Found(_)));
    assert_eq!(a, b);
}

#[tokio::test]
async fn a_short_wait_gives_up_but_the_answer_is_kept_for_next_time() {
    let server = MockServer::start().await;
    answers(
        &server,
        "Grimes",
        "Geidi Primes",
        found("Grimes", "Geidi Primes").set_delay(std::time::Duration::from_millis(300)),
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    assert_eq!(
        artwork
            .find_within("Grimes", "Geidi Primes", std::time::Duration::from_millis(20))
            .await,
        AnimatedLookup::Unavailable
    );
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(matches!(
        artwork.known("Grimes", "Geidi Primes"),
        Some(AnimatedLookup::Found(_))
    ));
}

/// Many answers back at once (an app opening a page of albums) save one at a time, and the file
/// ends up holding every one of them: no save writes over another's temporary file, and no older
/// list lands over a newer one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answers_saved_at_once_all_reach_the_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/artwork/search"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let artwork = service(&server.uri(), &dir);
    let asks: Vec<_> = (0..40)
        .map(|n| {
            let artwork = artwork.clone();
            tokio::spawn(async move { artwork.find("Artist", &format!("Album {n}")).await })
        })
        .collect();
    for ask in asks {
        assert_eq!(ask.await.expect("joins"), AnimatedLookup::None);
    }
    let restarted = service(&server.uri(), &dir);
    for n in 0..40 {
        assert_eq!(
            restarted.known("Artist", &format!("Album {n}")),
            Some(AnimatedLookup::None),
            "album {n} was saved"
        );
    }
    assert!(!dir.path().join("animated-artwork.json.tmp").exists());
}
