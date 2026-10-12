//! `Octo.Tests.DeezerResolverTests`, for the parts of the resolver ported here, against the
//! mock Deezer of `super::test_support`.

use bytes::Bytes;
use futures::TryStreamExt;

use super::*;
use crate::services::deezer::test_support::{Fixture, Scenario, deezer};

async fn read(opened: DeezerOpenStream) -> Vec<u8> {
    let chunks: Vec<Bytes> = opened.audio_stream.try_collect().await.expect("the song reads");
    chunks.concat()
}

#[tokio::test]
async fn playback_returns_requested_plaintext_range() {
    for (range, status, content_range, expected) in [
        (Some("bytes=2-5"), 206, Some("bytes 2-5/12"), "2345"),
        (Some("bytes=9-"), 206, Some("bytes 9-11/12"), "9ab"),
        (Some("bytes=-3"), 206, Some("bytes 9-11/12"), "9ab"),
        (Some("bytes=12-"), 416, Some("bytes */12"), ""),
        (Some("bytes=-0"), 416, Some("bytes */12"), ""),
        (None, 200, None, "0123456789ab"),
    ] {
        let fixture = Fixture::new(Scenario::default()).await;
        let opened = fixture
            .open(range)
            .await
            .unwrap_or_else(|| panic!("{range:?} opens"));
        assert_eq!(opened.status_code, status, "{range:?}");
        assert_eq!(opened.content_range.as_deref(), content_range, "{range:?}");
        assert_eq!(opened.content_type, "audio/mpeg", "{range:?}");
        assert_eq!(opened.content_length, Some(expected.len() as u64), "{range:?}");
        assert_eq!(read(opened).await, expected.as_bytes(), "{range:?}");

        let seen = fixture.seen.lock();
        assert_eq!(seen.media_bodies.len(), 1, "{range:?}");
        assert!(seen.media_bodies[0].contains("\"type\":\"FULL\""), "{range:?}");
        assert!(!seen.media_bodies[0].contains("FLAC"), "{range:?}");
        assert!(seen.media_bodies[0].contains("MP3_320"), "{range:?}");
    }
}

#[tokio::test]
async fn seeking_aligns_cdn_range_and_keeps_absolute_stripe_index() {
    for ignore_cdn_range in [false, true] {
        let payload: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let fixture = Fixture::new(Scenario {
            payload: payload.clone(),
            ignore_range: ignore_cdn_range,
            ..Default::default()
        })
        .await;
        let opened = fixture
            .open(Some("bytes=2301-4198"))
            .await
            .expect("the range opens");
        assert_eq!(opened.content_length, Some(1898));
        assert_eq!(opened.content_range.as_deref(), Some("bytes 2301-4198/5000"));
        assert_eq!(read(opened).await, payload[2301..4199], "{ignore_cdn_range}");
        assert_eq!(
            fixture.seen.lock().cdn_ranges.last().map(String::as_str),
            Some("bytes=2048-")
        );
    }
}

#[tokio::test]
async fn a_whole_song_is_decrypted() {
    let payload: Vec<u8> = (0..9000).map(|i| (i % 249) as u8).collect();
    let fixture = Fixture::new(Scenario {
        payload: payload.clone(),
        ..Default::default()
    })
    .await;
    let opened = fixture.open(None).await.expect("the song opens");
    assert_eq!(opened.content_length, Some(9000));
    assert_eq!(read(opened).await, payload);
}

#[tokio::test]
async fn expired_primary_uses_fallback_account_with_its_own_tokens() {
    let fixture = Fixture::with_accounts(
        Scenario {
            reject_primary: true,
            ..Default::default()
        },
        "primary",
        Some("fallback"),
    )
    .await;
    drop(fixture.open(None).await.expect("the fallback account opens it"));
    let seen = fixture.seen.lock();
    assert_eq!(seen.auth_cookies, ["arl=primary", "arl=fallback"]);
    assert!(
        seen.page_cookies
            .iter()
            .all(|cookie| cookie == "arl=fallback; sid=fallback-session")
    );
    assert_eq!(seen.media_bodies.len(), 1);
    assert!(seen.media_bodies[0].contains("fallback-license"));
    assert!(seen.media_cookies.iter().all(String::is_empty));
}

