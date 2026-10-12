//! The playback cache: a copy of each song played from Deezer, kept outside the music library,
//! so a replay or a seek reads the copy and does not ask Deezer again.
//!
//! After am-yy/octo's `Octo.Services.Deezer.DeezerAudioCache`, cut to what main's playback needs:
//! no pins, no encoded variants, no priorities. What it keeps of the original:
//!
//! - One download per song, however many players ask. The first play starts it and reads the
//!   file as it grows ([`super::progressive_file`]); a second play of the same song joins it.
//! - A player that goes away ends only its own read. The download carries on, so the copy is
//!   there for the next play.
//! - A copy is published under its final name only whole: every byte Deezer promised, and the
//!   look of the format it claims. Anything short of that is deleted.
//! - A seek past what has arrived so far is answered by Deezer directly, while the download
//!   carries on.
//!
//! Copies are named `<track id>.<FORMAT>.<ext>` in the cache folder, and downloads in progress
//! live in its `.partial` folder. Only files with those names are ever counted or removed, so a
//! cache folder pointed at the wrong place loses nothing. A copy's modification time is when it
//! was last played: copies older than `Deezer:CacheRetentionDays` go, and past
//! `Deezer:CacheMaxGiB` the ones played longest ago go first. A copy being read when it is
//! removed plays to its end, since its reader holds the file open.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::TryStreamExt;
use octo_core::settings::deezer::{DEEZER_FORMAT_FLAC, DEEZER_FORMAT_MP3_128, DEEZER_FORMAT_MP3_320};
use octo_core::settings::{DeezerSettings, SettingsStore};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::deezer_resolver::{
    ByteWindow, DeezerOpenStream, DeezerResolver, byte_window, content_type, valid_track_id,
};
use super::progressive_file::{ProgressiveFile, ProgressiveSource};
use crate::services::framework::InFlight;
use crate::services::i_download_service::AudioStream;

/// The folder downloads in progress live in, inside the cache folder.
pub const PARTIAL_FOLDER: &str = ".partial";

/// A download in progress, and what is known of the song it brings.
struct Fill {
    source: ProgressiveSource,
    /// The Deezer format the account was given.
    format: String,
    /// The whole song's size in bytes, as Deezer promised it.
    total: u64,
}

/// What the dashboard shows of the cache.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeezerCacheStatus {
    pub enabled: bool,
    pub path: String,
    /// Whole copies kept.
    pub songs: usize,
    pub bytes: u64,
    pub max_bytes: u64,
    /// Songs downloading now.
    pub filling: usize,
}

pub struct DeezerAudioCache {
    resolver: Arc<DeezerResolver>,
    settings: Arc<SettingsStore>,
    /// Deliberately captured at construction: files are found again by where they were put.
    root: PathBuf,
    fills: Mutex<HashMap<String, Arc<Fill>>>,
    /// Downloads being started, by song: players that ask at the same moment share one start.
    starts: InFlight<Option<Arc<Fill>>>,
    /// How many songs download at once; a play past that is not cached this time.
    slots: Arc<Semaphore>,
    /// One sweep of the folder at a time.
    sweeping: tokio::sync::Mutex<()>,
    stopping: CancellationToken,
}

impl DeezerAudioCache {
    /// Songs downloading into the cache at once.
    pub const MAX_FILLS: usize = 4;
    /// How far past what has arrived a read may start and still wait for the download: less
    /// than a second of audio in any format.
    pub const WAIT_AHEAD: u64 = 256 * 1024;
    /// How often the folder is swept for copies past their time.
    pub const SWEEP_EVERY: Duration = Duration::from_secs(600);

    pub fn new(resolver: Arc<DeezerResolver>, settings: Arc<SettingsStore>) -> Self {
        let root = PathBuf::from(settings.current().deezer.effective_cache_path());
        Self::with_root(resolver, settings, root)
    }

    /// A cache in `root`, whatever the settings name.
    pub fn with_root(resolver: Arc<DeezerResolver>, settings: Arc<SettingsStore>, root: PathBuf) -> Self {
        Self {
            resolver,
            settings,
            root,
            fills: Mutex::new(HashMap::new()),
            starts: InFlight::new(),
            slots: Arc::new(Semaphore::new(Self::MAX_FILLS)),
            sweeping: tokio::sync::Mutex::new(()),
            stopping: CancellationToken::new(),
        }
    }

    // Read at use: the switch and the limits apply to the next play without a restart.
    fn deezer_settings(&self) -> DeezerSettings {
        self.settings.current().deezer.clone()
    }

