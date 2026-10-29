use axum::http::StatusCode;
use octo_core::settings::{AppSettings, GeneratedPlaylistSettings, SubsonicSettings};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::test_app::{app, get};
use crate::app::{AppState, TestServices};

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/apple-music/",
            $name,
            ".html"
        ))
    };
}

async fn apple() -> MockServer {
    let server = MockServer::start().await;
    for (at, genre, body) in [
        ("/new/top-charts/songs", None, fixture!("us_all")),
        ("/us/new/top-charts/songs", None, fixture!("us_all")),
        ("/us/new/top-charts/songs/", Some("18"), fixture!("us_18")),
        ("/us/new", None, fixture!("us_new")),
        ("/us/room/6818396128", None, fixture!("us_room_best_new")),
    ] {
        let mock = Mock::given(method("GET")).and(path(at));
        let mock = match genre {
            Some(genre) => mock.and(query_param("genreId", genre)),
            None => mock.and(query_param_is_missing("genreId")),
        };
        mock.respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/html"))
            .mount(&server)
            .await;
    }
    server
}

fn state(apple: &MockServer, sign_in: Option<&str>) -> AppState {
    AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                auto_detect_download_path: false,
                ..Default::default()
            },
            generated_playlists: GeneratedPlaylistSettings {
                charts: "new,18".into(),
                chart_country: "auto".into(),
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            apple_music_base_url: Some(apple.uri()),
            sign_in: sign_in.map(str::to_string),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn charts_lists_country_choices_and_last_fetches() {
    let apple = apple().await;
    let state = state(&apple, None);
    state.top_songs.chart("34").await;

    let reply = get(&app(state), "/api/admin/charts").await;

    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    assert_eq!(body["country"]["setting"], "auto");
    assert_eq!(body["country"]["detected"], "us");
    assert_eq!(body["country"]["used"], "us");
    assert_eq!(body["countries"].as_array().unwrap().len(), 172);
    assert_eq!(body["popularNow"], true);
    let choices = body["choices"].as_array().unwrap();
    assert_eq!(
        (choices[0]["id"].as_str(), choices[0]["on"].as_bool()),
        (Some("new"), Some(true))
    );
    assert_eq!(
        (choices[1]["id"].as_str(), choices[1]["on"].as_bool()),
        (Some("trending"), Some(false))
    );
    let hip_hop = choices.iter().find(|c| c["id"] == "18").unwrap();
    assert_eq!(
        (hip_hop["name"].as_str(), hip_hop["on"].as_bool()),
        (Some("Hip-Hop/Rap"), Some(true))
    );
    let lists = body["lists"].as_array().unwrap();
    let charts: Vec<&str> = lists.iter().filter_map(|l| l["chart"].as_str()).collect();
    assert_eq!(charts, vec!["34", "new", "18"]);
    assert_eq!(lists[0]["name"], "Popular right now");
    assert_eq!(lists[0]["source"], "apple");
    assert_eq!(lists[0]["songs"], 5);
    assert!(lists[0]["fetchedUtc"].as_str().is_some());
    assert!(lists[1]["source"].is_null(), "not fetched yet");
}

#[tokio::test]
async fn a_genre_the_country_lacks_is_not_in_country() {
    let apple = apple().await;
    let state = state(&apple, None);
    state.top_songs.chart("34").await;
    state.top_songs.set_menu(
        "us",
        vec![octo_core::charts::apple_pages::ChartGenre {
            id: "34".into(),
            name: "All Genres".into(),
        }],
    );

    let reply = get(&app(state), "/api/admin/charts").await;

    let body = reply.json();
    let lists = body["lists"].as_array().unwrap();
    let in_country = |chart: &str| lists.iter().find(|l| l["chart"] == chart).unwrap()["inCountry"].clone();
    assert_eq!(in_country("18"), false);
    assert_eq!(in_country("new"), true);
    assert_eq!(in_country("34"), true);
}

#[tokio::test]
async fn charts_needs_the_sign_in() {
    let apple = apple().await;
    let reply = get(&app(state(&apple, Some("required"))), "/api/admin/charts").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

// ---- each person's charts ----------------------------------------------------------------

fn played(id: &str) -> crate::services::listening::Play {
    crate::services::listening::Play {
        song_id: id.into(),
        cover_id: id.into(),
        artist: "Kavinsky".into(),
        title: format!("Song {id}"),
        album: "OutRun".into(),
        seconds: 240,
        is_local: true,
        played_at: chrono::Utc::now() - chrono::TimeDelta::hours(1),
        ..Default::default()
    }
}

async fn put_people(
    app: &crate::http::pipeline::App,
    body: serde_json::Value,
) -> super::super::test_app::Reply {
    super::super::test_app::send(
        app,
        axum::http::Method::PUT,
        "/api/admin/charts/listeners",
        &[("X-Octo-Admin", "1")],
        Some(&body.to_string()),
    )
    .await
}

#[tokio::test]
async fn listeners_lists_people_from_plays_and_the_store() {
    let apple = apple().await;
    let state = state(&apple, None);
    assert!(state.listening.record("Anna", played("a")));
    state.top_songs.listeners().set(
        "bob",
        crate::services::charts::ListenerCharts {
            country: "br".into(),
            pinned: vec!["jp".into(), "xk".into()],
            ..Default::default()
        },
    );

    let reply = get(&app(state), "/api/admin/charts/listeners").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    assert_eq!(body["maxPinned"], 3);
    let people = body["people"].as_array().unwrap();
    let names: Vec<&str> = people.iter().filter_map(|p| p["user"].as_str()).collect();
    assert_eq!(names, vec!["Anna", "bob"]);
    assert_eq!(people[0]["country"], "auto");
    assert_eq!(people[0]["pinned"], serde_json::json!([]));
    assert_eq!(people[1]["country"], "br");
    assert_eq!(people[1]["pinned"], serde_json::json!(["jp", "xk"]));
}

#[tokio::test]
async fn the_dashboard_lists_no_one_navidrome_dropped() {
    let apple = apple().await;
    let state = state(&apple, None);
    assert!(state.listening.record("Anna", played("a")));
    assert!(state.listening.record("Gone", played("b")));
    state.top_songs.listeners().set(
        "gone",
        crate::services::charts::ListenerCharts {
            country: "br".into(),
            ..Default::default()
        },
    );
    let listeners = state.top_songs.listeners();
    listeners.reconcile(&[("u1".to_string(), "Anna".to_string())], &listeners.revisions());
    let app = app(state);

    let reply = get(&app, "/api/admin/charts/listeners").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    let names: Vec<&str> = body["people"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["user"].as_str())
        .collect();
    assert_eq!(names, vec!["Anna"]);
    let refused = put_people(&app, serde_json::json!({"user": "Gone", "country": "br"})).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.text());
    assert!(refused.text().contains("Navidrome has no such person."));
    let kept = put_people(&app, serde_json::json!({"user": "anna", "country": "br"})).await;
    assert_eq!(kept.status, StatusCode::OK, "{}", kept.text());
}

#[tokio::test]
async fn listeners_put_saves_a_persons_charts() {
    let apple = apple().await;
    let state = state(&apple, None);
    let app = app(state.clone());

    let saved = put_people(
        &app,
        serde_json::json!({"user": "Anna", "country": "BR", "pinned": ["jp", "", "kr"]}),
    )
    .await;

    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text());
    let anna = state.top_songs.listeners().get("anna");
    assert_eq!(anna.country, "br");
    assert_eq!(anna.pinned, vec!["jp", "kr"]);
    let people = saved.json()["people"].clone();
    assert_eq!(people[0]["user"], "anna");

    let back = put_people(
        &app,
        serde_json::json!({"user": "anna", "country": "auto", "pinned": [], "revision": people[0]["revision"]}),
    )
    .await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.text());
    assert!(
        state.top_songs.listeners().all().is_empty(),
        "nothing chosen, nothing kept"
    );
}

