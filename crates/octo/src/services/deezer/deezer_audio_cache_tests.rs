//! The playback cache, against the mock Deezer of `super::test_support`.

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::TryStreamExt;

use super::*;
use crate::services::deezer::test_support::{Fixture, Scenario};

struct Cached {
    deezer: Fixture,
    cache: Arc<DeezerAudioCache>,
    dir: tempfile::TempDir,
}

fn payload(length: usize) -> Vec<u8> {
    (0..length).map(|i| (i % 251) as u8).collect()
}

impl Cached {
    async fn new(payload: Vec<u8>) -> Cached {
        Self::with(Scenario {
            payload,
            ..Default::default()
        })
        .await
    }

    async fn with(scenario: Scenario) -> Cached {
        let deezer = Fixture::new(scenario).await;
        let dir = tempfile::tempdir().expect("a temp folder");
        let cache = Arc::new(DeezerAudioCache::with_root(
            deezer.resolver.clone(),
            deezer.settings.clone(),
            dir.path().join("cache"),
        ));
        let cached = Cached { deezer, cache, dir };
        cached.configure(|settings| settings.cache_enabled = true);
        cached
    }

    fn configure(&self, change: impl FnOnce(&mut DeezerSettings)) {
        let mut settings = (*self.deezer.settings.current()).clone();
        change(&mut settings.deezer);
        self.deezer.settings.set(settings);
    }

    async fn open(&self, range: Option<&str>) -> DeezerOpenStream {
        self.cache
            .open_stream("42", range, "MP3_320")
            .await
            .expect("the song opens")
    }

    fn copy(&self) -> PathBuf {
        self.cache.root().join("42.MP3_320.mp3")
    }