    pub fn is_enabled(&self) -> bool {
        self.deezer_settings().cache_enabled
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn fill_key(track_id: &str, quality: &str) -> String {
        format!("{track_id}\0{}", quality.trim().to_ascii_uppercase())
    }

    fn ready_path(&self, track_id: &str, format: &str) -> PathBuf {
        self.root
            .join(format!("{track_id}.{format}.{}", extension(format)))
    }

    /// The whole copy of a track in the best format `quality` asks for that is here.
    fn ready(&self, track_id: &str, quality: &str) -> Option<(PathBuf, String)> {
        if !valid_track_id(track_id) {
            return None;
        }
        DeezerSettings::formats(quality).iter().find_map(|format| {
            let path = self.ready_path(track_id, format);
            path.is_file().then(|| (path, format.to_string()))
        })
    }

    /// The audio of a track, the whole of it or the bytes `range_header` asks for: from the
    /// copy when there is one, else from Deezer while a copy is made. With the cache off, or
    /// when a copy cannot be made this time, straight from Deezer. `None` when no account
    /// could be given the track.
    pub async fn open_stream(
        self: &Arc<Self>,
        track_id: &str,
        range_header: Option<&str>,
        quality: &str,
    ) -> Option<DeezerOpenStream> {
        if !self.is_enabled() || !valid_track_id(track_id) {
            return self.resolver.open_stream(track_id, range_header, quality).await;
        }
        if let Some((path, format)) = self.ready(track_id, quality) {
            match self.open_ready(&path, &format, range_header).await {
                Ok(opened) => return Some(opened),
                // Removed between the look and the read: made again below.
                Err(e) => debug!("Deezer cache copy of track {track_id} could not be read: {e}"),
            }
        }

        let key = Self::fill_key(track_id, quality);
        let running = self
            .fills
            .lock()
            .get(&key)
            .filter(|fill| !fill.source.has_failed())
            .cloned();
        let fill = match running {
            Some(fill) => Some(fill),
            None => {
                let (cache, track, wanted) = (Arc::clone(self), track_id.to_string(), quality.to_string());
                self.starts
                    .run(&key, async move { cache.start_fill(&track, &wanted).await })
                    .await
            }
        };
        if let Some(fill) = fill
            && let Some(opened) = self.open_fill(&fill, range_header).await
        {
            return Some(opened);
        }
        self.resolver.open_stream(track_id, range_header, quality).await
    }

    /// Starts the download of a track into the cache. `None` when it cannot be cached this
    /// time: Deezer has no copy for the account, it does not say how long the song is, there is
    /// no room or no free slot, or the folder cannot be written.
    async fn start_fill(self: &Arc<Self>, track_id: &str, quality: &str) -> Option<Arc<Fill>> {
        let key = Self::fill_key(track_id, quality);
        if let Some(fill) = self
            .fills
            .lock()
            .get(&key)
            .filter(|fill| !fill.source.has_failed())
        {
            return Some(Arc::clone(fill));
        }
        let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            debug!("Deezer cache is busy; track {track_id} plays without a copy being kept");
            return None;
        };
        let opened = self.resolver.open_stream(track_id, None, quality).await?;
        let total = opened.content_length.filter(|total| *total > 0)?;
        if total > self.deezer_settings().effective_cache_max_bytes() {
            return None;
        }
        let staging = self
            .root
            .join(PARTIAL_FOLDER)
            .join(format!("{track_id}.{}.part", uuid::Uuid::new_v4().simple()));
        let file = match ProgressiveFile::create(&staging).await {
            Ok(file) => file,
            Err(e) => {
                warn!(
                    "Deezer cache folder {} cannot be written ({e}); playing without it",
                    self.root.display()
                );
                return None;
            }
        };
        let fill = Arc::new(Fill {
            source: file.source(),
            format: opened.format.clone(),
            total,
        });
        self.fills.lock().insert(key.clone(), Arc::clone(&fill));

        let cache = Arc::clone(self);
        let running = Arc::clone(&fill);
        let track = track_id.to_string();
        // On its own task: the player that started it may leave, and the copy is still made.
        tokio::spawn(async move {
            let published = cache.produce(&track, &running, file, opened.audio_stream).await;
            {
                let mut fills = cache.fills.lock();
                if fills.get(&key).is_some_and(|now| Arc::ptr_eq(now, &running)) {
                    fills.remove(&key);
                }
            }
            drop(slot);
            if published {
                cache.sweep().await;
            }
        });
        Some(fill)
    }