#[tokio::test]
async fn regional_fallback_track_uses_its_own_session_bound_token() {
    let payload: Vec<u8> = (0..3000).map(|i| (i % 241) as u8).collect();
    let fixture = Fixture::new(Scenario {
        fallback_track: true,
        payload: payload.clone(),
        ..Default::default()
    })
    .await;
    let opened = fixture.open(None).await.expect("the fallback track opens");
    // The bytes are the other track's, so the key is too.
    assert_eq!(read(opened).await, payload);
    let seen = fixture.seen.lock();
    assert_eq!(seen.page_ids, ["42", "43"]);
    assert!(
        seen.media_bodies
            .last()
            .expect("a media call")
            .contains("token-43")
    );
}

#[tokio::test]
async fn fallback_with_different_recording_version_is_rejected() {
    let fixture = Fixture::new(Scenario {
        fallback_track: true,
        wrong_fallback_version: true,
        ..Default::default()
    })
    .await;
    assert!(fixture.open(None).await.is_none());
    let seen = fixture.seen.lock();
    assert_eq!(seen.page_ids, ["42", "43"]);
    assert_eq!(seen.media_bodies.len(), 1);
    assert!(seen.media_bodies[0].contains("token-42"));
}

#[tokio::test]
async fn missing_fallback_id_and_isrc_use_readable_catalog_match_without_changing_version() {
    let fixture = Fixture::new(Scenario {
        fallback_track: true,
        no_fallback_id: true,
        ..Default::default()
    })
    .await;
    drop(fixture.open(None).await.expect("the catalog's copy opens"));
    let seen = fixture.seen.lock();
    assert_eq!(seen.page_ids, ["42", "44"]);
    assert!(
        seen.media_bodies
            .last()
            .expect("a media call")
            .contains("token-44")
    );
}

#[tokio::test]
async fn expired_cdn_url_is_evicted_and_resolved_again() {
    let fixture = Fixture::new(Scenario {
        reject_first_url: true,
        ..Default::default()
    })
    .await;
    drop(fixture.open(None).await.expect("the second address opens"));
    let seen = fixture.seen.lock();
    assert_eq!(seen.auth_cookies, ["arl=primary", "arl=primary"]);
    assert_eq!(seen.media_bodies.len(), 2);
}

#[tokio::test]
async fn account_specific_cdn_failure_uses_fallback_account_after_refresh() {
    let fixture = Fixture::with_accounts(
        Scenario {
            reject_primary_cdn: true,
            ..Default::default()
        },
        "primary",
        Some("fallback"),
    )
    .await;
    drop(fixture.open(None).await.expect("the fallback account opens it"));
    let seen = fixture.seen.lock();
    assert_eq!(seen.auth_cookies, ["arl=primary", "arl=primary", "arl=fallback"]);
    assert!(
        seen.media_bodies
            .last()
            .expect("a media call")
            .contains("fallback-license")
    );
}

#[tokio::test]
async fn a_media_address_is_reused_for_the_next_read() {
    let fixture = Fixture::new(Scenario::default()).await;
    drop(fixture.open(None).await.expect("opens"));
    drop(fixture.open(Some("bytes=3-")).await.expect("opens again"));
    assert_eq!(fixture.seen.lock().media_bodies.len(), 1);
}

#[tokio::test]
async fn saved_credentials_apply_to_next_request_without_restart() {
    let fixture = Fixture::new(Scenario::default()).await;
    drop(fixture.open(None).await.expect("opens"));
    fixture.settings.set(deezer("changed", None));
    drop(fixture.open(None).await.expect("opens with the new account"));
    let seen = fixture.seen.lock();
    assert_eq!(seen.auth_cookies, ["arl=primary", "arl=changed"]);
    assert!(
        seen.media_bodies
            .last()
            .expect("a media call")
            .contains("changed-license")
    );
}

#[tokio::test]
async fn an_arl_that_is_not_letters_and_digits_is_never_sent() {
    let fixture = Fixture::with_accounts(Scenario::default(), "abc; sid=stolen", None).await;
    assert!(fixture.open(None).await.is_none());
    assert!(!fixture.resolver.is_available().await);
    assert!(fixture.seen.lock().auth_cookies.is_empty());
}