    /// Waits for the download of track 42 to end, one way or the other.
    async fn settled(&self) {
        for _ in 0..500 {
            if self.cache.fills.lock().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the download never ended");
    }

    fn cdn_reads(&self) -> usize {
        self.deezer.seen.lock().cdn_ranges.len()
    }

    fn put(&self, name: &str, size: usize, age: Duration) -> PathBuf {
        let path = self.cache.root().join(name);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the folder");
        std::fs::write(&path, vec![7u8; size]).expect("written");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("opens")
            .set_modified(SystemTime::now() - age)
            .expect("dated");
        path
    }
}

async fn read(opened: DeezerOpenStream) -> Vec<u8> {
    let chunks: Vec<Bytes> = opened.audio_stream.try_collect().await.expect("the song reads");
    chunks.concat()
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

#[tokio::test]
async fn the_first_play_makes_a_copy_and_the_next_reads_it() {
    let song = payload(9000);
    let cached = Cached::new(song.clone()).await;

    let first = cached.open(None).await;
    assert_eq!((first.status_code, first.content_length), (200, Some(9000)));
    assert_eq!(first.content_type, "audio/mpeg");
    assert_eq!(read(first).await, song);
    cached.settled().await;
    assert_eq!(std::fs::read(cached.copy()).expect("the copy"), song);
    assert_eq!(cached.cdn_reads(), 1);

    let again = cached.open(None).await;
    assert_eq!((again.status_code, again.content_length), (200, Some(9000)));
    assert_eq!(again.format, "MP3_320");
    assert_eq!(read(again).await, song);
    // Deezer was not asked again: not for the address, not for the bytes.
    assert_eq!(cached.cdn_reads(), 1);
    assert_eq!(cached.deezer.seen.lock().media_bodies.len(), 1);
    // Nothing is left half-made.
    assert!(
        std::fs::read_dir(cached.cache.root().join(PARTIAL_FOLDER))
            .expect("the folder")
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn a_seek_in_a_copy_reads_the_copy() {
    let song = payload(5000);
    let cached = Cached::new(song.clone()).await;
    read(cached.open(None).await).await;
    cached.settled().await;

    for (range, status, content_range, expected) in [
        ("bytes=2301-4198", 206, "bytes 2301-4198/5000", &song[2301..4199]),
        ("bytes=4990-", 206, "bytes 4990-4999/5000", &song[4990..]),
        ("bytes=-10", 206, "bytes 4990-4999/5000", &song[4990..]),
        ("bytes=0-0", 206, "bytes 0-0/5000", &song[..1]),
        ("bytes=5000-", 416, "bytes */5000", &song[..0]),
    ] {
        let opened = cached.open(Some(range)).await;
        assert_eq!(opened.status_code, status, "{range}");
        assert_eq!(opened.content_range.as_deref(), Some(content_range), "{range}");
        assert_eq!(opened.content_length, Some(expected.len() as u64), "{range}");
        assert_eq!(read(opened).await, expected, "{range}");
    }
    assert_eq!(cached.cdn_reads(), 1);
}

#[tokio::test]
async fn two_plays_at_once_share_one_download() {
    let song = payload(70_000);
    let cached = Cached::new(song.clone()).await;

    let (one, two) = tokio::join!(cached.open(None), cached.open(Some("bytes=10-")));
    let (one, two) = tokio::join!(read(one), read(two));

    assert_eq!(one, song);
    assert_eq!(two, song[10..]);
    cached.settled().await;
    assert_eq!(cached.cdn_reads(), 1);
    assert_eq!(cached.deezer.seen.lock().media_bodies.len(), 1);
}

#[tokio::test]
async fn a_player_that_leaves_does_not_stop_the_copy() {
    let song = payload(70_000);
    let cached = Cached::new(song.clone()).await;

    drop(cached.open(None).await);

    cached.settled().await;
    assert_eq!(std::fs::read(cached.copy()).expect("the copy"), song);
}

#[tokio::test]
async fn a_play_counts_as_a_play_of_the_copy() {
    let cached = Cached::new(payload(100)).await;
    let copy = cached.put("42.MP3_320.mp3", 100, 3 * DAY);

    read(cached.open(None).await).await;

    let played = std::fs::metadata(&copy)
        .expect("the copy")
        .modified()
        .expect("a time");
    assert!(played.elapsed().expect("in the past") < Duration::from_secs(60));
    // A copy already there is the song: Deezer is not asked at all.
    assert_eq!(cached.cdn_reads(), 0);
    assert!(cached.deezer.seen.lock().auth_cookies.is_empty());
}

#[tokio::test]
async fn with_the_cache_off_every_play_is_deezers_and_nothing_is_kept() {
    let song = payload(3000);
    let cached = Cached::new(song.clone()).await;
    cached.configure(|settings| settings.cache_enabled = false);

    assert_eq!(read(cached.open(None).await).await, song);
    assert_eq!(read(cached.open(Some("bytes=5-")).await).await, song[5..]);

    assert!(!cached.cache.root().exists());
    assert!(cached.cdn_reads() >= 2);
    assert!(
        cached
            .cache
            .copy_ready("42", "MP3_320", "/nowhere/song")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_song_larger_than_the_cache_plays_without_a_copy() {
    let song = payload(3000);
    let cached = Cached::new(song.clone()).await;
    cached.configure(|settings| settings.cache_max_gi_b = 1000.0 / (1024.0 * 1024.0 * 1024.0));

    assert_eq!(read(cached.open(None).await).await, song);
    cached.settled().await;
    assert!(!cached.copy().exists());
}

#[tokio::test]
async fn a_song_no_account_is_given_is_no_song() {
    let cached = Cached::with(Scenario {
        available_formats: Vec::new(),
        ..Default::default()
    })
    .await;
    assert!(cached.cache.open_stream("42", None, "MP3_320").await.is_none());
    assert!(cached.cache.open_stream("abc", None, "MP3_320").await.is_none());
    cached.settled().await;
    assert!(!cached.copy().exists());
}

/// A download in progress, made by hand: `arrived` bytes of a song `total` long.
async fn half_made(cached: &Cached, arrived: &[u8], total: u64, format: &str) -> (ProgressiveFile, Fill) {
    let staging = cached
        .cache
        .root()
        .join(PARTIAL_FOLDER)
        .join(format!("42.{}.part", uuid::Uuid::new_v4().simple()));
    let mut file = ProgressiveFile::create(&staging).await.expect("created");
    file.append(arrived).await.expect("written");
    let fill = Fill {
        source: file.source(),
        format: format.to_string(),
        total,
    };
    (file, fill)
}

#[tokio::test]
async fn a_seek_far_past_what_has_arrived_is_not_made_to_wait() {
    let cached = Cached::new(payload(10)).await;
    let total = 2 * DeezerAudioCache::WAIT_AHEAD + 1000;
    let (mut file, fill) = half_made(&cached, &payload(1000), total, "MP3_320").await;

    // Just ahead: it waits for the download, which is about to get there.
    let near = cached
        .cache
        .open_fill(&fill, Some("bytes=1500-1509"))
        .await
        .expect("a reader on the download");
    assert_eq!(near.status_code, 206);
    assert_eq!(
        near.content_range.as_deref(),
        Some(format!("bytes 1500-1509/{total}").as_str())
    );
    // Far ahead: the caller asks Deezer for those bytes.
    let far = format!("bytes={}-", total - 10);
    assert!(cached.cache.open_fill(&fill, Some(&far)).await.is_none());
    // Past the end is past the end, wherever the download is.
    let past = format!("bytes={total}-");
    assert_eq!(
        cached
            .cache
            .open_fill(&fill, Some(&past))
            .await
            .expect("an answer")
            .status_code,
        416
    );

    let more = payload(2000);
    file.append(&more[1000..]).await.expect("written");
    assert_eq!(read(near).await, more[1500..1510]);
    file.fail("the test is over");
}

#[tokio::test]
async fn a_download_that_comes_short_is_not_kept_and_fails_its_readers() {
    let cached = Cached::new(payload(10)).await;
    let (file, fill) = half_made(&cached, b"", 10, "MP3_320").await;
    let staging = fill.source.path();
    let reader = fill.source.open_read(0, Some(10)).await.expect("opens");
    let short: AudioStream = Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(b"abcd"))]));

    assert!(!cached.cache.produce("42", &fill, file, short).await);

    assert!(!cached.copy().exists());
    assert!(!staging.exists());
    let chunks: std::io::Result<Vec<Bytes>> = reader.try_collect().await;
    assert!(chunks.is_err());
}

#[tokio::test]
async fn a_download_that_is_not_the_format_it_claims_is_not_kept() {
    let cached = Cached::new(payload(10)).await;
    let (file, fill) = half_made(&cached, b"", 8, "FLAC").await;
    let not_flac: AudioStream = Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(
        b"ID3\x04abcd",
    ))]));
    assert!(!cached.cache.produce("42", &fill, file, not_flac).await);
    assert!(!cached.cache.root().join("42.FLAC.flac").exists());

    let (file, fill) = half_made(&cached, b"", 8, "FLAC").await;
    let flac: AudioStream = Box::pin(futures::stream::iter(vec![
        Ok(Bytes::from_static(b"fL")),
        Ok(Bytes::from_static(b"aCabcd")),
    ]));
    assert!(cached.cache.produce("42", &fill, file, flac).await);
    assert_eq!(
        std::fs::read(cached.cache.root().join("42.FLAC.flac")).expect("the copy"),
        b"fLaCabcd"
    );
}

