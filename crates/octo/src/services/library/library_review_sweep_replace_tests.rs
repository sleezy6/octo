//! The sweep's replacement end to end: a song its YouTube preview says is another goes through the
//! real `ExecutorSweepReplacer` and `LibraryActionExecutor` to a download the test plays, leaves an
//! Acted note and the original in the trash, and Put back brings the original home again.

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;

use octo_core::settings::{LibraryActionDefinition, NoticeKind, SubsonicSettings};
use serde_json::json;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::TrackAcquisitionQueue;
use crate::services::common::test_fakes::until;
use crate::services::fingerprint::preview_check::{Comparison, PreviewFacts};
use crate::services::library::library_action_journal::LibraryActionJournal;
use crate::services::library::library_action_test_support::{
    Extras, RecordingLibrary, executor, slash_root, store,
};
use crate::services::library::library_edit_service::LibraryEditServices;
use crate::services::library::library_tag_edits::TagEditJournal;
use crate::services::library::{LibraryActionQuarantine, LibraryEditService, LibraryRescan, NoticeState};
use crate::services::subsonic::NavidromeIdentityService;

const FILE_NAME: &str = "Massive Attack - Teardrop.mp3";

/// An MP3 of `frames` silent MPEG-1 Layer III frames, 128 kbps, 44.1 kHz, tagged as Teardrop.
fn teardrop_mp3(path: &Path, frames: usize) {
    let mut bytes = vec![0u8; 417 * frames];
    for frame in 0..frames {
        bytes[frame * 417..frame * 417 + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    std::fs::write(path, bytes).expect("written");
    let mut file = TagFile::open(path).expect("an MP3");
    file.set_title(Some("Teardrop"));
    file.set_album(Some("Mezzanine"));
    file.set_performers(&["Massive Attack".to_string()]);
    file.set_album_artists(&["Massive Attack".to_string()]);
    file.save().expect("saved");
}

/// AcoustID has never heard the file, and its YouTube preview, a YouTube Music row from the
/// file's own album, says it is another song.
#[derive(Default)]
struct HeardAsAnother {
    verified: AtomicUsize,
}

#[async_trait]
impl IReviewSweepVerifier for HeardAsAnother {
    fn is_ready(&self) -> bool {
        true
    }

    async fn verify(&self, _: &str, _: Option<&str>, _: Option<&str>) -> VerificationResult {
        self.verified.fetch_add(1, Ordering::SeqCst);
        VerificationResult {
            reason: InconclusiveReason::NoEntry,
            fingerprint: Some("AQADtEqk".into()),
            duration_seconds: 330,
            ..Default::default()
        }
    }

    async fn isrc_agrees(&self, _: &VerificationResult, _: &str) -> Option<bool> {
        None
    }

    async fn hear_preview(&self, _: &str, _: &PreviewRequest<'_>, _: &VerificationResult) -> PreviewOutcome {
        PreviewOutcome::Compared {
            heard: Heard::Mismatch,
            comparison: Comparison {
                bit_error: 0.5,
                offset_seconds: 0.0,
            },
            trusted: true,
            preview: PreviewFacts {
                album: Some("Mezzanine".into()),
                ..PreviewFacts::bare("vid")
            },
        }
    }
}

struct Idle;

impl IAcquisitionActivity for Idle {
    fn is_busy(&self) -> bool {
        false
    }
}

/// A Navidrome whose admin left no mark on song nd-1, the only user, at `size` bytes.
async fn navidrome(size: i64) -> MockServer {
    let server = MockServer::start().await;
    let answer = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(path("/auth/login"))
        .respond_with(answer(json!({
            "token": "jwt", "isAdmin": true, "username": "admin",
            "subsonicToken": "st", "subsonicSalt": "ss",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/song/nd-1"))
        .respond_with(answer(json!({
            "id": "nd-1", "path": FILE_NAME, "size": size, "title": "Teardrop",
            "artist": "Massive Attack", "album": "Mezzanine", "suffix": "mp3", "duration": 330,
            "starred": false, "rating": 0, "playCount": 4,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/search3"))
        .respond_with(answer(json!({"subsonic-response": {
            "status": "ok",
            "searchResult3": {"song": [{"id": "nd-1", "size": size}]},
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(answer(json!([{"id": "u1", "userName": "admin"}])))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw("[]", "application/json"))
        .mount(&server)
        .await;
    server
}

/// Every audio file under `dir`, walked.
fn mp3s_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(items) = std::fs::read_dir(dir) else {
        return found;
    };
    for item in items.flatten() {
        let path = item.path();
        if path.is_dir() {
            found.extend(mp3s_under(&path));
        } else if path.extension().is_some_and(|e| e == "mp3") {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn the_sweep_replaces_a_song_its_preview_says_is_another_and_put_back_restores_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("music");
    std::fs::create_dir_all(&root).expect("the music root");
    let song = root.join(FILE_NAME);
    teardrop_mp3(&song, 20);
    let original_bytes = std::fs::read(&song).expect("the original");
    let navidrome = navidrome(original_bytes.len() as i64).await;

    let settings = store(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            review_enabled: true,
            review_sweep_per_hour: 3600,
            review_sweep_auto_replace_preview: true,
            keep_replaced_originals: true,
            allowed_users: vec!["admin".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::WrongSong,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            url: Some(navidrome.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("secret".into()),
            auto_detect_download_path: false,
            download_source: DownloadSource::SoulseekThenYouTube,
            ..Default::default()
        },
        soulseek: SoulseekSettings {
            enabled: true,
            base_url: Some("http://slskd.test".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    let music_root = slash_root(&root);
    settings.set_raw("Library:DownloadPath", Some(&music_root));

    let journal = Arc::new(LibraryActionJournal::new());
    let library = Arc::new(RecordingLibrary::default());
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let notices = Arc::new(NoticeQueue::new());
    let actions = executor(
        &settings,
        Extras {
            journal: Some(journal.clone()),
            library: Some(library.clone()),
            queue: Some(queue.clone()),
            notices: Some(notices.clone()),
            ..Default::default()
        },
    );
    actions.configure(|seams| {
        seams.history_poll = Duration::from_millis(1);
        seams.history_attempts = 1;
        seams.shows_at = Some(Arc::new(|_, _| async { true }.boxed()));
        seams.force_scan = Some(Arc::new(|| async { true }.boxed()));
    });
    let http = crate::services::http_client_factory::default_client();
    let identity = NavidromeIdentityService::new(settings.clone(), http.clone());
    let resolver = Arc::new(NavidromeSongPathResolver::new(
        identity.clone(),
        library.clone(),
        http.clone(),
        settings.clone(),
    ));
    let replacer = ExecutorSweepReplacer::new(
        resolver.clone(),
        actions,
        Arc::new(NavidromePlaylistApi::new(
            http.clone(),
            identity.clone(),
            settings.clone(),
        )),
        None,
        Arc::new(NavidromeOtherListeners::new(
            Arc::new(NavidromeAdmin::new(identity, http, settings.clone())),
            Arc::new(FamilyStore::in_memory()),
        )),
    );
    let verifier = Arc::new(HeardAsAnother::default());
    let swept_root = music_root.clone();
    let worker = Arc::new(
        LibraryReviewSweepWorker::new(
            LibraryReviewSweepParts {
                store: Arc::new(ReviewSweepStore::new(None)),
                notices: notices.clone(),
                verifier: verifier.clone(),
                activity: Arc::new(Idle),
                library: library.clone(),
                settings: settings.clone(),
                music_root: Box::new(move || swept_root.clone()),
            },
            Clock::system(),
        )
        .with_replacer(Arc::new(replacer)),
    );

    // Octo signs in to Navidrome as the admin before the sweep's search can name the song.
    assert!(resolver.resolve("nd-1").await.is_some());
    assert!(
        resolver
            .find_id_by_path("Massive Attack", "Teardrop", &slash_root(&song))
            .await
            .is_some()
    );

    let ticking = worker.clone();
    let mut tick = tokio::spawn(async move { ticking.tick().await });
    let never = CancellationToken::new();
    let download = tokio::select! {
        download = tokio::time::timeout(Duration::from_secs(10), queue.dequeue(&never)) => {
            download.expect("a download in time").expect("a download")
        }
        _ = &mut tick => panic!(
            "the tick ended without a download: {:?}; asked {}; notes {:?}; journal {:?}",
            worker.status(),
            verifier.verified.load(Ordering::SeqCst),
            notices.for_user("admin", NoticeKind::Review),
            journal.recent(10),
        ),
    };
    // Replaced by its preview: only from a source the preview checks, never the YouTube fallback.
    assert_eq!(download.source_override, Some(DownloadSource::Soulseek));

    // The download service's reveal: the check, then the right copy into the song's place.
    let incoming = root.join(".octo-incoming");
    std::fs::create_dir_all(&incoming).expect("the incoming folder");
    let staged = incoming.join("replacement.mp3");
    teardrop_mp3(&staged, 30);
    let right_bytes = std::fs::read(&staged).expect("the right copy");
    assert_ne!(right_bytes, original_bytes);
    let staged = slash_root(&staged);
    let handoff = download.replacement.clone().expect("a handoff");
    assert_eq!((handoff.before_reveal)(staged.clone()).await, None);
    let target = handoff.target_for(".mp3").expect("a free name");
    std::fs::rename(&staged, &target).expect("moved in");
    handoff.set_revealed_path(target.clone());
    if let Some(revealed) = &handoff.on_revealed {
        revealed(&target);
    }
    queue.release(&download);
    download.completion.try_set_result(&target);
    tokio::time::timeout(Duration::from_secs(10), tick)
        .await
        .expect("the tick in time")
        .expect("no panic");

    // The right copy is at the song's path, and the original sits in the trash.
    assert_eq!(std::fs::read(&song).expect("the song"), right_bytes);
    let trashed = mp3s_under(&root.join(".octo-trash"));
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    assert_eq!(
        std::fs::read(&trashed[0]).expect("the trashed file"),
        original_bytes
    );
    let notes = notices.for_user("admin", NoticeKind::Review);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].state, NoticeState::Acted);
    assert!(notes[0].reason.contains("YouTube preview"), "{notes:?}");
    assert_eq!(worker.store().read(|s| s.replaced), 1);
    // Navidrome kept the song: the action's last word is in before Put back reads it.
    until(|| {
        journal
            .recent(10)
            .iter()
            .any(|entry| entry.history_kept == Some(true))
    })
    .await;

    let edits = LibraryEditService::new(
        resolver,
        Arc::new(LibraryActionQuarantine::new(settings.clone())),
        journal,
        Arc::new(TagEditJournal::new(None)),
        Arc::new(LibraryRescan::new(None)),
        settings.clone(),
        LibraryEditServices {
            notices: Some(notices.clone()),
            ..Default::default()
        },
    );
    let put_back = edits.restore("nd-1", "admin").await;

    assert_eq!(put_back.state, "applied", "{put_back:?}");
    assert_eq!(std::fs::read(&song).expect("the song"), original_bytes);
    let note = notices
        .for_user("admin", NoticeKind::Review)
        .into_iter()
        .next()
        .expect("the note");
    assert_eq!(note.reason, NoticeQueue::PUT_BACK_REASON);
    assert_eq!(note.state, NoticeState::Acted);

    // Started over, the sweep still leaves the put-back original alone.
    worker.reset();
    worker.tick().await;
    worker.tick().await;
    assert_eq!(verifier.verified.load(Ordering::SeqCst), 1);
    assert!(queue.is_idle());
    assert_eq!(std::fs::read(&song).expect("the song"), original_bytes);
}