#[tokio::test]
async fn a_put_with_an_old_revision_is_refused_with_the_latest_row() {
    let apple = apple().await;
    let state = state(&apple, None);
    let app = app(state.clone());
    let first = put_people(&app, serde_json::json!({"user": "anna", "country": "br"})).await;
    let seen = first.json()["people"][0]["revision"].as_u64().unwrap();
    assert!(seen > 0);
    let second = put_people(
        &app,
        serde_json::json!({"user": "anna", "country": "jp", "revision": seen}),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.text());

    let stale = put_people(
        &app,
        serde_json::json!({"user": "anna", "country": "kr", "revision": seen}),
    )
    .await;

    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.text());
    let body = stale.json();
    assert_eq!(body["error"], "Changed elsewhere just now; showing the latest.");
    assert_eq!(body["people"][0]["country"], "jp");
    assert!(body["people"][0]["revision"].as_u64().unwrap() > seen);
    assert_eq!(state.top_songs.listeners().get("anna").country, "jp");
    let missing = put_people(&app, serde_json::json!({"user": "anna", "country": "kr"})).await;
    assert_eq!(
        missing.status,
        StatusCode::CONFLICT,
        "a missing revision reads as 0"
    );
}

#[tokio::test]
async fn listeners_put_refuses_an_unknown_country_and_a_fourth_pin() {
    let apple = apple().await;
    let state = state(&apple, None);
    let app = app(state.clone());

    let unknown = put_people(&app, serde_json::json!({"user": "anna", "country": "zz"})).await;
    let unknown_pin = put_people(&app, serde_json::json!({"user": "anna", "pinned": ["zz"]})).await;
    let four = put_people(
        &app,
        serde_json::json!({"user": "anna", "pinned": ["jp", "kr", "de", "gb"]}),
    )
    .await;
    let nobody = put_people(&app, serde_json::json!({"user": " ", "country": "br"})).await;
    let bad_of_four = put_people(
        &app,
        serde_json::json!({"user": "anna", "pinned": ["jp", "kr", "de", "x1"]}),
    )
    .await;

    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert!(unknown.text().contains("no charts for zz"), "{}", unknown.text());
    assert_eq!(unknown_pin.status, StatusCode::BAD_REQUEST);
    assert_eq!(four.status, StatusCode::BAD_REQUEST);
    assert!(four.text().contains("at most 3"), "{}", four.text());
    assert_eq!(nobody.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_of_four.status, StatusCode::BAD_REQUEST);
    assert!(
        bad_of_four.text().contains("no charts for x1"),
        "the code that is no country is named: {}",
        bad_of_four.text()
    );
    assert!(state.top_songs.listeners().all().is_empty());
}