#[tokio::test]
async fn copies_not_played_for_the_retention_are_removed() {
    let cached = Cached::new(payload(10)).await;
    cached.configure(|settings| settings.cache_retention_days = 7);
    let old = cached.put("1.MP3_320.mp3", 10, 8 * DAY);
    let recent = cached.put("2.FLAC.flac", 10, 6 * DAY);

    assert_eq!(cached.cache.sweep().await, 1);

    assert!(!old.exists() && recent.exists());

    // 0 keeps them for as long as there is room.
    let older = cached.put("3.MP3_128.mp3", 10, 400 * DAY);
    cached.configure(|settings| settings.cache_retention_days = 0);
    assert_eq!(cached.cache.sweep().await, 0);
    assert!(older.exists());
}

#[tokio::test]
async fn past_the_size_limit_the_copies_played_longest_ago_go_first() {
    let cached = Cached::new(payload(10)).await;
    cached.configure(|settings| {
        settings.cache_retention_days = 0;
        settings.cache_max_gi_b = 2500.0 / (1024.0 * 1024.0 * 1024.0);
    });
    let oldest = cached.put("1.MP3_320.mp3", 1000, 5 * DAY);
    let older = cached.put("2.MP3_320.mp3", 1000, 4 * DAY);
    let newer = cached.put("3.MP3_320.mp3", 1000, 2 * DAY);
    let newest = cached.put("4.FLAC.flac", 1000, DAY);

    assert_eq!(cached.cache.sweep().await, 2);

    assert!(!oldest.exists() && !older.exists());
    assert!(newer.exists() && newest.exists());
    let status = cached.cache.status().await;
    assert_eq!((status.songs, status.bytes), (2, 2000));
    assert!(
        status.max_bytes >= 2499 && status.max_bytes <= 2500,
        "{}",
        status.max_bytes
    );
}

