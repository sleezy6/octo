use reqwest::Url;
use wiremock::matchers::{any, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{address_of, allowed, file_name, rewrite_playlist};
use crate::controllers::subsonic::test_app::*;
use crate::services::cover_art::AnimatedArtwork;

const MASTER: &str = "#EXTM3U
#EXT-X-VERSION:7
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=548783,URI=\"https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/ab/P1_iframes.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=768x768
https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/47/P1_video_768x768.m3u8
";

const VARIANT: &str = "#EXTM3U
#EXT-X-TARGETDURATION:3
#EXT-X-MAP:URI=\"P1_video_768x768-.mp4\",BYTERANGE=\"896@0\"
#EXTINF:2.83333,
#EXT-X-BYTERANGE:662307@896
P1_video_768x768-.mp4
#EXT-X-ENDLIST
";

fn decode(name: &str) -> String {
    address_of(name).expect("a relayed name").to_string()
}

#[test]
fn a_top_playlist_names_every_address_relative_to_itself() {
    let base =
        Url::parse("https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/43/P1_default.m3u8").unwrap();
    let out = rewrite_playlist(MASTER, &base);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "#EXTM3U");
    // The URI attribute of a tag.
    let iframe = lines[2];
    let value = iframe.split("URI=\"").nth(1).unwrap().trim_end_matches('"');
    assert!(value.ends_with(".m3u8"), "{iframe}");
    assert_eq!(
        decode(value),
        "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/ab/P1_iframes.m3u8"
    );
    assert!(iframe.starts_with("#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=548783,URI=\""));
    // A URI line.
    assert_eq!(
        decode(lines[4]),
        "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/47/P1_video_768x768.m3u8"
    );
    assert!(!out.contains("https://"), "{out}");
}

#[test]
fn a_variant_resolves_its_relative_files_and_keeps_its_byte_ranges() {
    let base =
        Url::parse("https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/47/P1_video_768x768.m3u8")
            .unwrap();
    let out = rewrite_playlist(VARIANT, &base);
    let lines: Vec<&str> = out.lines().collect();
    let map = lines[2];
    let value = map.split("URI=\"").nth(1).unwrap().split('"').next().unwrap();
    assert!(value.ends_with(".mp4"));
    assert_eq!(
        decode(value),
        "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/47/P1_video_768x768-.mp4"
    );
    assert!(map.ends_with(",BYTERANGE=\"896@0\""), "{map}");
    assert_eq!(lines[4], "#EXT-X-BYTERANGE:662307@896");
    assert_eq!(
        decode(lines[5]),
        "https://mvod.itunes.apple.com/itunes-assets/HLSMusic112/v4/47/P1_video_768x768-.mp4"
    );
    assert_eq!(lines[6], "#EXT-X-ENDLIST");
}

#[test]
fn only_apples_video_assets_are_relayed() {
    let ok = Url::parse("https://mvod.itunes.apple.com/itunes-assets/a/b.mp4").unwrap();
    assert!(allowed(&ok, None));
    for bad in [
        "http://mvod.itunes.apple.com/itunes-assets/a/b.mp4",
        "https://evil.example.com/itunes-assets/a/b.mp4",
        "https://mvod.itunes.apple.com/other/b.mp4",
        "https://mvod.itunes.apple.com.evil.example.com/itunes-assets/b.mp4",
    ] {
        assert!(!allowed(&Url::parse(bad).unwrap(), None), "{bad}");
    }
}

struct Relay {
    app: crate::http::pipeline::App,
    apple: MockServer,
    key: String,
    origin: String,
}

async fn relay_fixture() -> Relay {
    let apple = MockServer::start().await;
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&navidrome)
        .await;
    let origin = apple.uri();
    let mut settings = settings(&navidrome.uri());
    settings.metadata.animated_artwork_url = "http://artwork.invalid:8080".into();
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = std::sync::Arc::new(octo_core::settings::SettingsStore::for_tests(settings.clone()));
    let artwork =
        AnimatedArtwork::new(store, dir.path().join("animated-artwork.json")).with_relay_test_origin(&origin);
    let key = artwork.seed(
        "Linkin Park",
        "Living Things",
        Some(&format!("{origin}/itunes-assets/v4/43/P1_default.m3u8")),
        None,
    );
    let artwork = std::sync::Arc::new(artwork);
    let state = state_with(settings, |inner| inner.animated_artwork = artwork);
    std::mem::forget(dir);
    Relay {
        app: app(&state),
        apple,
        key,
        origin,
    }
}

#[tokio::test]
async fn the_square_playlist_comes_back_rewritten_and_its_video_relayed_with_its_range() {
    let r = relay_fixture().await;
    let master = MASTER.replace("https://mvod.itunes.apple.com", &r.origin);
    Mock::given(method("GET"))
        .and(path("/itunes-assets/v4/43/P1_default.m3u8"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(master.into_bytes(), "application/vnd.apple.mpegurl"),
        )
        .mount(&r.apple)
        .await;
    Mock::given(method("GET"))
        .and(path("/itunes-assets/HLSMusic112/v4/47/P1_video_768x768-.mp4"))
        .and(header("range", "bytes=896-663202"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 896-663202/9000000")
                .set_body_raw(b"video bytes".to_vec(), "video/mp4"),
        )
        .mount(&r.apple)
        .await;

    let reply = get(&r.app, &format!("/animated-artwork/{}/square.m3u8", r.key)).await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.header("content-type"),
        Some("application/vnd.apple.mpegurl")
    );
    let text = reply.text();
    assert!(!text.contains(&r.origin), "{text}");

    let segment = Url::parse(&format!(
        "{}/itunes-assets/HLSMusic112/v4/47/P1_video_768x768-.mp4",
        r.origin
    ))
    .unwrap();
    let reply = send(
        &r.app,
        axum::http::Method::GET,
        &format!("/animated-artwork/{}/{}", r.key, file_name(&segment)),
        &[("range", "bytes=896-663202")],
        axum::body::Body::empty(),
    )
    .await;
    assert_eq!(reply.status, 206);
    assert_eq!(reply.header("content-range"), Some("bytes 896-663202/9000000"));
    assert_eq!(reply.text(), "video bytes");
}

#[tokio::test]
async fn an_unknown_key_or_a_foreign_address_is_not_relayed() {
    let r = relay_fixture().await;
    let reply = get(
        &r.app,
        "/animated-artwork/0123456789abcdef0123456789abcdef/square.m3u8",
    )
    .await;
    assert_eq!(reply.status, 404);
    // No tall cover was found for the seeded album.
    let reply = get(&r.app, &format!("/animated-artwork/{}/tall.m3u8", r.key)).await;
    assert_eq!(reply.status, 404);
    let foreign = Url::parse("https://example.com/itunes-assets/x.mp4").unwrap();
    let reply = get(
        &r.app,
        &format!("/animated-artwork/{}/{}", r.key, file_name(&foreign)),
    )
    .await;
    assert_eq!(reply.status, 404);
    assert!(r.apple.received_requests().await.unwrap_or_default().is_empty());
}
