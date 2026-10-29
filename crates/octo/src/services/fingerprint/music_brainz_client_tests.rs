//! The MusicBrainz client: its cache and escaping. Reading release details and candidate
//! sources is tested with the tagging code; the query builders are tested in
//! `octo_core::fingerprint::music_brainz_client`.

use wiremock::matchers::{any, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const NIGHT_AT_THE_OPERA: &str = r#"
{
  "id": "r-nato", "title": "A Night at the Opera", "status": "Official", "date": "1975-11-21", "country": "GB",
  "barcode": "077778949224", "quality": "normal",
  "label-info": [{"catalog-number": "EMTC 103", "label": {"id": "l-emi", "name": "EMI"}}],
  "release-group": {"id": "g-nato", "title": "A Night at the Opera", "primary-type": "Album",
    "secondary-types": [], "first-release-date": "1975-11-21"},
  "artist-credit": [{"name": "Queen", "joinphrase": "", "artist": {"id": "a-queen", "name": "Queen"}}],
  "media": [{"position": 1, "format": "12\" Vinyl", "track-count": 12,
    "tracks": [
      {"id": "t-1", "position": 1, "number": "A1", "title": "Death on Two Legs", "length": 223000,
       "recording": {"id": "rec-dotl", "title": "Death on Two Legs (Dedicated to...)", "length": 223000, "isrcs": ["GBUM71029604"]}},
      {"id": "t-11", "position": 11, "number": "B5", "title": "Bohemian Rhapsody", "length": 355000,
       "recording": {"id": "rec-br", "title": "Bohemian Rhapsody", "length": 355000, "isrcs": ["GBUM71029604", "GBUM71029605"]}}
    ]}],
  "genres": [{"name": "rock", "count": 12}, {"name": "progressive rock", "count": 5}, {"name": "glam rock", "count": 1}]
}
"#;

async fn client(answer: ResponseTemplate) -> (MusicBrainzClient, MockServer) {
    let server = MockServer::builder().start().await;
    Mock::given(any()).respond_with(answer).mount(&server).await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    (MusicBrainzClient::with_base_url(base), server)
}

async fn calls(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.to_string())
        .collect()
}

#[tokio::test]
async fn lookup_release_asks_once_then_answers_from_memory() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(NIGHT_AT_THE_OPERA)).await;

    let first = client
        .lookup_release("r-nato")
        .await
        .expect("no timeout")
        .expect("found");
    let second = client
        .lookup_release("r-nato")
        .await
        .expect("no timeout")
        .expect("found");

    assert!(Arc::ptr_eq(&first, &second));
    let calls = calls(&server).await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(
        "release/r-nato?inc=labels+release-groups+artist-credits+recordings+isrcs+genres&fmt=json"
    ));
    assert!(calls[0].contains("/ws/2/release/"));
    // MusicBrainz wants a User-Agent naming the application.
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests[0]
            .headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok()),
        Some(octo_user_agent::value())
    );
}

#[tokio::test]
async fn lookup_release_server_error_is_null_and_not_remembered() {
    let (client, server) = client(ResponseTemplate::new(503)).await;

    assert!(
        client
            .lookup_release("r-nato")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(
        client
            .lookup_release("r-nato")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert_eq!(calls(&server).await.len(), 2);
}

#[tokio::test]
async fn search_recordings_is_remembered_for_the_same_song() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(r#"{"recordings": []}"#)).await;

    let first = client
        .search_recordings("Portishead", "Glory Box (Live)", 300)
        .await
        .expect("no timeout");
    let second = client
        .search_recordings("Portishead", "Glory Box (Live)", 300)
        .await
        .expect("no timeout");

    assert!(first.is_some());
    assert!(second.is_some());
    assert_eq!(calls(&server).await.len(), 1);
}

/// The second request waits out the one-a-second gap.
#[tokio::test]
async fn requests_are_spaced_a_second_apart() {
    let (client, _server) =
        client(ResponseTemplate::new(200).set_body_string(r#"{"isrcs": ["GBUM71029604"]}"#)).await;

    let start = Instant::now();
    assert_eq!(
        client.fetch_isrcs("rec-1").await.expect("no timeout"),
        Some(vec!["GBUM71029604".to_string()])
    );
    client.fetch_isrcs("rec-2").await.expect("no timeout");
    assert!(Instant::now() - start >= Duration::from_millis(1100));
}

// ---- Where an artist is from, on the background lane ----------------------------------------

const ROSALIA: &str =
    r#"{"id":"a-rosalia","name":"Rosalía","country":"ES","area":{"name":"Spain","iso-3166-1-codes":["ES"]}}"#;

#[tokio::test]
async fn an_artist_lookup_reads_the_country_and_remembers_it() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(ROSALIA)).await;

    let first = client.artist_country("A-Rosalia").await.expect("no timeout");
    let second = client.artist_country("a-rosalia").await.expect("no timeout");

    assert_eq!(first.as_deref(), Some("ES"));
    assert_eq!(second.as_deref(), Some("ES"));
    let calls = calls(&server).await;
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].ends_with("/ws/2/artist/A-Rosalia?fmt=json"),
        "{}",
        calls[0]
    );
}

