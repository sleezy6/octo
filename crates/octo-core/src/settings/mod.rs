//! The settings classes, the live store that binds them from the layered configuration, and
//! the admin helpers that edit `settings.json` (`SettingsFileWriter`, `RestartTracker`).
//!
//! Each struct derives `Deserialize` with `#[serde(default)]` and carries its defaults in its
//! `Default`; [`crate::config::bind`] fills it from a configuration section. `effective_*`
//! methods clamp at read time.

pub mod admin;
pub mod deezer;
pub mod family;
pub mod generated_playlist;
pub mod genre;
pub mod import;
pub mod last_fm;
pub mod library_action;
pub mod lidarr;
pub mod listen_brainz;
pub mod metadata;
pub mod notification;
pub mod radio_source;
pub mod restart_tracker;
pub mod server;
pub mod soulseek;
pub mod store;
pub mod subsonic;
mod text;
pub mod update;
pub mod writer;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::{BindWarning, ConfigTree, bind};

pub use admin::AdminSettings;
pub use deezer::DeezerSettings;
pub use family::FamilySettings;
pub use generated_playlist::GeneratedPlaylistSettings;
pub use genre::{
    GenreEmptyBehavior, GenreFallbackSource, GenreMappingSettings, GenreMatchMode, GenreSettings,
};
pub use import::ImportSettings;
pub use last_fm::{DiscoveryStationSettings, LastFmSettings, LastFmUserSession};
pub use library_action::{
    LibraryAction, LibraryActionDefinition, LibraryActionSettings, LibraryActionTrigger, LibraryRatingScope,
    NoticeKind, UpgradeSourceChoice,
};
pub use lidarr::{LidarrCompletionMode, LidarrSettings};
pub use listen_brainz::ListenBrainzSettings;
pub use metadata::{AnimatedArtMode, LyricsSaveTo, MetadataSettings};
pub use notification::NotificationSettings;
pub use radio_source::RadioSourceSettings;
pub use restart_tracker::RestartTracker;
pub use server::ServerSettings;
pub use soulseek::SoulseekSettings;
pub use store::{RawConfig, SettingsStore};
pub use subsonic::{
    DownloadMode, DownloadSource, ExplicitFilter, FolderStructure, HeartDownloadSource, HeartDownloadStep,
    StorageMode, StreamSource, SubsonicSettings,
};
pub use text::IgnoreCaseSet;
pub use update::UpdateSettings;
pub use writer::{JsonObject, SettingsFileCorruptError, SettingsFileWriter, SettingsWriteError};

/// Every bound settings section, as one snapshot. Each class is bound from the configuration
/// section of its name; the section names are the field names in PascalCase.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AppSettings {
    pub subsonic: SubsonicSettings,
    pub soulseek: SoulseekSettings,
    pub lidarr: LidarrSettings,
    pub deezer: DeezerSettings,
    pub last_fm: LastFmSettings,
    pub radio_sources: RadioSourceSettings,
    pub genre: GenreSettings,
    pub library_actions: LibraryActionSettings,
    pub generated_playlists: GeneratedPlaylistSettings,
    pub notifications: NotificationSettings,
    pub metadata: MetadataSettings,
    pub server: ServerSettings,
    pub listen_brainz: ListenBrainzSettings,
    pub updates: UpdateSettings,
    pub admin: AdminSettings,
    pub imports: ImportSettings,
    pub family: FamilySettings,
}

impl AppSettings {
    /// What outside songs are declared as when they play from Deezer: `Subsonic:StreamSource`
    /// is Deezer and an account is set. `None` while they play from YouTube. The app reads this
    /// once, at startup: it decides what every row declares and where every play is asked for,
    /// and the two must not part ways under a client that has cached its rows.
    pub fn deezer_stream(&self) -> Option<crate::soulseek::stream_facts::DeclaredStream> {
        self.deezer_stream_when(true)
    }

    /// [`Self::deezer_stream`] on a server where YouTube may be switched off: then outside songs
    /// play from Deezer whatever `StreamSource` says, when an account is set, since YouTube
    /// has nothing to play them from.
    pub fn deezer_stream_when(
        &self,
        you_tube_on: bool,
    ) -> Option<crate::soulseek::stream_facts::DeclaredStream> {
        ((self.subsonic.stream_source == StreamSource::Deezer || !you_tube_on) && self.deezer.is_configured())
            .then(|| self.deezer.declared_stream())
    }

    /// The configuration section names, in registration order.
    pub const SECTIONS: [&'static str; 17] = [
        "Genre",
        "LibraryActions",
        "GeneratedPlaylists",
        "Subsonic",
        "Soulseek",
        "Lidarr",
        "Deezer",
        "LastFm",
        "RadioSources",
        "Notifications",
        "Metadata",
        "Server",
        "ListenBrainz",
        "Updates",
        "Admin",
        "Imports",
        "Family",
    ];