/// The dashboard's account health: the plan and what it gives when the sign-in says, a refused
/// ARL told apart from Deezer not answering, and a fresh check that signs in again.
#[tokio::test]
async fn account_health_names_the_plan_and_tells_refused_from_unanswered() {
    let fixture = Fixture::with_accounts(
        Scenario {
            reject_primary: true,
            fallback_unanswered: true,
            ..Default::default()
        },
        "primary",
        Some("fallback"),
    )
    .await;
    let (health, _) = fixture.resolver.account_health(false).await;
    assert_eq!(
        health
            .iter()
            .map(|a| (a.signed_in, a.problem))
            .collect::<Vec<_>>(),
        [(false, Some(ACCOUNT_REFUSED)), (false, Some(ACCOUNT_UNREACHABLE))]
    );
    // An ARL that cannot be one is refused without asking Deezer.
    let asked = fixture.seen.lock().auth_cookies.len();
    fixture.settings.set(deezer("not an arl!", None));
    let (health, _) = fixture.resolver.account_health(false).await;
    assert_eq!(health[0].problem, Some(ACCOUNT_REFUSED));
    assert_eq!(fixture.seen.lock().auth_cookies.len(), asked);

    let fixture = Fixture::new(Scenario {
        plan: Some(("Deezer Premium", "US", false, true)),
        ..Default::default()
    })
    .await;
    let (health, first) = fixture.resolver.account_health(false).await;
    assert_eq!(
        health,
        [DeezerAccountHealth {
            signed_in: true,
            problem: None,
            offer: Some("Deezer Premium".into()),
            country: Some("US".into()),
            lossless: Some(false),
            high_quality: Some(true),
        }]
    );
    // Kept for the next asker; a fresh check signs in again.
    fixture.resolver.account_health(false).await;
    assert_eq!(fixture.seen.lock().auth_cookies.len(), 1);
    let (_, second) = fixture.resolver.account_health(true).await;
    assert_eq!(fixture.seen.lock().auth_cookies.len(), 2);
    assert!(second >= first);

    // A sign-in that says nothing about the plan leaves it unknown.
    let plain = Fixture::new(Scenario::default()).await;
    let (health, _) = plain.resolver.account_health(false).await;
    assert_eq!(
        (health[0].signed_in, &health[0].offer, health[0].lossless),
        (true, &None, None)
    );
}

/// The account watch speaks when something changes, and only then.
#[test]
fn the_account_watch_warns_once_when_an_account_fails_and_notes_its_return() {
    // The first check: silence when all is well, a warning when it is not.
    assert_eq!(account_change(0, None, None), None);
    let (warning, message) = account_change(0, None, Some(ACCOUNT_REFUSED)).expect("said");
    assert!(
        warning && message.starts_with("Deezer refuses the ARL"),
        "{message}"
    );
    // Still failing the same way: nothing more.
    assert_eq!(
        account_change(0, Some(Some(ACCOUNT_REFUSED)), Some(ACCOUNT_REFUSED)),
        None
    );
    // Deezer not answering is not an expired ARL.
    let (warning, message) = account_change(1, Some(None), Some(ACCOUNT_UNREACHABLE)).expect("said");
    assert!(warning && message.contains("did not answer") && message.contains("fallback ARL"));
    // Back again.
    assert_eq!(
        account_change(1, Some(Some(ACCOUNT_UNREACHABLE)), None),
        Some((false, "Deezer signs the fallback ARL in again.".to_string()))
    );
}

#[tokio::test]
async fn the_account_check_says_which_accounts_sign_in_and_is_asked_once() {
    let fixture = Fixture::with_accounts(
        Scenario {
            reject_primary: true,
            ..Default::default()
        },
        "primary",
        Some("fallback"),
    )
    .await;
    assert_eq!(fixture.resolver.check_accounts().await, [false, true]);
    assert_eq!(fixture.resolver.check_accounts().await, [false, true]);
    assert_eq!(fixture.seen.lock().auth_cookies.len(), 2);

    // Other accounts are checked afresh.
    fixture.settings.set(deezer("fallback", None));
    assert_eq!(fixture.resolver.check_accounts().await, [true]);
    assert_eq!(fixture.seen.lock().auth_cookies.len(), 3);
}

