//! Deezer as a radio source: Deezer's track mix for the seed (the list behind the radio button in
//! Deezer's own player), or Deezer's radio for the seed's artist when Deezer does not have the
//! song, and for an artist radio. Needs no Deezer account. Every song it suggests is one Deezer
//! has, so where outside songs play from Deezer its picks play. Ported from am-yy/octo's
//! `DeezerRadioSource`.

use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use octo_core::last_fm::SimilarTrack;
use octo_core::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use octo_core::radio::radio_provider::DEEZER;
use octo_core::radio::{RadioAnswer, RadioMatch, RadioSeed};
use octo_core::settings::SettingsStore;

use super::radio_source::IRadioSource;
use crate::services::metadata::DeezerMetadataService;
use crate::services::metadata::deezer_metadata_service::ARTIST_NAME_CANDIDATES;
use crate::services::metadata::deezer_metadata_service::radio::RadioTrack;
use crate::services::metadata::deezer_requests::DeezerUse;

pub struct DeezerRadioSource {
    deezer: Arc<DeezerMetadataService>,
    /// The radio source settings.
    settings: Arc<SettingsStore>,
}

impl DeezerRadioSource {
    pub fn new(deezer: Arc<DeezerMetadataService>, settings: Arc<SettingsStore>) -> Self {
        DeezerRadioSource { deezer, settings }
    }

    /// The catalog artist Deezer files under the seed's very name. A search's best guess for an
    /// uploader's channel name could be anyone, so nothing else counts.
    async fn artist_id(&self, name: &str) -> Option<String> {
        let wanted = seed_normalizer::artist(name);
        self.deezer
            .search_artists(name, ARTIST_NAME_CANDIDATES)
            .await
            .into_iter()
            .find(|hit| seed_normalizer::artist(&hit.name).eq_ignore_ascii_case(&wanted))
            .map(|hit| hit.deezer_id)
    }
}

#[async_trait]
impl IRadioSource for DeezerRadioSource {
    fn provider(&self) -> &'static str {
        DEEZER
    }

    fn available(&self) -> bool {
        self.settings.current().radio_sources.deezer
    }

    async fn songs_like(
        &self,
        seed: &RadioSeed,
        count: i32,
        _auth: &IndexMap<String, String>,
    ) -> anyhow::Result<RadioAnswer> {
        DeezerUse::Radio
            .scope(async {
                if !seed.is_artist()
                    && let Some(track_id) = self
                        .deezer
                        .find_track_id(&seed.artist, &seed.title, None, seed.duration)
                        .await
                    && let Some(mix) = self.deezer.track_mix(&track_id).await
                    && !mix.is_empty()
                {
                    return Ok(RadioAnswer::new(
                        DEEZER,
                        RadioMatch::Song,
                        tracks(&mix, count),
                        Vec::new(),
                    ));
                }
                let Some(artist) = self.artist_id(&seed.artist).await else {
                    return Ok(RadioAnswer::nothing(DEEZER));
                };
                let radio = self.deezer.artist_radio(&artist).await.unwrap_or_default();
                let r#match = if radio.is_empty() {
                    RadioMatch::None
                } else {
                    RadioMatch::ArtistsTrusted
                };
                Ok(RadioAnswer::new(
                    DEEZER,
                    r#match,
                    tracks(&radio, count),
                    Vec::new(),
                ))
            })
            .await
    }
}

/// The radio's songs as tracks this source suggested, in Deezer's order, each with its Deezer
/// id, so playing or hearting one needs no search.
fn tracks(radio: &[RadioTrack], count: i32) -> Vec<SimilarTrack> {
    radio
        .iter()
        .take(count.max(0) as usize)
        .map(|track| {
            SimilarTrack::new(track.artist.clone(), track.title.clone(), 1.0)
                .with_duration(track.seconds)
                .with_deezer_track_id(Some(track.track_id.clone()))
                .with_provider(Some(DEEZER.to_string()))
        })
        .collect()
}