    /// Binds every section from the merged configuration. A value that does not convert
    /// keeps its default (or a lower layer's value) and comes back as a warning whose path
    /// starts with the section name.
    pub fn bind(tree: &ConfigTree) -> (AppSettings, Vec<BindWarning>) {
        let mut warnings = Vec::new();
        let mut settings = AppSettings {
            genre: bind_section(tree, "Genre", &mut warnings),
            library_actions: bind_section(tree, "LibraryActions", &mut warnings),
            generated_playlists: bind_section(tree, "GeneratedPlaylists", &mut warnings),
            subsonic: bind_section(tree, "Subsonic", &mut warnings),
            soulseek: bind_section(tree, "Soulseek", &mut warnings),
            lidarr: bind_section(tree, "Lidarr", &mut warnings),
            deezer: bind_section(tree, "Deezer", &mut warnings),
            last_fm: bind_section(tree, "LastFm", &mut warnings),
            radio_sources: bind_section(tree, "RadioSources", &mut warnings),
            notifications: bind_section(tree, "Notifications", &mut warnings),
            metadata: bind_section(tree, "Metadata", &mut warnings),
            server: bind_section(tree, "Server", &mut warnings),
            listen_brainz: bind_section(tree, "ListenBrainz", &mut warnings),
            updates: bind_section(tree, "Updates", &mut warnings),
            admin: bind_section(tree, "Admin", &mut warnings),
            imports: bind_section(tree, "Imports", &mut warnings),
            family: bind_section(tree, "Family", &mut warnings),
        };
        // Navidrome's address is read without the spaces and trailing slashes it may have been
        // typed with, so every upstream URL Octo builds from it has one slash (a trailing slash
        // would make the stream relay ask for `//rest/stream`).
        if let Some(url) = settings.subsonic.url.as_mut() {
            *url = normalize_server_url(url);
        }
        // The same for slskd's web address: read without the spaces it may have been typed with.
        settings.soulseek.web_url = settings.soulseek.web_url.trim().to_string();
        (settings, warnings)
    }
}

/// A server address as Octo keeps it: trimmed, with no trailing slash (`http://nd:4533/ ` is
/// `http://nd:4533`).
pub fn normalize_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// The config section `name` bound into `T`, collecting conversion warnings under the
/// section's path. The dashboard reads `Updates` this way on every request.
pub fn bind_section<T: DeserializeOwned + Default>(
    tree: &ConfigTree,
    name: &str,
    warnings: &mut Vec<BindWarning>,
) -> T {
    let (value, w) = bind::<T>(&tree.section(name));
    warnings.extend(w.into_iter().map(|w| BindWarning {
        path: if w.path.is_empty() {
            name.to_string()
        } else {
            format!("{name}:{}", w.path)
        },
        message: w.message,
    }));
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With YouTube switched off, outside songs play from Deezer when an account is set, whatever
    /// the stream source says; without an account there is nothing to play them from.
    #[test]
    fn with_you_tube_off_outside_songs_play_from_deezer_when_an_account_is_set() {
        let mut settings = AppSettings::default();
        assert_eq!(settings.subsonic.stream_source, StreamSource::YouTube);
        assert!(settings.deezer_stream_when(false).is_none());
        settings.deezer.arl = Some("arl".into());
        assert!(settings.deezer_stream().is_none());
        assert!(settings.deezer_stream_when(true).is_none());
        assert!(settings.deezer_stream_when(false).is_some());
        settings.subsonic.stream_source = StreamSource::Deezer;
        assert!(settings.deezer_stream_when(true).is_some());
    }

    #[test]
    fn you_tube_is_on_unless_switched_off() {
        let store = SettingsStore::for_tests(AppSettings::default());
        assert!(store.you_tube_enabled());
        for (value, on) in [
            ("false", false),
            (" FALSE ", false),
            ("0", false),
            ("true", true),
            ("", true),
            ("no idea", true),
        ] {
            store.set_raw("YouTube:Enabled", Some(value));
            assert_eq!(store.you_tube_enabled(), on, "{value:?}");
        }
    }

    #[test]
    fn navidromes_address_is_read_without_trailing_slashes_or_spaces() {
        let mut tree = ConfigTree::new();
        tree.set("Subsonic:Url", Some(" http://navidrome:4533// ".into()));
        let (settings, warnings) = AppSettings::bind(&tree);
        assert!(warnings.is_empty());
        assert_eq!(settings.subsonic.url.as_deref(), Some("http://navidrome:4533"));
        assert_eq!(
            normalize_server_url("http://nd:4533/base/"),
            "http://nd:4533/base"
        );
        assert_eq!(normalize_server_url(""), "");
    }

    /// slskd's web address is read without the spaces it was typed with.
    #[test]
    fn slskds_web_address_is_read_without_spaces() {
        let mut tree = ConfigTree::new();
        tree.set("Soulseek:WebUrl", Some(" http://slskd.lan:5030/ ".into()));
        let (settings, warnings) = AppSettings::bind(&tree);
        assert!(warnings.is_empty());
        assert_eq!(settings.soulseek.web_url, "http://slskd.lan:5030/");
    }
}
