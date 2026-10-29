use super::*;

fn charts(country: &str, pinned: &[&str]) -> ListenerCharts {
    ListenerCharts {
        country: country.to_string(),
        pinned: pinned.iter().map(|c| (*c).to_string()).collect(),
        ..Default::default()
    }
}

fn row(country: &str, revision: u64) -> ListenerCharts {
    row_with(country, &[], revision)
}

fn row_with(country: &str, pinned: &[&str], revision: u64) -> ListenerCharts {
    ListenerCharts {
        revision,
        ..charts(country, pinned)
    }
}

/// An empty store on a file in a fresh folder, with the folder and the file's path.
fn saved_store() -> (tempfile::TempDir, std::path::PathBuf, ChartCountryStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart-countries.json");
    let store = ChartCountryStore::new(Some(path.clone()));
    (dir, path, store)
}

#[test]
fn a_choice_survives_a_reload_from_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart-countries.json");
    ChartCountryStore::new(Some(path.clone())).set("anna", charts("br", &["jp", "kr"]));

    let again = ChartCountryStore::new(Some(path.clone()));

    assert_eq!(again.get("anna"), row_with("br", &["jp", "kr"], 1));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved,
        serde_json::json!({
            "stamp": 1,
            "people": {"anna": {"country": "br", "pinned": ["jp", "kr"], "revision": 1}}
        })
    );
}

#[test]
fn choosing_nothing_removes_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart-countries.json");
    let store = ChartCountryStore::new(Some(path.clone()));
    store.set("anna", charts("br", &[]));
    store.set("bob", charts("", &["jp"]));

    store.set("anna", ListenerCharts::default());

    assert_eq!(store.get("anna"), ListenerCharts::default());
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved["people"].get("anna").is_none());
    assert!(saved["people"].get("bob").is_some());
}

#[test]
fn names_ignore_case() {
    let store = ChartCountryStore::new(None);
    store.set(" Anna ", charts("br", &[]));

    assert_eq!(store.get("anna").country, "br");
    assert_eq!(store.get("ANNA").country, "br");
    assert_eq!(store.all().len(), 1);
    assert_eq!(store.all()[0].0, "anna");
}

#[test]
fn countries_hold_homes_and_pins_once_each() {
    let store = ChartCountryStore::new(None);
    store.set("anna", charts("br", &["jp", "kr"]));
    store.set("bob", charts("jp", &["br"]));
    store.set("cara", charts("", &["de"]));

    assert_eq!(store.countries(), vec!["br", "de", "jp", "kr"]);
}

#[test]
fn a_broken_file_is_moved_aside_and_the_store_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart-countries.json");
    std::fs::write(&path, b"{ not json").unwrap();

    let store = ChartCountryStore::new(Some(path.clone()));

    assert!(store.all().is_empty());
    assert!(!path.exists(), "moved aside");
    let aside: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("chart-countries.json.broken-"))
        })
        .collect();
    assert_eq!(aside.len(), 1);
    assert_eq!(std::fs::read(&aside[0]).unwrap(), b"{ not json", "kept as it was");
    store.set("anna", charts("br", &[]));
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["people"]["anna"]["country"], "br");
}

#[test]
fn a_file_in_any_case_is_read_in_lowercase() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart-countries.json");
    std::fs::write(
        &path,
        br#"{"Anna": {"country": " BR ", "pinned": ["JP", "jp", "Kr"]}, "bob": {"pinned": ["De"]}}"#,
    )
    .unwrap();

    let store = ChartCountryStore::new(Some(path));

    assert_eq!(store.get("anna"), charts("br", &["jp", "kr"]));
    assert_eq!(store.get("BOB"), charts("", &["de"]));
    assert_eq!(store.get("bob").revision, 0);
    assert_eq!(store.countries(), vec!["br", "de", "jp", "kr"]);
}

#[test]
fn saves_from_many_threads_leave_the_newest_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chart-countries.json");
    let store = std::sync::Arc::new(ChartCountryStore::new(Some(path.clone())));

    let threads: Vec<_> = (0..8)
        .map(|n| {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                for i in 0..20 {
                    store.set(
                        &format!("user{n}"),
                        charts(if i % 2 == 0 { "br" } else { "jp" }, &[]),
                    );
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }

    let again = ChartCountryStore::new(Some(path));
    assert_eq!(again.all(), store.all(), "the file holds the last choices");
}

#[test]
fn a_refused_change_leaves_the_choices_as_they_were() {
    let store = ChartCountryStore::new(None);
    store.set("anna", charts("br", &["jp"]));

    let refused = store.update("anna", |mine| {
        mine.pinned.push("kr".into());
        Err("no")
    });
    let added = store.update("anna", |mine| {
        mine.pinned.push("kr".into());
        Ok::<(), ()>(())
    });

    assert_eq!(refused, Err("no"));
    assert_eq!(added, Ok(row_with("br", &["jp", "kr"], 2)));
    assert_eq!(store.get("anna"), row_with("br", &["jp", "kr"], 2));
}

#[test]
fn a_put_with_an_old_revision_is_refused_with_the_latest_row() {
    let store = ChartCountryStore::new(None);
    store.set("anna", charts("br", &[]));
    let seen = store.get("anna").revision;
    store.set("anna", charts("jp", &[]));

    let refused = store.update_if("anna", seen, |mine| mine.country = "kr".into());

    assert_eq!(
        refused,
        Err(Conflict {
            current: row("jp", 2)
        })
    );
    assert_eq!(store.get("anna").country, "jp", "left as it was");
    let accepted = store.update_if("anna", 2, |mine| mine.country = "kr".into());
    assert_eq!(accepted, Ok(row("kr", 3)));
}

#[test]
fn a_removed_then_remade_row_never_reuses_a_revision() {
    let store = ChartCountryStore::new(None);
    store.set("anna", charts("br", &[]));
    let first = store.get("anna").revision;
    store.set("anna", ListenerCharts::default());
    assert_eq!(store.get("anna").revision, 0, "a removed row reads as 0");

    store.set("anna", charts("br", &[]));

    assert!(store.get("anna").revision > first);
    assert!(store.update_if("anna", first, |_| ()).is_err());
}

#[test]
fn the_revision_check_and_write_are_one_step() {
    let store = std::sync::Arc::new(ChartCountryStore::new(None));
    store.set("anna", charts("br", &[]));
    let seen = store.get("anna").revision;

    let threads: Vec<_> = ["jp", "kr", "de", "gb"]
        .into_iter()
        .map(|country| {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .update_if("anna", seen, |mine| mine.country = country.into())
                    .is_ok()
            })
        })
        .collect();
    let wins = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1, "only the first save based on that revision lands");
    assert_eq!(store.get("anna").revision, seen + 1);
}

