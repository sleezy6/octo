//! Charts and new songs from Apple Music's public pages: fetching them, and keeping them warm.
//! The pure half (parsing, settings, merging) is `octo_core::charts`.

pub mod apple_music_pages;
pub mod chart_country_store;
pub mod charts_warm_worker;
pub mod genre_names;

pub use apple_music_pages::AppleMusicPages;
pub use chart_country_store::{ChartCountryStore, ListenerCharts};
pub use charts_warm_worker::ChartsWarmWorker;
pub use genre_names::GenreNames;