#[tokio::test]
async fn an_artist_search_that_could_not_be_asked_is_none_and_not_remembered() {
    let (client, server) = client(ResponseTemplate::new(503)).await;

    assert_eq!(
        client.search_artist_country("Rosalía").await.expect("no timeout"),
        None
    );
    assert_eq!(
        client.search_artist_country("Rosalía").await.expect("no timeout"),
        None
    );

    let calls = calls(&server).await;
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0].contains("/ws/2/artist/?query=artist%3A%22Rosal"),
        "{}",
        calls[0]
    );
}

#[tokio::test]
async fn background_lookups_step_aside_so_a_download_check_waits_one_gap_at_most() {
    let (client, _server) = client(ResponseTemplate::new(200).set_body_string(ROSALIA)).await;
    let client = Arc::new(client);
    let background = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            for i in 0..20 {
                let _ = client.artist_country(&format!("artist-{i}")).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A download being verified asks three questions in a row.
    let started = std::time::Instant::now();
    client.fetch_isrcs("rec-1").await.expect("no timeout");
    let first = started.elapsed();
    client.fetch_isrcs("rec-2").await.expect("no timeout");
    client.fetch_isrcs("rec-3").await.expect("no timeout");
    let all = started.elapsed();
    background.abort();

    let gap = MusicBrainzClient::MINIMUM_GAP;
    assert!(first < gap + Duration::from_millis(400), "first {first:?}");
    // One gap behind the background's last call, then one gap each: never behind its queue.
    assert!(all < gap * 3 + Duration::from_millis(900), "all {all:?}");
}

// ---- artists by name, and recording credits ------------------------------------------------

#[tokio::test]
async fn find_artists_named_asks_once_and_remembers_the_answer() {
    let body = r#"{"artists":[{"id":"a1","name":"Earth, Wind & Fire"},{"id":"a2","name":"Earth, Wind & Fire Experience"}]}"#;
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(body)).await;

    assert!(client.known_artists_named("Earth, Wind & Fire").is_none());
    let first = client
        .find_artists_named("Earth, Wind & Fire")
        .await
        .expect("no timeout")
        .expect("asked");
    let second = client
        .find_artists_named("EARTH, WIND & FIRE")
        .await
        .expect("no timeout")
        .expect("asked");

    assert_eq!(first.len(), 1);
    assert_eq!(second, first);
    assert_eq!(calls(&server).await.len(), 1);
    assert!(client.known_artists_named("earth, wind & fire").is_some());
}

/// A failed request is "could not ask", never "no such artist", and is not remembered.
#[tokio::test]
async fn find_artists_named_failure_is_none_and_not_remembered() {
    let (client, _server) = client(ResponseTemplate::new(503)).await;

    assert!(
        client
            .find_artists_named("Bizarrap")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(client.known_artists_named("Bizarrap").is_none());
}

#[tokio::test]
async fn recording_credits_answer_every_id_in_one_search() {
    let body = r#"{"recordings":[{"id":"11111111-2222-3333-4444-555555555555","artist-credit":[{"name":"Bizarrap","joinphrase":", "},{"name":"Rauw Alejandro"}]}]}"#;
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(body)).await;
    let ids = vec![
        "11111111-2222-3333-4444-555555555555".to_string(),
        "99999999-2222-3333-4444-555555555555".to_string(),
    ];

    let credits = client
        .recording_credits(&ids)
        .await
        .expect("no timeout")
        .expect("asked");
    let again = client
        .recording_credits(&ids)
        .await
        .expect("no timeout")
        .expect("asked");

    assert_eq!(credits[&ids[0]].len(), 2);
    assert_eq!(credits[&ids[0]][1].join_phrase, "");
    assert!(credits[&ids[1]].is_empty());
    assert_eq!(again, credits);
    assert_eq!(calls(&server).await.len(), 1);
}

