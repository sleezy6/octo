use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
use octo_subsonic::subsonic_request_parser::Parameters;
use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

/// A Navidrome whose scan time, folders and one song's star can be changed, counting searches.
struct Nav {
    server: MockServer,
    proxy: SubsonicProxyService,
    last_scan: Arc<Mutex<String>>,
    folders: Arc<Mutex<Vec<u32>>>,
    scan_fails: Arc<AtomicBool>,
    starred: Arc<AtomicBool>,
}

impl Nav {
    async fn new() -> Nav {
        let server = MockServer::start().await;
        let last_scan = Arc::new(Mutex::new("2026-10-01T00:00:00Z".to_string()));
        let folders = Arc::new(Mutex::new(vec![1u32]));
        let scan_fails = Arc::new(AtomicBool::new(false));
        let starred = Arc::new(AtomicBool::new(false));
        let ok = |body: Value| ResponseTemplate::new(200).set_body_json(json!({"subsonic-response": body}));
        {
            let (last_scan, scan_fails) = (Arc::clone(&last_scan), Arc::clone(&scan_fails));
            Mock::given(method("GET"))
                .and(path("/rest/getScanStatus"))
                .respond_with(move |_: &Request| {
                    if scan_fails.load(Ordering::SeqCst) {
                        return ResponseTemplate::new(500);
                    }
                    ok(json!({"status": "ok", "scanStatus": {"scanning": false, "lastScan": *last_scan.lock()}}))
                })
                .mount(&server)
                .await;
        }
        {
            let folders = Arc::clone(&folders);
            Mock::given(method("GET"))
                .and(path("/rest/getMusicFolders"))
                .respond_with(move |_: &Request| {
                    let list: Vec<Value> = folders
                        .lock()
                        .iter()
                        .map(|id| json!({"id": id, "name": "Music"}))
                        .collect();
                    ok(json!({"status": "ok", "musicFolders": {"musicFolder": list}}))
                })
                .mount(&server)
                .await;
        }
        {
            let starred = Arc::clone(&starred);
            Mock::given(method("GET"))
                .and(path("/rest/search3"))
                .respond_with(move |request: &Request| {
                    let query = request
                        .url
                        .query_pairs()
                        .find(|(k, _)| k == "query")
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default();
                    let songs = if query == "has" {
                        let mut song = json!({"id": "s1", "artist": "A", "title": "T"});
                        if starred.load(Ordering::SeqCst) {
                            song["starred"] = json!("2026-10-02T00:00:00Z");
                        }
                        json!([song])
                    } else {
                        json!([])
                    };
                    ok(json!({"status": "ok", "searchResult3": {"song": songs}}))
                })
                .mount(&server)
                .await;
        }
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        Nav {
            proxy: SubsonicProxyService::new(settings),
            server,
            last_scan,
            folders,
            scan_fails,
            starred,
        }
    }

    /// One lookup the way a build does it: skip what the library already said it lacks, else
    /// search and remember the answer.
    async fn look(&self, cache: &LibraryMatchCache, user: &str, query: &str) -> Option<Value> {
        let session = cache.session(&self.proxy, &auth(), user).await;
        if session.skips(query) {
            return None;
        }
        let mut search = auth();
        search.insert("query".into(), query.to_string());
        search.insert("f".into(), "json".into());
        let relay = self.proxy.relay_safe("rest/search3", &search).await.unwrap();
        let doc: Value = serde_json::from_slice(&relay.body).unwrap();
        let song = doc.pointer("/subsonic-response/searchResult3/song/0").cloned();
        session.record(query, song.as_ref());
        song
    }

    async fn searches(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/rest/search3")
            .count()
    }
}

fn auth() -> Parameters {
    "u=alice&t=good&s=salt&v=1.16.1&c=test"
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn two_builds_with_the_same_library_search_misses_once() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::new();

    nav.look(&cache, "alice", "none").await;
    nav.look(&cache, "alice", "none").await;

    assert_eq!(nav.searches().await, 1);
}

#[tokio::test]
async fn a_new_scan_searches_again() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::new();
    nav.look(&cache, "alice", "none").await;

    *nav.last_scan.lock() = "2026-10-02T00:00:00Z".into();
    nav.look(&cache, "alice", "none").await;

    assert_eq!(nav.searches().await, 2);
}

#[tokio::test]
async fn a_library_granted_without_a_scan_searches_again() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::new();
    nav.look(&cache, "alice", "none").await;

    nav.folders.lock().push(2);
    nav.look(&cache, "alice", "none").await;

    assert_eq!(nav.searches().await, 2);
}

#[tokio::test]
async fn a_failed_scan_status_searches_without_the_cache() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::new();
    nav.scan_fails.store(true, Ordering::SeqCst);

    nav.look(&cache, "alice", "none").await;
    nav.look(&cache, "alice", "none").await;

    assert_eq!(nav.searches().await, 2);
    assert_eq!(cache.kept_for("alice"), 0);
}

#[tokio::test]
async fn users_never_share_decisions() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::new();

    nav.look(&cache, "alice", "none").await;
    nav.look(&cache, "bob", "none").await;
    nav.look(&cache, "Alice", "none").await;

    assert_eq!(
        nav.searches().await,
        2,
        "bob searched for himself; Alice is alice"
    );
}

#[tokio::test]
async fn a_starred_hit_shows_its_new_star_without_a_scan() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::new();
    let before = nav.look(&cache, "alice", "has").await.unwrap();
    assert!(before.get("starred").is_none());

    nav.starred.store(true, Ordering::SeqCst);
    let after = nav.look(&cache, "alice", "has").await.unwrap();

    assert_eq!(after["starred"], "2026-10-02T00:00:00Z");
    assert_eq!(nav.searches().await, 2, "a matched song is searched each time");
}

#[tokio::test]
async fn a_user_cache_never_passes_its_cap() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::with_cap(3);

    for n in 0..10 {
        nav.look(&cache, "alice", &format!("none {n}")).await;
    }

    assert_eq!(cache.kept_for("alice"), 3);
    nav.look(&cache, "alice", "none 0").await;
    assert_eq!(nav.searches().await, 11, "the oldest decision was dropped");
    nav.look(&cache, "alice", "none 9").await;
    assert_eq!(nav.searches().await, 11, "the newest was kept");
}

#[tokio::test]
async fn an_unnamed_caller_uses_no_cache() {
    let nav = Nav::new().await;
    let cache = LibraryMatchCache::new();

    nav.look(&cache, "", "none").await;
    nav.look(&cache, "  ", "none").await;

    assert_eq!(nav.searches().await, 2);
    assert_eq!(cache.kept_for(""), 0);
}
