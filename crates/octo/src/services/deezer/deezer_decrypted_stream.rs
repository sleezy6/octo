//! `Octo.Services.Deezer.DeezerDecryptedStream`: Deezer's stripe cipher, undone as the bytes
//! arrive.
//!
//! Adapted from V1ck3s/octo-fiesta (GPL-3.0), with aligned byte-range support. A track's bytes
//! are cut into 2048-byte stripes; every third stripe (0, 3, 6, ...) that is a whole stripe is
//! Blowfish-CBC encrypted on its own, with a key made from the track's id. The last, short
//! stripe is never encrypted.

use blowfish::Blowfish;
use blowfish::cipher::{Block, BlockCipherDecrypt, KeyInit};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use md5::{Digest, Md5};

use crate::services::i_download_service::AudioStream;

/// The length of one stripe.
pub const STRIPE: usize = 2048;

// Deezer's standard Blowfish CBC encryption key for track decryption
// This is a well-known constant used by the Deezer API, not a user-specific secret
const BF_SECRET: &[u8; 16] = b"g4el58wc0zvf9na1";
const IV: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
const BLOCK: usize = 8;

/// The Blowfish key of a track: the two halves of its id's MD5 (as lower-case hex) and the
/// secret, byte by byte.
fn blowfish_key(track_id: &str) -> [u8; 16] {
    let hash_hex = hex::encode(Md5::digest(track_id.as_bytes()));
    let hash_hex = hash_hex.as_bytes();
    let mut key = [0u8; 16];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = hash_hex[i] ^ hash_hex[i + 16] ^ BF_SECRET[i];
    }
    key
}

/// Turns a track's stored bytes into its audio, whatever sizes the bytes arrive in.
///
/// `start_stripe` is the index of the first stripe fed in (a ranged read starts on a stripe
/// boundary, not at 0), `skip` how many decrypted bytes to drop before the first one handed
/// out, and `length` how many to hand out in all (`None`: to the end).
pub struct StripeDecryptor {
    cipher: Blowfish,
    stripe_index: u64,
    skip: u64,
    remaining: Option<u64>,
    /// Bytes of a stripe still short of whole.
    pending: Vec<u8>,
}

impl StripeDecryptor {
    pub fn new(track_id: &str, start_stripe: u64, skip: u64, length: Option<u64>) -> Self {
        Self {
            cipher: Blowfish::new_from_slice(&blowfish_key(track_id)).expect("a 16-byte Blowfish key"),
            stripe_index: start_stripe,
            skip,
            remaining: length,
            pending: Vec::with_capacity(STRIPE),
        }
    }

    /// A decryptor of a whole track from its first byte.
    pub fn whole(track_id: &str) -> Self {
        Self::new(track_id, 0, 0, None)
    }

    /// Every byte asked for has been handed out.
    pub fn is_done(&self) -> bool {
        self.remaining == Some(0)
    }

    /// Takes the next bytes read and appends to `out` the audio of every stripe they completed.
    // Fill whole stripes before decryption, even when HTTP reads are fragmented.
    pub fn push(&mut self, mut input: &[u8], out: &mut Vec<u8>) {
        while !input.is_empty() && !self.is_done() {
            let take = (STRIPE - self.pending.len()).min(input.len());
            self.pending.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.pending.len() == STRIPE {
                let mut stripe = std::mem::replace(&mut self.pending, Vec::with_capacity(STRIPE));
                // Every 3rd chunk (index % 3 == 0) is encrypted
                if self.stripe_index.is_multiple_of(3) {
                    self.decrypt_blowfish_cbc(&mut stripe);
                }
                self.emit(&stripe, out);
            }
        }
    }

    /// The source ended: the last, short stripe goes out as it is.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.pending.is_empty() {
            let stripe = std::mem::take(&mut self.pending);
            self.emit(&stripe, out);
        }
    }

    fn emit(&mut self, stripe: &[u8], out: &mut Vec<u8>) {
        self.stripe_index += 1;
        let discarded = self.skip.min(stripe.len() as u64) as usize;
        self.skip -= discarded as u64;
        let mut audio = &stripe[discarded..];
        if let Some(remaining) = self.remaining {
            let keep = remaining.min(audio.len() as u64) as usize;
            audio = &audio[..keep];
            self.remaining = Some(remaining - keep as u64);
        }
        out.extend_from_slice(audio);
    }

    // Each stripe is its own CBC message: the IV starts it afresh.
    fn decrypt_blowfish_cbc(&self, data: &mut [u8]) {
        let mut previous = IV;
        for chunk in data.as_chunks_mut::<BLOCK>().0 {
            let cipher_text = *chunk;
            let mut block = Block::<Blowfish>::default();
            block.copy_from_slice(chunk);
            self.cipher.decrypt_block(&mut block);
            for (i, byte) in chunk.iter_mut().enumerate() {
                *byte = block[i] ^ previous[i];
            }
            previous = cipher_text;
        }
    }
}

/// `source` decrypted: an audio body that reads the stored bytes as it is read. It ends when
/// `source` does, or as soon as the decryptor has handed out its length (dropping `source`,
/// which closes the response it reads from). A read that fails ends it with that failure.
pub fn decrypted<S>(source: S, decryptor: StripeDecryptor) -> AudioStream
where
    S: Stream<Item = std::io::Result<Bytes>> + Send + 'static,
{
    struct State<S> {
        source: std::pin::Pin<Box<S>>,
        decryptor: StripeDecryptor,
        ended: bool,
    }
    let state = State {
        source: Box::pin(source),
        decryptor,
        ended: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if state.ended || state.decryptor.is_done() {
                return None;
            }
            let mut out = Vec::new();
            match state.source.next().await {
                Some(Ok(chunk)) => state.decryptor.push(&chunk, &mut out),
                Some(Err(e)) => {
                    state.ended = true;
                    return Some((Err(e), state));
                }
                None => {
                    state.ended = true;
                    state.decryptor.finish(&mut out);
                }
            }
            if !out.is_empty() {
                return Some((Ok(Bytes::from(out)), state));
            }
        }
    }))
}

/// What Deezer stores for `plaintext`: every third whole stripe Blowfish-CBC encrypted on its
/// own. For the tests of everything that reads a track.
#[cfg(test)]
pub(crate) fn encrypt_stripes(plaintext: &[u8], track_id: &str) -> Vec<u8> {
    use blowfish::cipher::BlockCipherEncrypt;
    let cipher: Blowfish = Blowfish::new_from_slice(&blowfish_key(track_id)).expect("a 16-byte Blowfish key");
    let mut stored = plaintext.to_vec();
    for (index, stripe) in stored.chunks_mut(STRIPE).enumerate() {
        if !index.is_multiple_of(3) || stripe.len() != STRIPE {
            continue;
        }
        let mut previous = IV;
        for chunk in stripe.as_chunks_mut::<BLOCK>().0 {
            let mut block = Block::<Blowfish>::default();
            for (i, byte) in block.iter_mut().enumerate() {
                *byte = chunk[i] ^ previous[i];
            }
            cipher.encrypt_block(&mut block);
            chunk.copy_from_slice(&block);
            previous = *chunk;
        }
    }
    stored
}

#[cfg(test)]
#[path = "deezer_decrypted_stream_tests.rs"]
mod tests;
