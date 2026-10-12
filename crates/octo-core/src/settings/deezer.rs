//! `DeezerSettings` (the `Deezer` section): a Deezer account as a source of audio.
//!
//! Deezer is already the catalog Octo reads album and track facts from, with no account. These
//! settings are the other half: an account's ARL cookie, which lets Octo play and keep the songs
//! themselves from Deezer. Adapted from am-yy/octo's `Octo.Models.Settings.DeezerSettings`.
//! Every value is read at use, so a change needs no restart.

use serde::{Deserialize, Serialize};

use crate::soulseek::stream_facts::DeclaredStream;

/// The Deezer formats Octo asks for, best first.
pub const DEEZER_FORMAT_FLAC: &str = "FLAC";
pub const DEEZER_FORMAT_MP3_320: &str = "MP3_320";
pub const DEEZER_FORMAT_MP3_128: &str = "MP3_128";
/// FLAC and nothing lesser: what Better quality asks for, since only a lossless copy replaces
/// a song.
pub const DEEZER_QUALITY_LOSSLESS_ONLY: &str = "FLAC_ONLY";

/// Where the playback cache lives when the setting does not say: inside the container, where
/// the compose file mounts a host folder.
pub const DEFAULT_CACHE_PATH: &str = "/app/cache/deezer";

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct DeezerSettings {
    /// Account ARL cookie from deezer.com. Required for full-track playback.
    /// Environment variable: DEEZER__ARL
    pub arl: Option<String>,

    /// Optional account to try when the primary account cannot serve a track.
    /// Environment variable: DEEZER__ARLFALLBACK
    pub arl_fallback: Option<String>,

    /// Permanent-copy quality, with lower-quality fallback (default: FLAC).
    /// Values: "FLAC", "MP3_320", "MP3_128".
    /// Environment variable: DEEZER__QUALITY
    pub quality: String,

    /// Quality of an outside song played straight from Deezer, with lower-quality fallback
    /// (default: MP3_320). An MP3 is what every Subsonic app plays and seeks in; FLAC is there
    /// for a listener who wants it and whose apps take it.
    /// Values: "FLAC", "MP3_320", "MP3_128".
    /// Environment variable: DEEZER__STREAMQUALITY
    pub stream_quality: String,

    /// Keep a copy of each song played from Deezer, outside the music library, so a replay
    /// or a seek reads the copy and does not ask Deezer again (default: false).
    /// Environment variable: DEEZER__CACHEENABLED
    pub cache_enabled: bool,

    /// The folder the copies are kept in (default: /app/cache/deezer). Keep it outside the
    /// music library, or Navidrome lists every played song. Restart required.
    /// Environment variable: DEEZER__CACHEPATH
    pub cache_path: String,

    /// The most the copies may take, in GiB (default: 20). Past it, the copies played longest
    /// ago go first. 0 or less keeps no copy once its play is over.
    /// Environment variable: DEEZER__CACHEMAXGIB
    pub cache_max_gi_b: f64,

    /// How many days a copy is kept after it was last played (default: 7). 0 or less keeps
    /// them until the size limit removes them.
    /// Environment variable: DEEZER__CACHERETENTIONDAYS
    pub cache_retention_days: i32,
}

impl Default for DeezerSettings {
    fn default() -> Self {
        Self {
            arl: None,
            arl_fallback: None,
            quality: DEEZER_FORMAT_FLAC.to_string(),
            stream_quality: DEEZER_FORMAT_MP3_320.to_string(),
            cache_enabled: false,
            cache_path: DEFAULT_CACHE_PATH.to_string(),
            cache_max_gi_b: 20.0,
            cache_retention_days: 7,
        }
    }
}

impl DeezerSettings {
    /// The accounts to try, in order: the ARL, then the fallback, each trimmed, blanks and a
    /// repeat left out.
    pub fn accounts(&self) -> Vec<String> {
        let mut accounts: Vec<String> = Vec::new();
        for arl in [&self.arl, &self.arl_fallback].into_iter().flatten() {
            let arl = arl.trim();
            if !arl.is_empty() && accounts.iter().all(|known| known != arl) {
                accounts.push(arl.to_string());
            }
        }
        accounts
    }

    /// An account is set, so Deezer can be asked for audio.
    pub fn is_configured(&self) -> bool {
        !self.accounts().is_empty()
    }

    /// The cache folder: `CachePath`, or the default when it is blank.
    pub fn effective_cache_path(&self) -> String {
        match self.cache_path.trim() {
            "" => DEFAULT_CACHE_PATH.to_string(),
            path => path.to_string(),
        }
    }

    /// The most the cache may hold, in bytes: `CacheMaxGiB`, never below 0 and never more
    /// than a u64 holds.
    pub fn effective_cache_max_bytes(&self) -> u64 {
        const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
        if !self.cache_max_gi_b.is_finite() || self.cache_max_gi_b <= 0.0 {
            return 0;
        }
        (self.cache_max_gi_b * GIB).min(u64::MAX as f64) as u64
    }

    /// How long a copy is kept after its last play, or `None` for as long as there is room.
    pub fn effective_cache_retention(&self) -> Option<std::time::Duration> {
        (self.cache_retention_days > 0)
            .then(|| std::time::Duration::from_secs(self.cache_retention_days as u64 * 24 * 60 * 60))
    }

