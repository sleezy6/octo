//! Songs fetched ahead on the download path: one lands in the prefetch folder with its facts and
//! nothing else, and a heart afterwards (or one that was waiting on it) keeps that file, finalized
//! like a fresh download, without a second transfer.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octo_core::common::Clock;
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, SettingsStore, SoulseekSettings, SubsonicSettings};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::test_support::{Harness, build, catalog};
use super::*;
use crate::services::common::DownloadConcurrency;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::prefetch::{PrefetchStore, sidecar_path};

fn song(id: &str) -> Song {
    Song {
        title: format!("Song {id}"),
        artist: "Artist".into(),
        album: "Album".into(),
        duration: Some(1),
        external_provider: Some("test".into()),
        external_id: Some(id.into()),
        ..Default::default()
    }
}

struct Ahead {
    harness: Harness,
    store: Arc<PrefetchStore>,
    root: tempfile::TempDir,
    folder: tempfile::TempDir,
}

impl Ahead {
    fn new() -> Ahead {
        let root = tempfile::tempdir().expect("the music folder");
        let folder = tempfile::tempdir().expect("the prefetch folder");
        let in_folder = folder.path().to_path_buf();
        let store = Arc::new(PrefetchStore::new(
            Arc::new(move || in_folder.clone()),
            Clock::system(),
        ));
        let harness = build(
            root.path(),
            AppSettings::default(),
            None,
            Some(catalog(vec![("t1", song("t1"))])),
            DownloadServices {
                prefetched: Some(store.clone() as Arc<dyn PrefetchedFiles>),
                ..Default::default()
            },
        );
        // The transfer lands a real FLAC in a staging folder, as a Soulseek job would, and
        // says what its checks found.
        let staging = root.path().join(".octo-incoming").join("job");
        harness.backend.land(move |song, id| {
            std::fs::create_dir_all(&staging).expect("staging");
            let path = staging.join(format!("{id}.flac"));
            std::fs::write(&path, crate::services::test_support::flac()).expect("the file");
            song.source_peer = Some("peer-1".into());
            song.verification = Some(Box::new(octo_core::fingerprint::VerificationResult {
                reason: octo_core::fingerprint::InconclusiveReason::NoEntry,
                duration_seconds: 1,
                ..Default::default()
            }));
            path.to_string_lossy().into_owned()
        });
        Ahead {
            harness,
            store,
            root,
            folder,
        }
    }

    async fn ahead(&self, id: &str) -> crate::services::i_download_service::Ahead {
        self.harness
            .service
            .download_ahead("test", id, self.folder.path(), &CancellationToken::new())
            .await
            .expect("downloaded ahead")
    }

    async fn heart(&self, id: &str) -> String {
        heart(&self.harness.service, id).await
    }
}

async fn heart(service: &BaseDownloadService, id: &str) -> String {
    service
        .execute_acquisition(
            "test",
            id,
            false,
            true,
            None,
            &CancellationToken::new(),
            Some(vec!["alice".into()]),
            false,
            None,
        )
        .await
        .expect("hearted")
}

#[tokio::test]
async fn a_download_ahead_lands_in_the_folder_with_its_facts_and_nothing_else() {
    let w = Ahead::new();
    let crate::services::i_download_service::Ahead::Kept { path } = w.ahead("t1").await else {
        panic!("kept");
    };
    assert_eq!(path, w.folder.path().join("test_t1.flac"));
    assert!(path.is_file());
    let facts: crate::services::prefetch::AheadSidecar =
        serde_json::from_slice(&std::fs::read(sidecar_path(&path)).expect("the facts")).expect("JSON");
    assert_eq!(facts.external_id, "t1");
    assert_eq!(facts.format, "FLAC");
    assert_eq!(facts.source_peer.as_deref(), Some("peer-1"));
    assert_eq!(
        facts.verification.as_ref().map(|v| v.reason.as_str()),
        Some("NoEntry")
    );
    // No fetched-songs entry, no record left behind, and nothing in the music folder.
    assert!(w.harness.history.get_recent(10).is_empty());
    assert!(w.harness.service.active_download("ext-test-t1").is_none());
    assert!(!w.root.path().join("Artist").exists());
    assert_eq!(w.store.ready("test", "t1"), Some(path));
}

#[tokio::test]
async fn a_heart_afterwards_keeps_the_file_without_a_second_transfer() {
    let w = Ahead::new();
    w.ahead("t1").await;
    assert_eq!(w.harness.backend.transfers(), 1);

    let landed = w.heart("t1").await;
    assert_eq!(w.harness.backend.transfers(), 1, "no second download");
    assert!(Path::new(&landed).starts_with(w.root.path()), "{landed}");
    assert!(Path::new(&landed).is_file());
    assert!(!landed.contains(".octo-incoming"), "{landed}");
    // Gone from the folder, facts and all.
    assert!(w.store.ready("test", "t1").is_none());
    assert!(!w.folder.path().join("test_t1.flac").exists());
    assert!(!sidecar_path(&w.folder.path().join("test_t1.flac")).exists());
    // Finalized as a fresh download: in the fetched-songs log, for whoever hearted it.
    let history = w.harness.history.get_recent(10);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].path, landed);
    assert_eq!(history[0].format, "FLAC");
    assert_eq!(
        history[0].requested_by.as_deref(),
        Some(&["alice".to_string()][..])
    );
}