/// A cache folder pointed at the wrong place, the music library say, loses nothing: only the
/// names the cache gives are its own.
#[tokio::test]
async fn files_the_cache_did_not_name_are_never_counted_or_removed() {
    let cached = Cached::new(payload(10)).await;
    cached.configure(|settings| {
        settings.cache_retention_days = 1;
        settings.cache_max_gi_b = 0.0;
    });
    let foreign: Vec<PathBuf> = [
        "Song.flac",
        "01 - Track.mp3",
        "123.flac",
        "123.FLAC.mp3",
        "123.MP3_320.flac",
        "123.OGG.ogg",
        "abc.FLAC.flac",
        "0.FLAC.flac",
        "123.FLAC.flac.bak",
        "notes.txt",
        "Artist/7.FLAC.flac",
    ]
    .iter()
    .map(|name| cached.put(name, 50, 30 * DAY))
    .collect();
    let own = cached.put("123.FLAC.flac", 50, 30 * DAY);

    assert_eq!(cached.cache.status().await.songs, 1);
    assert_eq!(cached.cache.sweep().await, 1);
    assert_eq!(cached.cache.clear().await, 0);

    assert!(!own.exists());
    for path in foreign {
        assert!(path.exists(), "{}", path.display());
    }
}

#[tokio::test]
async fn emptying_the_cache_removes_every_copy_and_says_how_many() {
    let cached = Cached::new(payload(10)).await;
    cached.put("1.MP3_320.mp3", 10, DAY);
    cached.put("2.FLAC.flac", 30, DAY);
    let status = cached.cache.status().await;
    assert_eq!(
        (status.enabled, status.songs, status.bytes, status.filling),
        (true, 2, 40, 0)
    );
    assert_eq!(status.path, cached.cache.root().display().to_string());

    assert_eq!(cached.cache.clear().await, 2);
    assert_eq!(cached.cache.status().await.songs, 0);
    assert_eq!(cached.cache.clear().await, 0);
}

#[tokio::test]
async fn a_heart_takes_the_cached_copy_only_in_the_format_it_wants() {
    let cached = Cached::new(payload(10)).await;
    cached.put("42.MP3_320.mp3", 64, DAY);
    let dest = cached.dir.path().join("incoming/song");
    let dest = dest.to_str().expect("UTF-8");

    // A download asking for FLAC may be given FLAC: the MP3 copy is no substitute.
    assert!(cached.cache.copy_ready("42", "FLAC", dest).await.is_none());
    assert_eq!(
        cached.cache.copy_ready("42", "MP3_320", dest).await.as_deref(),
        Some(format!("{dest}.mp3").as_str())
    );
    assert_eq!(std::fs::read(format!("{dest}.mp3")).expect("the copy").len(), 64);
    assert!(cached.copy().exists());
    // Never over a file already there.
    assert!(cached.cache.copy_ready("42", "MP3_320", dest).await.is_none());
    assert!(cached.cache.copy_ready("../42", "MP3_320", dest).await.is_none());
}

#[tokio::test]
async fn starting_removes_what_an_earlier_run_left_half_downloaded() {
    let cached = Cached::new(payload(10)).await;
    let left = cached.put(".partial/42.abc.part", 10, DAY);
    let other = cached.put(".partial/keep.txt", 10, DAY);
    let copy = cached.put("42.MP3_320.mp3", 10, DAY);
    let token = CancellationToken::new();
    let worker = tokio::spawn(Arc::clone(&cached.cache).run(token.clone()));

    for _ in 0..200 {
        if !left.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    token.cancel();
    worker.await.expect("joined").expect("ran");

    assert!(!left.exists());
    assert!(other.exists() && copy.exists());
}

#[test]
fn only_the_names_the_cache_gives_are_copies() {
    for (name, expected) in [
        ("42.FLAC.flac", true),
        ("42.MP3_320.mp3", true),
        ("42.MP3_128.mp3", true),
        ("42.FLAC.mp3", false),
        ("42.flac", false),
        ("42.MP3_320.mp3.part", false),
        ("-4.FLAC.flac", false),
        ("x.FLAC.flac", false),
        ("", false),
    ] {
        assert_eq!(is_copy_name(name), expected, "{name}");
    }
}