    /// What a client is told an outside song played from Deezer is: the suffix and content type
    /// of `StreamQuality`, which a player picks its decoder from, and the bit rate of an MP3.
    /// A FLAC's bit rate is not known before it is read, so none is given.
    pub fn declared_stream(&self) -> DeclaredStream {
        match Self::formats(&self.stream_quality)[0] {
            DEEZER_FORMAT_FLAC => DeclaredStream {
                suffix: "flac".into(),
                content_type: "audio/flac".into(),
                bit_rate: None,
            },
            DEEZER_FORMAT_MP3_128 => DeclaredStream {
                suffix: "mp3".into(),
                content_type: "audio/mpeg".into(),
                bit_rate: Some(128),
            },
            _ => DeclaredStream {
                suffix: "mp3".into(),
                content_type: "audio/mpeg".into(),
                bit_rate: Some(320),
            },
        }
    }

    /// The formats to ask for at `quality`, best first: the named one and every lesser one.
    /// Anything that is not a lossy name asks for FLAC first.
    pub fn formats(quality: &str) -> &'static [&'static str] {
        match quality.trim().to_ascii_uppercase().as_str() {
            DEEZER_FORMAT_MP3_128 => &[DEEZER_FORMAT_MP3_128],
            DEEZER_QUALITY_LOSSLESS_ONLY => &[DEEZER_FORMAT_FLAC],
            DEEZER_FORMAT_MP3_320 => &[DEEZER_FORMAT_MP3_320, DEEZER_FORMAT_MP3_128],
            _ => &[DEEZER_FORMAT_FLAC, DEEZER_FORMAT_MP3_320, DEEZER_FORMAT_MP3_128],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_trimmed_ordered_and_listed_once() {
        let settings = DeezerSettings {
            arl: Some("  abc  ".into()),
            arl_fallback: Some("abc".into()),
            ..Default::default()
        };
        assert_eq!(settings.accounts(), ["abc"]);

        let settings = DeezerSettings {
            arl: Some(" ".into()),
            arl_fallback: Some("second".into()),
            ..Default::default()
        };
        assert_eq!(settings.accounts(), ["second"]);
        assert!(settings.is_configured());
        assert!(!DeezerSettings::default().is_configured());
    }

    #[test]
    fn a_song_played_from_deezer_is_declared_in_the_stream_quality() {
        let declared = DeezerSettings::default().declared_stream();
        assert_eq!(
            (
                declared.suffix.as_str(),
                declared.content_type.as_str(),
                declared.bit_rate
            ),
            ("mp3", "audio/mpeg", Some(320))
        );
        for (quality, suffix, content_type, bit_rate) in [
            ("FLAC", "flac", "audio/flac", None),
            ("mp3_128", "mp3", "audio/mpeg", Some(128)),
            ("", "flac", "audio/flac", None),
        ] {
            let declared = DeezerSettings {
                stream_quality: quality.into(),
                ..Default::default()
            }
            .declared_stream();
            assert_eq!(
                (
                    declared.suffix.as_str(),
                    declared.content_type.as_str(),
                    declared.bit_rate
                ),
                (suffix, content_type, bit_rate),
                "{quality}"
            );
        }
    }

    #[test]
    fn the_cache_is_off_by_default_with_twenty_gib_for_a_week() {
        let settings = DeezerSettings::default();
        assert!(!settings.cache_enabled);
        assert_eq!(settings.effective_cache_path(), "/app/cache/deezer");
        assert_eq!(settings.effective_cache_max_bytes(), 20 * 1024 * 1024 * 1024);
        assert_eq!(
            settings.effective_cache_retention(),
            Some(std::time::Duration::from_secs(7 * 24 * 60 * 60))
        );
    }

    #[test]
    fn cache_limits_that_make_no_sense_mean_none() {
        for (gib, expected) in [(0.0, 0), (-3.0, 0), (f64::NAN, 0), (0.5, 512 * 1024 * 1024)] {
            let settings = DeezerSettings {
                cache_max_gi_b: gib,
                ..Default::default()
            };
            assert_eq!(settings.effective_cache_max_bytes(), expected, "{gib}");
        }
        let settings = DeezerSettings {
            cache_retention_days: 0,
            cache_path: "  ".into(),
            ..Default::default()
        };
        assert_eq!(settings.effective_cache_retention(), None);
        assert_eq!(settings.effective_cache_path(), "/app/cache/deezer");
    }

    #[test]
    fn the_cache_size_binds_from_its_c_sharp_name() {
        use crate::config::ConfigTree;
        let mut tree = ConfigTree::new();
        tree.set("Deezer:CacheMaxGiB", Some("2.5".into()));
        tree.set("Deezer:CacheEnabled", Some("true".into()));
        let (settings, warnings) = crate::settings::AppSettings::bind(&tree);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(settings.deezer.cache_enabled);
        assert_eq!(settings.deezer.cache_max_gi_b, 2.5);
    }

    #[test]
    fn a_quality_asks_for_itself_and_every_lesser_format() {
        assert_eq!(DeezerSettings::formats("FLAC"), ["FLAC", "MP3_320", "MP3_128"]);
        assert_eq!(DeezerSettings::formats(" mp3_320 "), ["MP3_320", "MP3_128"]);
        assert_eq!(DeezerSettings::formats("MP3_128"), ["MP3_128"]);
        assert_eq!(DeezerSettings::formats("flac_only"), ["FLAC"]);
        assert_eq!(
            DeezerSettings::formats("anything"),
            ["FLAC", "MP3_320", "MP3_128"]
        );
        assert_eq!(DeezerSettings::formats(""), ["FLAC", "MP3_320", "MP3_128"]);
    }
}