#[tokio::test]
async fn no_account_means_not_configured() {
    let fixture = Fixture::with_accounts(Scenario::default(), " ", None).await;
    assert!(!fixture.resolver.is_configured());
    assert!(fixture.open(None).await.is_none());
    assert!(fixture.seen.lock().auth_cookies.is_empty());
}

#[tokio::test]
async fn a_lesser_format_is_taken_when_the_account_is_not_given_the_one_asked_for() {
    let fixture = Fixture::new(Scenario {
        available_formats: vec!["MP3_128"],
        ..Default::default()
    })
    .await;
    let opened = fixture
        .resolver
        .open_stream("42", None, "FLAC")
        .await
        .expect("the MP3 opens");
    assert_eq!(opened.format, "MP3_128");
    assert_eq!(opened.content_type, "audio/mpeg");
}

#[tokio::test]
async fn permanent_copy_uses_flac_and_never_deletes_existing_destination() {
    let fixture = Fixture::new(Scenario::default()).await;
    let directory = tempfile::tempdir().expect("a temp folder");
    let destination = directory.path().join("nested").join("track");
    let destination = destination.to_str().expect("a path in UTF-8");

    let saved = fixture
        .resolver
        .download("42", destination)
        .await
        .expect("the download runs");
    assert_eq!(saved.as_deref(), Some(format!("{destination}.flac").as_str()));
    {
        let seen = fixture.seen.lock();
        assert_eq!(seen.media_bodies.len(), 1);
        assert!(seen.media_bodies[0].contains("FLAC"));
    }
    let path = format!("{destination}.flac");
    let original = std::fs::read(&path).expect("the saved file");
    assert_eq!(original, fixture.scenario.payload);

    assert!(fixture.resolver.download("42", destination).await.is_err());
    assert_eq!(std::fs::read(&path).expect("the saved file"), original);
}

#[tokio::test]
async fn a_track_id_that_is_not_a_number_is_never_asked_for() {
    let fixture = Fixture::new(Scenario::default()).await;
    for id in ["", "abc", "-4", "0", "4 2", "42/../43"] {
        assert!(
            fixture.resolver.open_stream(id, None, "MP3_320").await.is_none(),
            "{id}"
        );
    }
    assert!(fixture.seen.lock().page_ids.is_empty());
}

#[test]
fn one_range_in_bytes_is_read_and_anything_else_is_no_range() {
    for (header, expected) in [
        ("bytes=2-5", Some(ByteRange::From(2, Some(5)))),
        ("bytes=9-", Some(ByteRange::From(9, None))),
        ("bytes=-3", Some(ByteRange::Suffix(3))),
        (" Bytes = 0 - 0 ", Some(ByteRange::From(0, Some(0)))),
        ("bytes=5-2", None),
        ("bytes=0-1,4-5", None),
        ("items=0-1", None),
        ("bytes=-", None),
        ("bytes=a-b", None),
        ("", None),
    ] {
        assert_eq!(parse_single_range(header), expected, "{header}");
    }
}

/// A CDN that stops sending halfway ends the read with an error after the idle limit, instead
/// of leaving a download, a cache fill or a play waiting forever. The limit only runs while a
/// reader asks for the next piece.
#[tokio::test(start_paused = true)]
async fn a_stream_that_stops_sending_ends_with_an_error() {
    use futures::StreamExt;
    let first: std::io::Result<Bytes> = Ok(Bytes::from_static(b"first"));
    let stalled = futures::stream::iter([first]).chain(futures::stream::pending());
    let mut read = Box::pin(idle_limited(stalled, Duration::from_secs(60)));

    assert_eq!(read.next().await.expect("a piece").expect("ok"), "first");
    // Nobody asks for a while (a paused player): nothing is cut off.
    tokio::time::advance(Duration::from_secs(600)).await;
    let error = read.next().await.expect("an answer").expect_err("stalled");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(read.next().await.is_none(), "ended after the error");

    // A stream that keeps sending is passed through whole.
    let pieces: Vec<std::io::Result<Bytes>> =
        vec![Ok(Bytes::from_static(b"a")), Ok(Bytes::from_static(b"b"))];
    let whole: Vec<Bytes> = idle_limited(futures::stream::iter(pieces), Duration::from_secs(60))
        .map(|piece| piece.expect("ok"))
        .collect()
        .await;
    assert_eq!(whole, ["a", "b"]);
}