#[test]
fn the_kept_answers_are_dropped_before_they_outgrow_their_bound() {
    let mut map: HashMap<String, u32> = (0..3).map(|i| (i.to_string(), i)).collect();
    make_room(&mut map, 1, 4);
    assert_eq!(map.len(), 3, "room for one more");
    make_room(&mut map, 2, 4);
    assert!(map.is_empty(), "no room for two: the old answers go");
    assert_eq!(KNOWN_MAX, 10_000);
}

// ---- codes ---------------------------------------------------------------------------------

fn recorded(name: &str) -> String {
    std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/musicbrainz/").to_owned() + name,
    )
    .expect("a recorded MusicBrainz answer")
}

fn json_answer(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "application/json")
}

/// A server answering the search by code and the code lookup for one code.
async fn code_server(
    code: &str,
    search: ResponseTemplate,
    lookup: ResponseTemplate,
) -> (MusicBrainzClient, MockServer) {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/recording"))
        .and(query_param("query", format!("isrc:{code}")))
        .respond_with(search)
        .mount(&server)
        .await;
    Mock::given(path(format!("/ws/2/isrc/{code}")))
        .respond_with(lookup)
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    (MusicBrainzClient::with_base_url(base), server)
}

fn ids(doc: &serde_json::Value) -> Vec<String> {
    doc["recordings"]
        .as_array()
        .expect("recordings")
        .iter()
        .map(|r| r["id"].as_str().expect("an id").to_string())
        .collect()
}

#[tokio::test]
async fn lookup_isrc_asks_a_search_by_code_and_reads_releases_and_their_groups() {
    let (client, server) = code_server(
        "GBA077700130",
        json_answer(recorded("isrc-search-GBA077700130.json")),
        ResponseTemplate::new(500),
    )
    .await;

    let IsrcLookup::Found(doc) = client.lookup_isrc("gb-a07-77-00130").await.expect("no timeout") else {
        panic!("found");
    };
    assert_eq!(doc["isrc"], "GBA077700130");
    assert_eq!(doc["recordings"].as_array().map(Vec::len), Some(3));
    let candidates = octo_core::tagging::candidate_sources::CandidateSources::from_isrc_lookup(&doc);
    assert!(
        candidates
            .iter()
            .any(|c| c.release_group_id.is_some() && c.primary_type.is_some() && c.track_number.is_some()),
        "{candidates:?}"
    );
    // Remembered: a second ask goes nowhere.
    client.lookup_isrc("GBA077700130").await.expect("no timeout");
    let asked = calls(&server).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].ends_with("/ws/2/recording?query=isrc:GBA077700130&limit=100&fmt=json"),
        "{asked:?}"
    );
    assert!(!asked[0].contains("release-groups"));
}

#[tokio::test]
async fn lookup_isrc_leaves_out_recordings_the_search_lists_without_the_code() {
    let answer = serde_json::json!({"recordings": [
        {"id": "with", "title": "Song", "isrcs": ["GBA077700130"]},
        {"id": "without", "title": "Song", "isrcs": ["USXXX0000001"]},
        {"id": "none", "title": "Song"}
    ]});
    let (client, _server) = code_server(
        "GBA077700130",
        json_answer(answer.to_string()),
        ResponseTemplate::new(500),
    )
    .await;
    let IsrcLookup::Found(doc) = client.lookup_isrc("GBA077700130").await.expect("no timeout") else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["with"]);
}

#[tokio::test]
async fn the_tagger_asks_a_code_once_and_a_code_nobody_knows_is_not_known_and_not_remembered() {
    let (client, server) = code_server(
        "ZZZZZ0000001",
        json_answer(recorded("search-empty.json")),
        ResponseTemplate::new(404).set_body_raw(recorded("isrc-not-found.json"), "application/json"),
    )
    .await;
    assert_eq!(
        client.lookup_isrc("ZZZZZ0000001").await.expect("no timeout"),
        IsrcLookup::NotKnown
    );
    assert_eq!(calls(&server).await.len(), 1, "the tagger asks the search only");
    assert_eq!(
        client.lookup_isrc("ZZZZZ0000001").await.expect("no timeout"),
        IsrcLookup::NotKnown
    );
    assert_eq!(calls(&server).await.len(), 2, "not known is not remembered");
}

