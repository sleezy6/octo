//! `Octo.Tests.DeezerDecryptedStreamTests`.

use futures::TryStreamExt;

use super::*;

const TRACK_ID: &str = "1234567890";

fn make_plaintext(length: usize) -> Vec<u8> {
    (0..length).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

/// `bytes` as a body that arrives in pieces of the given sizes, over and over.
fn fragmented(bytes: &[u8], sizes: &[usize]) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let mut pieces = Vec::new();
    let mut rest = bytes;
    let mut turn = 0;
    while !rest.is_empty() {
        let take = sizes[turn % sizes.len()].min(rest.len());
        pieces.push(Ok(Bytes::copy_from_slice(&rest[..take])));
        rest = &rest[take..];
        turn += 1;
    }
    futures::stream::iter(pieces)
}

async fn drain(stream: AudioStream) -> Vec<u8> {
    let chunks: Vec<Bytes> = stream.try_collect().await.expect("the stream reads to its end");
    chunks.concat()
}

#[test]
fn the_key_is_made_from_the_track_id() {
    assert_eq!(
        blowfish_key(TRACK_ID),
        [
            59, 110, 55, 107, 98, 49, 114, 97, 96, 117, 119, 58, 105, 108, 106, 49
        ]
    );
}

#[test]
fn encrypted_stripes_differ_from_their_audio() {
    let plaintext = make_plaintext(4 * STRIPE);
    let stored = encrypt_stripes(&plaintext, TRACK_ID);
    assert_ne!(stored[..STRIPE], plaintext[..STRIPE]);
    assert_eq!(stored[STRIPE..3 * STRIPE], plaintext[STRIPE..3 * STRIPE]);
    assert_ne!(stored[3 * STRIPE..], plaintext[3 * STRIPE..]);
}

#[tokio::test]
async fn decrypts_encrypted_stripes_across_fragmented_reads_and_leaves_short_tail_plaintext() {
    let plaintext = make_plaintext(5 * STRIPE + 317);
    let stored = encrypt_stripes(&plaintext, TRACK_ID);
    assert_eq!(stored[5 * STRIPE..], plaintext[5 * STRIPE..]);
    let source = fragmented(&stored, &[1, 13, 777, 5, 2031, 89]);

    let actual = drain(decrypted(source, StripeDecryptor::whole(TRACK_ID))).await;

    assert_eq!(actual, plaintext);
}

#[tokio::test]
async fn a_whole_last_stripe_on_the_third_is_decrypted() {
    let plaintext = make_plaintext(4 * STRIPE);
    let stored = encrypt_stripes(&plaintext, TRACK_ID);
    let source = fragmented(&stored, &[4096, 4096]);

    assert_eq!(
        drain(decrypted(source, StripeDecryptor::whole(TRACK_ID))).await,
        plaintext
    );
}

#[tokio::test]
async fn range_uses_absolute_stripe_index_then_skips_and_caps_plaintext() {
    const START_CHUNK: usize = 3;
    const SKIP: usize = 73;
    const LENGTH: usize = 431;
    let plaintext = make_plaintext(6 * STRIPE + 91);
    let stored = encrypt_stripes(&plaintext, TRACK_ID);
    let range_offset = START_CHUNK * STRIPE;
    let source = fragmented(&stored[range_offset..], &[19, 2047, 43]);
    let decryptor = StripeDecryptor::new(TRACK_ID, START_CHUNK as u64, SKIP as u64, Some(LENGTH as u64));

    let actual = drain(decrypted(source, decryptor)).await;

    assert_eq!(
        actual,
        plaintext[range_offset + SKIP..range_offset + SKIP + LENGTH]
    );
}

#[tokio::test]
async fn a_range_starting_on_a_plain_stripe_reads_to_the_end() {
    let plaintext = make_plaintext(7 * STRIPE + 5);
    let stored = encrypt_stripes(&plaintext, TRACK_ID);
    let offset = 4 * STRIPE + 1000;
    let aligned = 4 * STRIPE;
    let source = fragmented(&stored[aligned..], &[333]);
    let decryptor = StripeDecryptor::new(TRACK_ID, 4, 1000, Some((plaintext.len() - offset) as u64));

    assert_eq!(drain(decrypted(source, decryptor)).await, plaintext[offset..]);
}

#[tokio::test]
async fn an_empty_source_is_an_empty_song() {
    let source = futures::stream::iter(Vec::<std::io::Result<Bytes>>::new());
    assert!(
        drain(decrypted(source, StripeDecryptor::whole(TRACK_ID)))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_read_that_fails_ends_the_song_with_that_failure() {
    let plaintext = make_plaintext(STRIPE);
    let stored = encrypt_stripes(&plaintext, TRACK_ID);
    let source = futures::stream::iter(vec![
        Ok(Bytes::from(stored)),
        Err(std::io::Error::other("the connection dropped")),
        Ok(Bytes::from_static(b"never read")),
    ]);
    let mut stream = decrypted(source, StripeDecryptor::whole(TRACK_ID));

    assert_eq!(stream.next().await.expect("a chunk").expect("audio"), plaintext);
    assert!(stream.next().await.expect("the failure").is_err());
    assert!(stream.next().await.is_none());
}