#[tokio::test]
async fn a_real_flac_heart_takes_the_file_too() {
    let w = Ahead::new();
    w.ahead("t1").await;
    let landed = w
        .harness
        .service
        .download_song_internal(
            "test",
            "t1",
            DownloadOptions {
                force_permanent: true,
                place: AcquisitionPlace {
                    pool_members: None,
                    flac_only: true,
                },
                ..Default::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("kept");
    assert_eq!(w.harness.backend.transfers(), 1);
    assert!(Path::new(&landed).starts_with(w.root.path()));
}

/// A transfer that waits until the test lets it finish, counting how many ran.
struct GatedBackend {
    root: std::path::PathBuf,
    open: Arc<Notify>,
    started: Arc<Notify>,
    transfers: Arc<AtomicUsize>,
}

#[async_trait]
impl DownloadBackend for GatedBackend {
    fn provider_name(&self) -> &str {
        "test"
    }

    async fn is_available(&self, _: &BaseDownloadService) -> bool {
        true
    }

    async fn download_track(
        &self,
        _: &BaseDownloadService,
        download: &mut TrackDownload,
        _: &CancellationToken,
    ) -> anyhow::Result<String> {
        self.transfers.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.open.notified().await;
        let staging = self.root.join(".octo-incoming").join("gated");
        std::fs::create_dir_all(&staging)?;
        let path = staging.join(format!("{}.flac", download.track_id));
        std::fs::write(&path, crate::services::test_support::flac())?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn extract_external_id_from_album_id(&self, _: &str) -> Option<String> {
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_heart_waiting_on_a_download_ahead_keeps_its_file_rather_than_its_path() {
    let root = tempfile::tempdir().expect("the music folder");
    let folder = tempfile::tempdir().expect("the prefetch folder");
    let in_folder = folder.path().to_path_buf();
    let store = Arc::new(PrefetchStore::new(
        Arc::new(move || in_folder.clone()),
        Clock::system(),
    ));
    // Two transfers may run at once, so the heart waits on the running one rather than on the
    // download lock.
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            auto_detect_download_path: false,
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            parallel_downloads: 2,
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&root.path().to_string_lossy()));
    let concurrency = Arc::new(DownloadConcurrency::new(Arc::clone(&settings)));
    concurrency.prove();
    let history = Arc::new(DownloadHistoryService::new(root.path().join("history.json")));
    let metadata = FakeMetadata::default();
    metadata
        .songs
        .lock()
        .insert(("test".into(), "t1".into()), song("t1"));
    let backend = Arc::new(GatedBackend {
        root: root.path().to_path_buf(),
        open: Arc::new(Notify::new()),
        started: Arc::new(Notify::new()),
        transfers: Arc::new(AtomicUsize::new(0)),
    });
    let service = BaseDownloadService::new(
        DownloadCore {
            settings: Arc::clone(&settings),
            local_library: Arc::new(FakeLocalLibrary::default()),
            metadata: Arc::new(metadata),
            navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
            history: Arc::clone(&history),
            notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
        },
        DownloadServices {
            concurrency: Some(concurrency),
            prefetched: Some(store.clone() as Arc<dyn PrefetchedFiles>),
            ..Default::default()
        },
        backend.clone() as Arc<dyn DownloadBackend>,
    );

    let started = backend.started.notified();
    let ahead = {
        let service = Arc::clone(&service);
        let folder = folder.path().to_path_buf();
        tokio::spawn(async move {
            service
                .download_ahead("test", "t1", &folder, &CancellationToken::new())
                .await
        })
    };
    started.await;
    let hearted = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { heart(&service, "t1").await })
    };
    // The heart is waiting on the running download now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!hearted.is_finished());
    backend.open.notify_one();

    let ahead = tokio::time::timeout(Duration::from_secs(20), ahead)
        .await
        .expect("in time")
        .expect("ran")
        .expect("downloaded ahead");
    assert!(matches!(
        ahead,
        crate::services::i_download_service::Ahead::Kept { .. }
    ));
    let landed = tokio::time::timeout(Duration::from_secs(20), hearted)
        .await
        .expect("in time")
        .expect("ran");
    assert_eq!(backend.transfers.load(Ordering::SeqCst), 1, "no second download");
    assert!(Path::new(&landed).starts_with(root.path()), "{landed}");
    assert!(!Path::new(&landed).starts_with(folder.path()), "{landed}");
    assert!(Path::new(&landed).is_file());
    assert!(store.ready("test", "t1").is_none());
    assert_eq!(history.get_recent(10).len(), 1);
}

#[tokio::test]
async fn a_download_ahead_never_writes_into_a_row_that_has_ended() {
    let root = tempfile::tempdir().expect("the music folder");
    let folder = tempfile::tempdir().expect("the prefetch folder");
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let harness = build(
        root.path(),
        AppSettings::default(),
        None,
        Some(catalog(vec![("t1", song("t1"))])),
        DownloadServices {
            tracker: Some(tracker.clone()),
            ..Default::default()
        },
    );
    let staging = root.path().join(".octo-incoming").join("job");
    harness.backend.land(move |_, id| {
        std::fs::create_dir_all(&staging).expect("staging");
        let path = staging.join(format!("{id}.flac"));
        std::fs::write(&path, crate::services::test_support::flac()).expect("the file");
        path.to_string_lossy().into_owned()
    });
    // A heart on the song ran earlier and ended.
    tracker.begin("test", "t1", None, Some("alice"), None, None, None);
    tracker.complete("test", "t1", None);
    let lines = |tracker: &AcquisitionTracker| {
        tracker
            .detail("test:t1", None)
            .and_then(|row| row.events)
            .map_or(0, |events| events.len())
    };
    let before = lines(&tracker);
    harness
        .service
        .download_ahead("test", "t1", folder.path(), &CancellationToken::new())
        .await
        .expect("downloaded ahead");
    assert_eq!(lines(&tracker), before, "the ended row is as it was");

    // Once the download ahead is over, lines that come after a row's end land as before.
    tracker.log("test", "t1", "note", "Lyrics found", None);
    assert_eq!(lines(&tracker), before + 1);
}