#[test]
fn a_v1_file_loads_with_revision_0() {
    let (_dir, path, _) = saved_store();
    std::fs::write(&path, br#"{"anna": {"country": "br", "pinned": ["jp"]}}"#).unwrap();

    let store = ChartCountryStore::new(Some(path));

    assert_eq!(store.get("anna"), charts("br", &["jp"]));
    assert_eq!(store.get("anna").revision, 0);
    store.set("anna", charts("kr", &[]));
    assert_eq!(store.get("anna").revision, 1);
}

#[test]
fn duplicate_case_keys_keep_the_newer_row() {
    let (_dir, path, _) = saved_store();
    std::fs::write(
        &path,
        br#"{"stamp": 2, "people": {"ANNA": {"country": "br", "revision": 5}, "anna": {"country": "jp", "revision": 2}}}"#,
    )
    .unwrap();

    let store = ChartCountryStore::new(Some(path));

    assert_eq!(store.all().len(), 1);
    assert_eq!(store.get("Anna"), row("br", 5));
    store.set("bob", charts("kr", &[]));
    assert_eq!(store.get("bob").revision, 6, "the stamp starts past every row");
}

fn people(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(id, name)| ((*id).to_string(), (*name).to_string()))
        .collect()
}

#[test]
fn rows_of_people_navidrome_dropped_are_removed_and_not_warmed() {
    let store = ChartCountryStore::new(None);
    store.set("anna", charts("br", &["jp"]));
    store.set("bob", charts("kr", &[]));
    let seen = store.revisions();

    let done = store.reconcile(&people(&[("u1", "Anna")]), &seen);

    assert_eq!(done.dropped, vec!["bob".to_string()]);
    assert_eq!(store.get("bob"), ListenerCharts::default());
    assert_eq!(store.get("anna").navidrome_id, "u1");
    assert_eq!(store.countries(), vec!["br".to_string(), "jp".to_string()]);
}

#[test]
fn a_non_ascii_name_is_kept() {
    let store = ChartCountryStore::new(None);
    store.set("Élise", charts("fr", &[]));
    let seen = store.revisions();

    let done = store.reconcile(&people(&[("u9", "Élise"), ("u1", "anna")]), &seen);

    assert_eq!(done, Reconciled::default());
    assert_eq!(store.get("élise").country, "fr");
    assert_eq!(store.get("Élise").navidrome_id, "u9");
}

#[test]
fn a_row_saved_after_the_list_was_read_is_kept() {
    let store = ChartCountryStore::new(None);
    store.set("bob", charts("kr", &[]));
    let seen = store.revisions();
    store.set("bob", charts("de", &[]));
    store.set("carl", charts("it", &[]));

    let done = store.reconcile(&people(&[("u1", "anna")]), &seen);

    assert_eq!(done, Reconciled::default());
    assert_eq!(store.get("bob").country, "de", "changed since it was read");
    assert_eq!(store.get("carl").country, "it", "added since it was read");
}

#[test]
fn a_renamed_person_keeps_their_row() {
    let (_dir, path, store) = saved_store();
    store.set("anna", charts("br", &["jp"]));
    store.reconcile(&people(&[("u1", "Anna")]), &store.revisions());
    let before = store.get("anna");

    let done = store.reconcile(&people(&[("u1", "Annie")]), &store.revisions());

    assert_eq!(done.renamed, vec![("anna".to_string(), "annie".to_string())]);
    assert!(done.dropped.is_empty());
    assert_eq!(store.get("annie"), before);
    assert_eq!(store.get("anna"), ListenerCharts::default());
    let again = ChartCountryStore::new(Some(path));
    assert_eq!(again.get("annie").navidrome_id, "u1");
    assert_eq!(again.all().len(), 1);
}

#[test]
fn a_save_never_loses_the_recorded_navidrome_id() {
    let store = ChartCountryStore::new(None);
    store.set("anna", charts("br", &[]));
    store.reconcile(&people(&[("u1", "anna")]), &store.revisions());

    store.set("anna", charts("jp", &[]));

    assert_eq!(store.get("anna").navidrome_id, "u1");
}

#[test]
fn the_people_are_known_only_after_a_check() {
    let store = ChartCountryStore::new(None);
    assert!(store.is_known("anyone"));

    store.reconcile(&people(&[("u1", "Anna")]), &store.revisions());

    assert!(store.is_known("ANNA"));
    assert!(!store.is_known("bob"));
}
