//! Getting upcoming songs ahead: the songs that play next fetched as real FLAC copies into a
//! folder of their own before they play, served to the players that can take them, and kept by a
//! heart without a second download.

pub mod play_source;
pub mod prefetch_service;
pub mod queue_window;
pub mod store;

pub use play_source::{PlaySources, Source, from_start};
pub use prefetch_service::{JobView, ONE_AT_A_TIME, PrefetchParts, PrefetchService, PrefetchStatus};
pub use queue_window::{SavedQueue, window};
pub use store::{
    AheadSidecar, FOLDER_MARKER, FolderFn, PrefetchStore, PrefetchedFiles, ReadySong, Taken,
    VerificationMirror, file_name, free_bytes, owns_folder, prepare_folder, sidecar_path, write_sidecar,
};

#[cfg(test)]
#[path = "prefetch_service_tests.rs"]
mod prefetch_service_tests;
