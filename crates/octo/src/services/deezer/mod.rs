//! Deezer as a source of audio: an account's ARL cookie, the media address of a track, and the
//! stripe decryption its bytes need. The catalog side (facts about songs and albums, no account)
//! is `services::metadata::deezer_metadata_service`.
//!
//! Ported from am-yy/octo's `Octo.Services.Deezer` (`DeezerResolver`, `DeezerDecryptedStream`,
//! `ProgressiveFile`, and a cut-down `DeezerAudioCache`), which adapted the first two from
//! V1ck3s/octo-fiesta (GPL-3.0).

pub mod deezer_account_report;
pub mod deezer_audio_cache;
pub mod deezer_decrypted_stream;
pub mod deezer_resolver;
pub mod progressive_file;
#[cfg(test)]
pub(crate) mod test_support;

pub use deezer_account_report::DeezerAccountReport;
pub use deezer_audio_cache::{DeezerAudioCache, DeezerCacheStatus};
pub use deezer_decrypted_stream::{STRIPE, StripeDecryptor, decrypted};
pub use deezer_resolver::{DeezerAccountHealth, DeezerOpenStream, DeezerResolver};