#[tokio::test]
async fn a_code_answer_keeps_at_most_25_releases_per_recording_and_the_same_candidates() {
    let raw: serde_json::Value =
        serde_json::from_str(&recorded("isrc-search-USSM16401419.json")).expect("JSON");
    assert!(
        raw["recordings"][0]["releases"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
            > 25
    );
    let (client, _server) = code_server(
        "USSM16401419",
        json_answer(recorded("isrc-search-USSM16401419.json")),
        ResponseTemplate::new(500),
    )
    .await;
    let IsrcLookup::Found(doc) = client.lookup_isrc("USSM16401419").await.expect("no timeout") else {
        panic!("found");
    };
    for recording in doc["recordings"].as_array().expect("recordings") {
        assert!(recording["releases"].as_array().map(Vec::len).unwrap_or(0) <= 25);
    }
    let mut whole = raw.clone();
    whole
        .as_object_mut()
        .expect("an object")
        .insert("isrc".into(), serde_json::Value::String("USSM16401419".into()));
    use octo_core::tagging::candidate_sources::CandidateSources;
    assert_eq!(
        CandidateSources::from_isrc_lookup(&doc),
        CandidateSources::from_isrc_lookup(&whole)
    );
}

#[tokio::test]
async fn a_dj_mix_segment_the_code_lists_is_left_out() {
    let (client, server) = code_server(
        "USSM19803320",
        json_answer(recorded("search-empty.json")),
        json_answer(recorded("isrc-lookup-USSM19803320.json")),
    )
    .await;
    let IsrcLookup::Found(doc) = client
        .isrc_recordings_in_background("USSM19803320")
        .await
        .expect("no timeout")
    else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["384f304d-5539-4cbf-920b-cfa8e69f795e"]);
    // The code lookup alone: the search is not asked for the matcher.
    let asked = calls(&server).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].ends_with("/ws/2/isrc/USSM19803320?inc=artist-credits&fmt=json"),
        "{asked:?}"
    );
}

#[test]
fn a_dj_mix_segment_is_read_however_its_hyphen_is_written() {
    let said = |text: &str| serde_json::json!({ "disambiguation": text });
    for segment in [
        "part of \u{201c}NOW 80s\u{201d} DJ\u{2010}mix",
        "part of X DJ-mix",
        "part of X DJ mix",
        "dj mix",
    ] {
        assert!(is_dj_mix_segment(&said(segment)), "{segment}");
    }
    for take in [
        "2015 stereo mix",
        "remix",
        "original acoustic version, stereo mix",
        "",
    ] {
        assert!(!is_dj_mix_segment(&said(take)), "{take}");
    }
}

#[tokio::test]
async fn the_matcher_gets_every_recording_the_code_lookup_lists() {
    let lookup = serde_json::json!({"isrc": "GBA077700130", "recordings": [
        {"id": "a", "title": "Song"},
        {"id": "b", "title": "Song"}
    ]});
    let (client, server) = code_server(
        "GBA077700130",
        ResponseTemplate::new(500),
        json_answer(lookup.to_string()),
    )
    .await;
    let IsrcLookup::Found(doc) = client
        .isrc_recordings_in_background("GBA077700130")
        .await
        .expect("no timeout")
    else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["a", "b"]);
    // Remembered: asked once.
    client
        .isrc_recordings_in_background("GBA077700130")
        .await
        .expect("no timeout");
    let asked = calls(&server).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].ends_with("/ws/2/isrc/GBA077700130?inc=artist-credits&fmt=json"),
        "{asked:?}"
    );
}