    /// Reads Deezer's audio into the growing file, and publishes it when it is whole. True when
    /// the copy was published.
    async fn produce(
        &self,
        track_id: &str,
        fill: &Fill,
        mut file: ProgressiveFile,
        mut audio: AudioStream,
    ) -> bool {
        let staging = fill.source.path();
        let copied: anyhow::Result<()> = async {
            let mut written = 0u64;
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = self.stopping.cancelled() => anyhow::bail!("Octo is stopping."),
                    chunk = audio.try_next() => chunk?,
                };
                let Some(chunk) = chunk else { break };
                file.append(&chunk).await?;
                written += chunk.len() as u64;
                if written > fill.total {
                    anyhow::bail!("Deezer sent more than the {} bytes it promised.", fill.total);
                }
            }
            if written != fill.total {
                anyhow::bail!("Deezer sent {written} of {} bytes.", fill.total);
            }
            if !looks_like(&staging, &fill.format).await {
                anyhow::bail!("What Deezer sent is not {} audio.", fill.format);
            }
            file.publish(self.ready_path(track_id, &fill.format)).await?;
            Ok(())
        }
        .await;
        match copied {
            Ok(()) => {
                file.complete();
                debug!(
                    "Deezer cache kept track {track_id} ({}, {} bytes)",
                    fill.format, fill.total
                );
                true
            }
            Err(e) => {
                warn!("Deezer cache copy of track {track_id} failed: {e}");
                file.fail(e.to_string());
                let _ = tokio::fs::remove_file(&staging).await;
                false
            }
        }
    }

    /// A read of a download in progress. `None` when its file is gone; the caller asks Deezer.
    async fn open_fill(&self, fill: &Fill, range_header: Option<&str>) -> Option<DeezerOpenStream> {
        let total = fill.total;
        let (start, end, partial) = match byte_window(range_header, total) {
            None => (0, total - 1, false),
            Some(ByteWindow::Part { start, end }) => (start, end, true),
            Some(ByteWindow::Unsatisfiable) => {
                return Some(DeezerOpenStream::unsatisfiable(total, &fill.format));
            }
        };
        // A seek far past what has arrived would wait for the whole download to get there.
        if !fill.source.is_finished() && start > fill.source.length().saturating_add(Self::WAIT_AHEAD) {
            return None;
        }
        let length = end - start + 1;
        let audio_stream = fill.source.open_read(start, Some(length)).await.ok()?;
        Some(DeezerOpenStream {
            audio_stream,
            content_type: content_type(&fill.format).to_string(),
            content_length: Some(length),
            status_code: if partial { 206 } else { 200 },
            content_range: partial.then(|| format!("bytes {start}-{end}/{total}")),
            format: fill.format.clone(),
        })
    }

    /// A read of a whole copy, which counts as a play of it.
    async fn open_ready(
        &self,
        path: &Path,
        format: &str,
        range_header: Option<&str>,
    ) -> std::io::Result<DeezerOpenStream> {
        let mut file = tokio::fs::File::open(path).await?;
        let total = file.metadata().await?.len();
        if total == 0 {
            return Err(std::io::Error::other("an empty copy"));
        }
        touch(path);
        let (start, end, partial) = match byte_window(range_header, total) {
            None => (0, total - 1, false),
            Some(ByteWindow::Part { start, end }) => (start, end, true),
            Some(ByteWindow::Unsatisfiable) => return Ok(DeezerOpenStream::unsatisfiable(total, format)),
        };
        let length = end - start + 1;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let audio_stream: AudioStream = Box::pin(tokio_util::io::ReaderStream::new(file.take(length)));
        Ok(DeezerOpenStream {
            audio_stream,
            content_type: content_type(format).to_string(),
            content_length: Some(length),
            status_code: if partial { 206 } else { 200 },
            content_range: partial.then(|| format!("bytes {start}-{end}/{total}")),
            format: format.to_string(),
        })
    }

    /// Copies the whole copy of a track to `{dest_without_ext}.flac` or `.mp3`, when the cache
    /// holds it in exactly the first format `quality` asks for (a lesser copy is no substitute
    /// for a download that may get the better one). Never over a file already there. The path
    /// written, or `None` when there is no such copy.
    pub async fn copy_ready(&self, track_id: &str, quality: &str, dest_without_ext: &str) -> Option<String> {
        if !self.is_enabled() || !valid_track_id(track_id) {
            return None;
        }
        let format = DeezerSettings::formats(quality)[0];
        let source = self.ready_path(track_id, format);
        if !source.is_file() {
            return None;
        }
        let dest = format!("{dest_without_ext}.{}", extension(format));
        if Path::new(&dest).exists() {
            return None;
        }
        if let Some(parent) = Path::new(&dest).parent() {
            tokio::fs::create_dir_all(parent).await.ok()?;
        }
        match tokio::fs::copy(&source, &dest).await {
            Ok(size) if size > 0 => Some(dest),
            _ => {
                let _ = tokio::fs::remove_file(&dest).await;
                None
            }
        }
    }

    /// Removes the copies past `Deezer:CacheRetentionDays`, then the ones played longest ago
    /// until the rest fits `Deezer:CacheMaxGiB`. How many were removed.
    pub async fn sweep(&self) -> usize {
        let _one = self.sweeping.lock().await;
        let settings = self.deezer_settings();
        let (root, retention, max_bytes) = (
            self.root.clone(),
            settings.effective_cache_retention(),
            settings.effective_cache_max_bytes(),
        );
        tokio::task::spawn_blocking(move || {
            let now = SystemTime::now();
            let mut copies = copies_in(&root);
            // Oldest play first.
            copies.sort_by_key(|copy| copy.played);
            let mut kept = Vec::new();
            let mut removed = 0;
            for copy in copies {
                let expired =
                    retention.is_some_and(|keep| now.duration_since(copy.played).is_ok_and(|age| age > keep));
                if expired && std::fs::remove_file(&copy.path).is_ok() {
                    removed += 1;
                } else {
                    kept.push(copy);
                }
            }
            let mut held: u64 = kept.iter().map(|copy| copy.size).sum();
            for copy in kept {
                if held <= max_bytes {
                    break;
                }
                if std::fs::remove_file(&copy.path).is_ok() {
                    held -= copy.size;
                    removed += 1;
                }
            }
            removed
        })
        .await
        .unwrap_or(0)
    }

    /// Removes every whole copy. Downloads in progress carry on. How many were removed.
    pub async fn clear(&self) -> usize {
        let _one = self.sweeping.lock().await;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            copies_in(&root)
                .into_iter()
                .filter(|copy| std::fs::remove_file(&copy.path).is_ok())
                .count()
        })
        .await
        .unwrap_or(0)
    }

    pub async fn status(&self) -> DeezerCacheStatus {
        let settings = self.deezer_settings();
        let root = self.root.clone();
        let (songs, bytes) = tokio::task::spawn_blocking(move || {
            let copies = copies_in(&root);
            (copies.len(), copies.iter().map(|copy| copy.size).sum())
        })
        .await
        .unwrap_or((0, 0));
        DeezerCacheStatus {
            enabled: settings.cache_enabled,
            path: self.root.display().to_string(),
            songs,
            bytes,
            max_bytes: settings.effective_cache_max_bytes(),
            filling: self.fills.lock().len(),
        }
    }

    /// The worker: what an earlier run left half-downloaded is removed, then the folder is swept
    /// every [`Self::SWEEP_EVERY`]. Stopping ends the downloads in progress.
    pub async fn run(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        let partial = self.root.join(PARTIAL_FOLDER);
        let left = tokio::task::spawn_blocking(move || remove_partials(&partial))
            .await
            .unwrap_or(0);
        if left > 0 {
            info!("Deezer cache removed {left} unfinished download(s) from an earlier run");
        }
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(Self::SWEEP_EVERY) => {}
            }
            if self.is_enabled() {
                let removed = self.sweep().await;
                if removed > 0 {
                    debug!("Deezer cache removed {removed} song(s) past their time or the size limit");
                }
            }
        }
        self.stopping.cancel();
        Ok(())
    }
}

