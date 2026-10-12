//! `Octo.Services.Deezer.ProgressiveFile`: a file that is still being written, read by any
//! number of readers that wait for the bytes not yet there.
//!
//! One writer appends and flushes; each reader has its own handle and position, reads what has
//! been flushed, and waits when it catches up. A clean end gives every reader the end of the
//! file; a failed one gives them the bytes that did arrive and then the failure. The file can be
//! moved to its final name while readers are on it: they keep the handle they opened.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::services::i_download_service::AudioStream;

/// How much one read hands out at most.
const READ_CHUNK: usize = 64 * 1024;
/// How many reads in a row may give nothing where bytes were flushed.
const EMPTY_READS: u32 = 200;

/// How far the writer has come.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Progress {
    /// Bytes flushed and available to readers.
    length: u64,
    finished: bool,
    /// Why the writer stopped short, when it did.
    failure: Option<String>,
}

struct Shared {
    path: Mutex<PathBuf>,
    progress: watch::Sender<Progress>,
}

/// The writer's end. Dropped before [`ProgressiveFile::complete`], it fails the file, so no
/// reader waits for bytes that will never come.
pub struct ProgressiveFile {
    writer: Option<tokio::fs::File>,
    shared: Arc<Shared>,
}

/// A way to open readers on a [`ProgressiveFile`], which outlives the writer.
#[derive(Clone)]
pub struct ProgressiveSource {
    shared: Arc<Shared>,
}

impl ProgressiveFile {
    /// Creates the file, and its folder; never over a file already there.
    pub async fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let writer = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        let (progress, _) = watch::channel(Progress::default());
        Ok(Self {
            writer: Some(writer),
            shared: Arc::new(Shared {
                path: Mutex::new(path),
                progress,
            }),
        })
    }

    pub fn source(&self) -> ProgressiveSource {
        ProgressiveSource {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Writes and flushes `bytes`, then lets the readers have them.
    pub async fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Progressive file already finished."))?;
        writer.write_all(bytes).await?;
        writer.flush().await?;
        self.shared
            .progress
            .send_modify(|progress| progress.length += bytes.len() as u64);
        Ok(())
    }

    /// Moves the file to its final name, over any file there. Readers keep their open handles;
    /// readers opened from now on open the new name.
    pub async fn publish(&mut self, final_path: impl AsRef<Path>) -> std::io::Result<()> {
        let final_path = final_path.as_ref().to_path_buf();
        if let Some(writer) = self.writer.as_mut() {
            writer.flush().await?;
            writer.sync_data().await?;
        }
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let from = self.shared.path.lock().clone();
        tokio::fs::rename(&from, &final_path).await?;
        *self.shared.path.lock() = final_path;
        Ok(())
    }

    /// A clean end: readers reach the end of the file once they have read every byte.
    pub fn complete(mut self) {
        self.writer = None;
        self.shared
            .progress
            .send_modify(|progress| progress.finished = true);
    }

    /// The writer stopped short: readers get the bytes that arrived, then `message` as an error.
    pub fn fail(mut self, message: impl Into<String>) {
        self.writer = None;
        self.end_failed(message.into());
    }

    fn end_failed(&self, message: String) {
        self.shared.progress.send_modify(|progress| {
            if !progress.finished {
                progress.finished = true;
                progress.failure = Some(message);
            }
        });
    }
}

impl Drop for ProgressiveFile {
    fn drop(&mut self) {
        if self.writer.is_some() {
            self.end_failed("The download stopped before it finished.".to_string());
        }
    }
}

impl ProgressiveSource {
    /// Where the file is now.
    pub fn path(&self) -> PathBuf {
        self.shared.path.lock().clone()
    }

    /// Bytes flushed so far.
    pub fn length(&self) -> u64 {
        self.shared.progress.borrow().length
    }

    /// The writer has ended, cleanly or not.
    pub fn is_finished(&self) -> bool {
        self.shared.progress.borrow().finished
    }

    /// The writer ended short.
    pub fn has_failed(&self) -> bool {
        self.shared.progress.borrow().failure.is_some()
    }

    /// Waits for the writer to end. True when it ended cleanly.
    pub async fn finished(&self) -> bool {
        let mut progress = self.shared.progress.subscribe();
        loop {
            {
                let now = progress.borrow_and_update();
                if now.finished {
                    return now.failure.is_none();
                }
            }
            if progress.changed().await.is_err() {
                return false;
            }
        }
    }

    /// A reader from `offset`, of `length` bytes or to the end. The handle is opened here, so a
    /// later move or removal of the file does not reach this reader. A reader with a length ends
    /// at it without waiting for the writer's end.
    pub async fn open_read(&self, offset: u64, length: Option<u64>) -> std::io::Result<AudioStream> {
        let path = self.path();
        let mut file = tokio::fs::File::open(&path).await?;
        file.seek(SeekFrom::Start(offset)).await?;
        let state = Reader {
            file,
            position: offset,
            end: length.map(|length| offset.saturating_add(length)),
            progress: self.shared.progress.subscribe(),
            ended: false,
            empty_reads: 0,
        };
        Ok(Box::pin(futures::stream::unfold(
            state,
            |mut reader| async move {
                let item = reader.next().await?;
                Some((item, reader))
            },
        )))
    }
}

struct Reader {
    file: tokio::fs::File,
    position: u64,
    end: Option<u64>,
    progress: watch::Receiver<Progress>,
    ended: bool,
    /// Reads in a row that gave nothing although bytes were flushed.
    empty_reads: u32,
}

impl Reader {
    async fn next(&mut self) -> Option<std::io::Result<Bytes>> {
        loop {
            if self.ended || self.end.is_some_and(|end| self.position >= end) {
                return None;
            }
            let now = self.progress.borrow_and_update().clone();
            let available = now.length.saturating_sub(self.position);
            if available == 0 {
                if now.finished {
                    self.ended = true;
                    return now.failure.map(|failure| {
                        Err(std::io::Error::other(format!(
                            "Progressive source failed: {failure}"
                        )))
                    });
                }
                // The writer is gone without a word: nothing more will come.
                if self.progress.changed().await.is_err() {
                    self.ended = true;
                    return Some(Err(std::io::Error::other("Progressive source failed.")));
                }
                continue;
            }
            let mut want = available.min(READ_CHUNK as u64);
            if let Some(end) = self.end {
                want = want.min(end - self.position);
            }
            let mut buffer = vec![0u8; want as usize];
            match self.file.read(&mut buffer).await {
                // Flushed bytes are readable; a handle that lags the flush is asked again, a
                // few times. A file cut short under the reader never has them.
                Ok(0) => {
                    self.empty_reads += 1;
                    if self.empty_reads > EMPTY_READS {
                        self.ended = true;
                        return Some(Err(std::io::Error::other("Progressive file was cut short.")));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Ok(read) => {
                    self.empty_reads = 0;
                    buffer.truncate(read);
                    self.position += read as u64;
                    return Some(Ok(Bytes::from(buffer)));
                }
                Err(e) => {
                    self.ended = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "progressive_file_tests.rs"]
mod tests;
