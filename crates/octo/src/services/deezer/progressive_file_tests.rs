//! `ProgressiveFile`: readers on a file that is still being written.

use futures::{StreamExt, TryStreamExt};

use super::*;

async fn drain(stream: AudioStream) -> std::io::Result<Vec<u8>> {
    let chunks: Vec<Bytes> = stream.try_collect().await?;
    Ok(chunks.concat())
}

fn folder() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temp folder")
}

#[tokio::test]
async fn a_reader_waits_for_bytes_and_ends_with_the_writer() {
    let dir = folder();
    let mut file = ProgressiveFile::create(dir.path().join("nested/song.part"))
        .await
        .expect("created");
    let source = file.source();
    let reader = tokio::spawn({
        let source = source.clone();
        async move { drain(source.open_read(0, None).await.expect("opens")).await }
    });

    file.append(b"0123").await.expect("written");
    tokio::task::yield_now().await;
    assert!(!reader.is_finished());
    file.append(b"4567").await.expect("written");
    assert_eq!(source.length(), 8);
    assert!(!source.is_finished());
    file.complete();

    assert_eq!(reader.await.expect("joined").expect("read"), b"01234567");
    assert!(source.is_finished() && !source.has_failed());
    assert!(source.finished().await);
}

#[tokio::test]
async fn readers_have_their_own_positions() {
    let dir = folder();
    let mut file = ProgressiveFile::create(dir.path().join("song.part"))
        .await
        .expect("created");
    let source = file.source();
    file.append(b"0123456789").await.expect("written");
    file.complete();

    let (whole, tail, middle) = tokio::join!(
        async { drain(source.open_read(0, None).await.expect("opens")).await },
        async { drain(source.open_read(7, None).await.expect("opens")).await },
        async { drain(source.open_read(2, Some(3)).await.expect("opens")).await },
    );
    assert_eq!(whole.expect("read"), b"0123456789");
    assert_eq!(tail.expect("read"), b"789");
    assert_eq!(middle.expect("read"), b"234");
}

#[tokio::test]
async fn a_bounded_reader_ends_at_its_length_while_the_writer_goes_on() {
    let dir = folder();
    let mut file = ProgressiveFile::create(dir.path().join("song.part"))
        .await
        .expect("created");
    let source = file.source();
    file.append(b"0123456789").await.expect("written");

    let part = drain(source.open_read(4, Some(4)).await.expect("opens"))
        .await
        .expect("read");
    assert_eq!(part, b"4567");
    assert!(!source.is_finished());
    file.complete();
}

#[tokio::test]
async fn a_failed_writer_gives_what_arrived_and_then_the_failure() {
    let dir = folder();
    let mut file = ProgressiveFile::create(dir.path().join("song.part"))
        .await
        .expect("created");
    let source = file.source();
    let mut reader = source.open_read(0, None).await.expect("opens");
    file.append(b"abc").await.expect("written");
    file.fail("the connection dropped");

    assert_eq!(reader.next().await.expect("a chunk").expect("bytes"), "abc");
    let error = reader.next().await.expect("the failure").expect_err("failed");
    assert!(error.to_string().contains("the connection dropped"), "{error}");
    assert!(reader.next().await.is_none());
    assert!(source.has_failed());
    assert!(!source.finished().await);
}

#[tokio::test]
async fn a_writer_dropped_midway_fails_its_readers() {
    let dir = folder();
    let mut file = ProgressiveFile::create(dir.path().join("song.part"))
        .await
        .expect("created");
    let source = file.source();
    file.append(b"abc").await.expect("written");
    drop(file);

    assert!(source.has_failed());
    assert!(
        drain(source.open_read(0, None).await.expect("opens"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn publishing_moves_the_file_under_its_readers() {
    let dir = folder();
    let staging = dir.path().join("staging/song.part");
    let published = dir.path().join("ready/song.mp3");
    let mut file = ProgressiveFile::create(&staging).await.expect("created");
    let source = file.source();
    file.append(b"0123").await.expect("written");
    // Opened on the staging name, and still reading after the move.
    let before = source.open_read(0, None).await.expect("opens");

    file.publish(&published).await.expect("published");
    file.complete();

    assert!(!staging.exists());
    assert_eq!(std::fs::read(&published).expect("the file"), b"0123");
    assert_eq!(source.path(), published);
    assert_eq!(drain(before).await.expect("read"), b"0123");
    let after = source.open_read(1, None).await.expect("opens the new name");
    assert_eq!(drain(after).await.expect("read"), b"123");
}

#[tokio::test]
async fn a_file_already_there_is_never_written_over() {
    let dir = folder();
    let path = dir.path().join("song.part");
    std::fs::write(&path, b"kept").expect("written");
    assert!(ProgressiveFile::create(&path).await.is_err());
    assert_eq!(std::fs::read(&path).expect("the file"), b"kept");
}

#[tokio::test]
async fn a_removed_file_is_still_read_by_the_reader_that_had_it_open() {
    let dir = folder();
    let path = dir.path().join("song.part");
    let mut file = ProgressiveFile::create(&path).await.expect("created");
    let source = file.source();
    file.append(b"0123").await.expect("written");
    file.complete();
    let reader = source.open_read(0, None).await.expect("opens");

    std::fs::remove_file(&path).expect("removed");

    assert_eq!(drain(reader).await.expect("read"), b"0123");
    assert!(source.open_read(0, None).await.is_err());
}