#[tokio::test]
async fn listeners_are_for_the_dashboard_and_admins_only() {
    use crate::services::admin::{AdminCaller, AdminCallerKind};
    let may = |kind| super::may_choose_anyones(Some(&AdminCaller::new(Some("x".into()), kind)));
    assert!(may(AdminCallerKind::Dashboard));
    assert!(may(AdminCallerKind::NavidromeAdmin));
    assert!(may(AdminCallerKind::SignInOff));
    assert!(!may(AdminCallerKind::Listener));
    assert!(!may(AdminCallerKind::FamilyMember));
    assert!(!super::may_choose_anyones(None));

    // An app's own sign-in does not open the dashboard's route.
    let apple = apple().await;
    let reply = get(
        &app(state(&apple, Some("required"))),
        "/api/admin/charts/listeners?u=alice&p=pw&v=1.16.1&c=test",
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn charts_say_the_countries_people_chose() {
    let apple = apple().await;
    let state = state(&apple, None);
    state.top_songs.chart("34").await;
    state.top_songs.listeners().set(
        "anna",
        crate::services::charts::ListenerCharts {
            country: "br".into(),
            pinned: vec!["us".into(), "jp".into()],
            ..Default::default()
        },
    );

    let body = get(&app(state), "/api/admin/charts").await.json();

    assert_eq!(body["listenerCountries"], serde_json::json!(["br", "jp"]));
}

#[tokio::test]
async fn another_countrys_fetch_is_never_shown_as_the_servers() {
    let apple = apple().await;
    let state = state(&apple, None);
    state.top_songs.chart("34").await;
    // Japan's page is not served: its fetch fails, and must not stand for the server's.
    state.top_songs.chart_in(Some("jp".into()), "34").await;

    let body = get(&app(state), "/api/admin/charts").await.json();

    let popular = &body["lists"][0];
    assert_eq!(popular["source"], "apple");
    assert_eq!(popular["songs"], 5);
    assert!(popular["error"].is_null());
}