#[tokio::test]
async fn the_matchers_code_lookup_tells_each_kind_of_answer() {
    let (busy, _a) = code_server(
        "GBA077700130",
        ResponseTemplate::new(500),
        ResponseTemplate::new(503),
    )
    .await;
    assert_eq!(
        busy.isrc_recordings_in_background("GBA077700130")
            .await
            .expect("no timeout"),
        IsrcLookup::Unanswered
    );
    let (refused, _b) = code_server(
        "GBA077700130",
        ResponseTemplate::new(500),
        ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
    )
    .await;
    assert_eq!(
        refused
            .isrc_recordings_in_background("GBA077700130")
            .await
            .expect("no timeout"),
        IsrcLookup::Refused
    );
    let (unknown, _c) = code_server(
        "ZZZZZ0000001",
        ResponseTemplate::new(500),
        ResponseTemplate::new(404).set_body_raw(recorded("isrc-not-found.json"), "application/json"),
    )
    .await;
    assert_eq!(
        unknown
            .isrc_recordings_in_background("ZZZZZ0000001")
            .await
            .expect("no timeout"),
        IsrcLookup::NotKnown
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_question_musicbrainz_refuses_is_refused_and_logged_as_a_warning() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let (client, server) = code_server(
        "GBA077700130",
        ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
        ResponseTemplate::new(500),
    )
    .await;
    assert_eq!(
        client.lookup_isrc("GBA077700130").await.expect("no timeout"),
        IsrcLookup::Refused
    );
    assert_eq!(calls(&server).await.len(), 1);
    let warnings = logs.at(tracing::Level::WARN);
    assert!(
        warnings
            .iter()
            .any(|line| line.contains("refused the code search question")
                && line.contains("not a valid inc parameter")),
        "{warnings:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_id_musicbrainz_calls_invalid_is_nothing_and_never_warned() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let (client, _server) = client(
        ResponseTemplate::new(400).set_body_raw(recorded("release-invalid-mbid.json"), "application/json"),
    )
    .await;
    assert!(
        client
            .lookup_release("not-an-mbid")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(
        logs.at(tracing::Level::WARN).is_empty(),
        "{:?}",
        logs.at(tracing::Level::WARN)
    );
}

#[tokio::test]
async fn a_busy_musicbrainz_is_unanswered_and_asked_again_next_time() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/recording"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/recording"))
        .respond_with(json_answer(recorded("isrc-search-GBA077700130.json")))
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let client = MusicBrainzClient::with_base_url(base);
    assert_eq!(
        client.lookup_isrc("GBA077700130").await.expect("no timeout"),
        IsrcLookup::Unanswered
    );
    assert!(matches!(
        client.lookup_isrc("GBA077700130").await.expect("no timeout"),
        IsrcLookup::Found(_)
    ));
}