/// One whole copy in the cache folder.
struct Copy {
    path: PathBuf,
    size: u64,
    /// When it was last played: its modification time.
    played: SystemTime,
}

/// The file extension of a Deezer format.
fn extension(format: &str) -> &'static str {
    if format == DEEZER_FORMAT_FLAC {
        "flac"
    } else {
        "mp3"
    }
}

/// Whether a file name is one the cache gives a whole copy: `<digits>.<FORMAT>.<ext>` with the
/// extension of that format.
fn is_copy_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let (Some(id), Some(format), Some(ext), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    valid_track_id(id)
        && [DEEZER_FORMAT_FLAC, DEEZER_FORMAT_MP3_320, DEEZER_FORMAT_MP3_128].contains(&format)
        && ext == extension(format)
}

/// The whole copies directly inside `root`. Anything else there is not the cache's and is left
/// out.
fn copies_in(root: &Path) -> Vec<Copy> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            if !is_copy_name(name.to_str()?) {
                return None;
            }
            let metadata = entry.metadata().ok().filter(|m| m.is_file())?;
            Some(Copy {
                path: entry.path(),
                size: metadata.len(),
                played: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect()
}

/// Removes the `.part` files of `partial`. How many were removed.
fn remove_partials(partial: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(partial) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "part")
                && entry.metadata().is_ok_and(|m| m.is_file())
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Marks a copy as played now. Best effort: a copy that cannot be marked is only removed sooner.
fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// The first bytes are what the format starts with: `fLaC` for a FLAC; an MP3 has no one
/// beginning, so any bytes pass.
async fn looks_like(path: &Path, format: &str) -> bool {
    if format != DEEZER_FORMAT_FLAC {
        return true;
    }
    let mut magic = [0u8; 4];
    match tokio::fs::File::open(path).await {
        Ok(mut file) => file.read_exact(&mut magic).await.is_ok() && &magic == b"fLaC",
        Err(_) => false,
    }
}

#[cfg(test)]
#[path = "deezer_audio_cache_tests.rs"]
mod tests;