#[tokio::test]
async fn the_matchers_code_questions_step_aside_so_a_download_check_waits_one_gap_at_most() {
    let (client, _server) = client(json_answer(recorded("search-empty.json"))).await;
    let client = Arc::new(client);
    // A download check has just asked: the gate's last call is a foreground one.
    client
        .fetch_isrcs("11111111-1111-4111-8111-111111111111")
        .await
        .expect("no timeout");
    let background = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            for i in 0..20 {
                let _ = client
                    .isrc_recordings_in_background(&format!("ZZZZZ00000{i:02}"))
                    .await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    client
        .fetch_isrcs("22222222-2222-4222-8222-222222222222")
        .await
        .expect("no timeout");
    let waited = started.elapsed();
    background.abort();

    let gap = MusicBrainzClient::MINIMUM_GAP;
    assert!(waited < gap + Duration::from_millis(400), "waited {waited:?}");
}

#[tokio::test]
async fn the_tagger_hears_each_kind_of_code_answer() {
    use octo_core::tagging::ReleaseLookup;

    let (busy, _a) = code_server(
        "GBA077700130",
        ResponseTemplate::new(503),
        ResponseTemplate::new(500),
    )
    .await;
    assert!(ReleaseLookup::lookup_isrc(&busy, "GBA077700130").await.is_err());

    let (refused, _b) = code_server(
        "GBA077700130",
        ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
        ResponseTemplate::new(500),
    )
    .await;
    assert!(
        ReleaseLookup::lookup_isrc(&refused, "GBA077700130")
            .await
            .is_err()
    );

    let (unknown, server) = code_server(
        "ZZZZZ0000001",
        json_answer(recorded("search-empty.json")),
        ResponseTemplate::new(500),
    )
    .await;
    assert_eq!(
        ReleaseLookup::lookup_isrc(&unknown, "ZZZZZ0000001")
            .await
            .expect("an answer"),
        None
    );
    assert_eq!(calls(&server).await.len(), 1);

    let (known, _c) = code_server(
        "GBA077700130",
        json_answer(recorded("isrc-search-GBA077700130.json")),
        ResponseTemplate::new(500),
    )
    .await;
    assert!(
        ReleaseLookup::lookup_isrc(&known, "GBA077700130")
            .await
            .expect("an answer")
            .is_some()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn each_kind_of_answer_is_read_and_only_a_refused_question_is_warned() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let cases: [(ResponseTemplate, &str); 7] = [
        (
            ResponseTemplate::new(404).set_body_raw(recorded("isrc-not-found.json"), "application/json"),
            "not known",
        ),
        (
            ResponseTemplate::new(400)
                .set_body_raw(recorded("release-invalid-mbid.json"), "application/json"),
            "not known",
        ),
        (ResponseTemplate::new(429), "busy"),
        (
            ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
            "refused",
        ),
        (ResponseTemplate::new(500), "busy"),
        (
            ResponseTemplate::new(200).set_body_raw("<html>not json</html>", "text/html"),
            "busy",
        ),
        (json_answer(recorded("search-empty.json")), "answer"),
    ];
    for (answer, expected) in cases {
        let (client, _server) = client(answer).await;
        let asked = client
            .ask(
                "recording?query=isrc:GBA077700130&limit=100&fmt=json",
                "code search",
            )
            .await
            .expect("no timeout");
        let kind = match asked {
            Asked::Answer(_) => "answer",
            Asked::NotKnown => "not known",
            Asked::Refused => "refused",
            Asked::Busy => "busy",
        };
        assert_eq!(kind, expected);
    }
    assert_eq!(
        logs.at(tracing::Level::WARN).len(),
        1,
        "{:?}",
        logs.at(tracing::Level::WARN)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_lookup_by_an_id_that_is_not_a_musicbrainz_id_is_nothing_whatever_musicbrainz_says() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let (client, _server) = client(
        ResponseTemplate::new(400)
            .set_body_raw(r#"{"error":"Something else entirely."}"#, "application/json"),
    )
    .await;
    assert!(
        client
            .lookup_release("not-an-mbid")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(asks_by_a_non_mbid("release/not-an-mbid?inc=labels&fmt=json"));
    assert!(!asks_by_a_non_mbid(
        "release/5e5c5a1c-b4fc-4ba2-9d1e-5f0e1bd1b9a2?inc=labels&fmt=json"
    ));
    assert!(!asks_by_a_non_mbid(
        "recording?query=isrc:GBA077700130&limit=100&fmt=json"
    ));
    assert!(!asks_by_a_non_mbid(
        "isrc/GBA077700130?inc=artist-credits&fmt=json"
    ));
    assert!(
        logs.at(tracing::Level::WARN).is_empty(),
        "{:?}",
        logs.at(tracing::Level::WARN)
    );
}

#[tokio::test]
async fn a_big_code_answer_keeps_25_releases_a_recording_and_drops_its_dj_mix_segment() {
    let raw: serde_json::Value =
        serde_json::from_str(&recorded("isrc-search-USSM19803320.json")).expect("JSON");
    let (client, _server) = code_server(
        "USSM19803320",
        json_answer(recorded("isrc-search-USSM19803320.json")),
        ResponseTemplate::new(500),
    )
    .await;
    let IsrcLookup::Found(doc) = client.lookup_isrc("USSM19803320").await.expect("no timeout") else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["384f304d-5539-4cbf-920b-cfa8e69f795e"]);
    assert_eq!(
        doc["recordings"][0]["releases"].as_array().map(Vec::len),
        Some(25)
    );

    // The candidates are those of the whole answer without the segment: the reader takes 25.
    let mut whole = raw.clone();
    let recordings = whole["recordings"].as_array_mut().expect("recordings");
    recordings.retain(|r| r["id"] != "4f6bae05-b451-4117-8703-f4fffa07322c");
    whole
        .as_object_mut()
        .expect("an object")
        .insert("isrc".into(), serde_json::Value::String("USSM19803320".into()));
    use octo_core::tagging::candidate_sources::CandidateSources;
    assert_eq!(
        CandidateSources::from_isrc_lookup(&doc),
        CandidateSources::from_isrc_lookup(&whole)
    );
}
