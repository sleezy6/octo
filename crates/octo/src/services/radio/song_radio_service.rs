//! Radio from one song, album or artist (getSimilarSongs): every source asked at once, their
//! answers blended, resolved local first, and each song marked with the source it came from.

use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;
use octo_core::common::skipped_takes;
use octo_core::last_fm::last_fm_radio_placement::{self, Spacing};
use octo_core::last_fm::last_fm_radio_recommendation_service::{
    IgnoreCaseKeys, Randomizer, default_randomizer,
};
use octo_core::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use octo_core::last_fm::last_fm_radio_spacing;
use octo_core::models::domain::Song;
use octo_core::radio::radio_blend::{self, RadioPick};
use octo_core::radio::radio_provider::{self, LIBRARY};
use octo_core::radio::{RadioAnswer, RadioMatch, RadioSeed};
use octo_core::settings::{ExplicitFilter, SettingsStore};
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tracing::{debug, info};

use super::radio_outcome_store::RadioOutcomeStore;
use super::radio_source_set::RadioSourceSet;
use crate::services::family::{FamilyStore, kid_filter};
use crate::services::last_fm::LastFmRadioTrackResolver;
use crate::services::sonic::SonicStore;

/// How many songs each source is asked for, whatever the radio's length: the draw is over the
/// whole of every list.
pub const SOURCE_DEPTH: i32 = 100;
/// How long a song a song radio gave a listener counts as served lately.
const SERVED_FOR: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);
/// The most songs remembered as served, per listener.
const SERVED_KEPT: usize = 3000;

/// Picks resolved beyond the count asked for, so the sound check has some to drop.
pub const PICK_HEADROOM: f64 = 1.5;

/// Song radio: the parts shared across requests, with each request's resolver handed to
/// [`SongRadioService::build`].
pub struct SongRadioService {
    sources: Arc<RadioSourceSet>,
    /// The Last.fm, radio source and Subsonic settings.
    settings: Arc<SettingsStore>,
    sonic: Option<Arc<SonicStore>>,
    outcomes: Option<Arc<RadioOutcomeStore>>,
    /// Source of the placement's draw. Tests replace it with a seeded one so a radio repeats
    /// exactly.
    randomizer: Mutex<Randomizer>,
    /// The family, whose kid profiles get clean radio.
    family: Option<Arc<FamilyStore>>,
    /// The songs each listener's song radios gave them lately (track keys, when), so the next
    /// radio leads with others. In memory: a restart starts the week over.
    served: Mutex<std::collections::HashMap<String, IndexMap<String, std::time::Instant>>>,
}

impl SongRadioService {
    pub fn new(
        sources: Arc<RadioSourceSet>,
        settings: Arc<SettingsStore>,
        sonic: Option<Arc<SonicStore>>,
        outcomes: Option<Arc<RadioOutcomeStore>>,
    ) -> Self {
        SongRadioService {
            sources,
            settings,
            sonic,
            outcomes,
            randomizer: Mutex::new(default_randomizer()),
            family: None,
            served: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The draw placement uses from now on, as the station service's `set_randomizer`.
    pub fn set_randomizer(&self, randomizer: Randomizer) {
        *self.randomizer.lock() = randomizer;
    }

    /// The service with the family, so a kid profile's radio is clean.
    pub fn with_family(mut self, family: Arc<FamilyStore>) -> Self {
        self.family = Some(family);
        self
    }

    pub fn sources(&self) -> &Arc<RadioSourceSet> {
        &self.sources
    }

    pub fn enabled(&self) -> bool {
        self.settings.current().last_fm.enable_radio && !self.sources.available().is_empty()
    }

    /// `listener`: who the radio is for, named the way scrobbles name them (RequestIdentity), so
    /// what they play teaches their own weights; None for nobody. `client_order`: the caller
    /// mixes the radio itself (Octo's apps send `octoMix=client`) and wants the order of fit, so
    /// outside songs are not placed. `bans` and `banned_artists`: the songs (track keys) and
    /// artists (artist keys) the listener banned from their radio. `heard`: the songs (track
    /// keys) the listener played in the last days, which wait until nothing else fits.
    ///
    /// Every source is asked for [`SOURCE_DEPTH`] songs. Unless the library leads, the radio is a
    /// draw over the whole pool ([`radio_blend::draw_order`]): the best match first, a song any
    /// song radio gave the listener lately weighing less, one artist holding a tenth of it at
    /// most ([`radio_blend::fit_count`]).
    #[allow(clippy::too_many_arguments)]
    pub async fn build(
        &self,
        seed: &RadioSeed,
        count: i32,
        listener: Option<&str>,
        auth: &IndexMap<String, String>,
        bans: &IgnoreCaseKeys,
        banned_artists: &IgnoreCaseKeys,
        heard: &IgnoreCaseKeys,
        resolver: &LastFmRadioTrackResolver,
        client_order: bool,
    ) -> Vec<Song> {
        // Strip collab/feature decoration so the catalogs find the canonical artist.
        let lookup = RadioSeed {
            artist: seed_normalizer::artist(&seed.artist),
            title: seed_normalizer::title(&seed.title),
            ..seed.clone()
        };
        let mut answers = self.sources.ask_all(&lookup, SOURCE_DEPTH, auth, None).await;

        // When no catalog has songs like this very song, the seed's own tags know its style
        // better than a guess from its artist's name, which for a YouTube upload is only the
        // channel's (#78).
        let library_led =
            seed.library_song.is_some() && !seed.is_artist() && radio_blend::library_led(&answers, count);
        if library_led && let Some(library_song) = &seed.library_song {
            answers.push(RadioAnswer::new(
                LIBRARY,
                RadioMatch::Song,
                Vec::new(),
                resolver.library_neighbours(library_song, count, auth).await,
            ));
            if let Some(genre) = radio_blend::primary_genre(library_song.genre.as_deref()) {
                answers.extend(self.sources.tag_all(&genre, count).await);
            }
        }

        let answers: Vec<RadioAnswer> = answers
            .into_iter()
            .map(|answer| radio_blend::without_filler_or_bans(answer, bans))
            .map(|answer| radio_blend::without_artists(answer, banned_artists))
            .collect();
        let weights = self.sources.weights(listener, library_led);
        let pool = radio_blend::blend(
            &answers,
            &weights,
            usize::MAX,
            &seed_normalizer::track_key(&seed.artist, &seed.title),
            seed.library_song.as_ref().map(|song| song.id.as_str()),
            count,
        );
        let randomizer = self.randomizer.lock().clone();
        let mut random = randomizer();
        let pool_size = pool.len();
        // A library-led radio keeps its order of fit; every other is a draw over the pool.
        let mut picks = if library_led {
            pool
        } else {
            radio_blend::draw_order(pool, &self.served_lately(listener), &mut *random)
        };
        picks.truncate((f64::from(count) * PICK_HEADROOM).ceil() as usize);
        info!(
            "Radio for '{} - {}': {}; weights {}; {} picked of {pool_size}{}",
            seed.artist,
            seed.title,
            answers
                .iter()
                .map(|answer| format!("{} {} {}", answer.provider, answer.r#match, answer.count()))
                .collect::<Vec<_>>()
                .join(", "),
            weights
                .iter()
                .map(|(provider, weight)| format!("{provider} {weight:.2}"))
                .collect::<Vec<_>>()
                .join(", "),
            picks.len(),
            if library_led { ", led by the library" } else { "" }
        );

        // For each pick, prefer the local copy if we own it. Tracks the user already has play at
        // full quality from Navidrome and skip the YouTube round trip. Lookups run in parallel.
        let gate = Semaphore::new(10);
        let resolved: Vec<Option<Song>> = futures::future::join_all(picks.iter().map(|pick| {
            let gate = &gate;
            async move {
                let _permit = gate.acquire().await.expect("the gate is never closed");
                resolve(pick, auth, resolver).await
            }
        }))
        .await;

        // One per song, and never the seed itself.
        let seed_id = seed.library_song.as_ref().map(|song| song.id.as_str());
        let mut seen = HashSet::new();
        let mut scored: Vec<(Song, f64)> = picks
            .iter()
            .zip(resolved)
            .filter_map(|(pick, song)| song.map(|song| (song, pick.score)))
            .filter(|(song, _)| Some(song.id.as_str()) != seed_id)
            .filter(|(song, _)| seen.insert(song.id.clone()))
            .collect();
        // Sound check: a library pick that sounds far from the seed slips down, one that sounds
        // close moves up. It catches a catalog's wrong-mood pick that the other sources missed.
        // Only songs Octo has analyzed have a sound, so outside songs keep their score. Not while
        // Sounds alike comes from Navidrome, so that test leaves octo-sonic out of radio.
        if let Some(sonic) = &self.sonic
            && self.settings.current().radio_sources.sounds_alike
            && !self
                .settings
                .current()
                .radio_sources
                .sounds_alike_from_navidrome()
            && let Some(seed_song) = seed.library_song.as_ref().filter(|song| !song.id.is_empty())
        {
            let fit = sonic.fit_factors(
                &seed_song.id,
                scored
                    .iter()
                    .filter(|(song, _)| song.is_local)
                    .map(|(song, _)| song.id.as_str()),
            );
            if !fit.is_empty() {
                for (song, score) in &mut scored {
                    *score *= fit.get(&song.id).copied().unwrap_or(1.0);
                }
                // A stable sort, highest score first. A drawn radio keeps its draw, and the
                // sound check's scores only order what Octo's apps get (client order).
                if library_led {
                    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                }
            }
        }
        // The admin's explicit filter, as stations apply it: with outside songs from every
        // source, radio is where most unknown lyrics now come from. A kid profile's is clean.
        // Skip covers and instrumentals leaves outside cover, karaoke and instrumental takes out the same way.
        let settings = self.settings.current();
        let filter = kid_filter::explicit_filter_for(&settings, self.family.as_deref(), listener);
        let skip_covers_and_instrumentals = settings.soulseek.skip_covers_and_instrumentals;
        let kept: Vec<(Song, f64)> = scored
            .into_iter()
            .filter(|(song, _)| match filter {
                ExplicitFilter::CleanOnly => song.explicit_content_lyrics != Some(1),
                ExplicitFilter::ExplicitOnly => song.explicit_content_lyrics != Some(3),
                ExplicitFilter::All => true,
            })
            .filter(|(song, _)| !skipped_takes::hides_song(song, skip_covers_and_instrumentals))
            .collect();
        let wanted = count.max(0) as usize;
        let mut kept: Vec<(Song, f64)> = if library_led {
            kept.into_iter().take(wanted).collect()
        } else {
            radio_blend::fit_count(
                kept,
                wanted,
                &seed.artist,
                heard,
                |(song, _)| song.artist.clone(),
                |(song, _)| seed_normalizer::track_key(&song.artist, &song.title),
            )
        };
        // Octo's apps mix the radio themselves from the order of fit.
        if client_order && !library_led {
            kept.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        }
        let songs: Vec<Song> = kept.into_iter().map(|(song, _)| song).collect();
        self.remember_served(listener, &songs);
        if let Some(outcomes) = &self.outcomes {
            outcomes.served(
                listener,
                songs.iter().map(|song| {
                    (
                        song.id.as_str(),
                        radio_provider::from_display_name(song.suggested_by.as_deref()),
                    )
                }),
                None,
                true,
            );
        }

        // A library-led radio keeps its order: its neighbors often share the seed's artist (one
        // uploader for a whole playlist), and spacing by artist would push every one of them
        // back.
        if library_led {
            info!("Radio for '{} - {}': led by the library", seed.artist, seed.title);
            songs
        } else if client_order {
            // Octo's own apps mix the radio themselves and ask for the order of fit
            // (octoMix=client).
            info!("Radio for '{} - {}': client order", seed.artist, seed.title);
            last_fm_radio_spacing::spread(
                &songs,
                |song: &Song| Some(song.artist.clone()),
                Some(&seed.artist),
            )
        } else {
            // For every other app this order is the radio, so outside songs are placed at random
            // at the radio's own mix: the share of its songs from outside the library, not the
            // Discovery balance, which is Your Radio's (last_fm_radio_placement). Artists are
            // spaced as spread spaces them, with no title gap.
            let outside = songs.iter().filter(|song| !song.is_local).count();
            let share = if songs.is_empty() {
                0.0
            } else {
                outside as f64 / songs.len() as f64
            };
            info!(
                "Radio for '{} - {}': placed at share {share:.2}",
                seed.artist, seed.title
            );
            last_fm_radio_placement::place(
                &songs,
                |song: &Song| !song.is_local,
                |song: &Song| Some(song.artist.clone()),
                Some(&seed.artist),
                Spacing::artists(last_fm_radio_spacing::GAP),
                share,
                &mut *random,
            )
        }
    }
}

impl SongRadioService {
    /// The songs (track keys) any song radio gave this listener in the last week.
    fn served_lately(&self, listener: Option<&str>) -> IgnoreCaseKeys {
        let Some(listener) = listener.filter(|l| !l.trim().is_empty()) else {
            return IgnoreCaseKeys::new();
        };
        let now = std::time::Instant::now();
        self.served
            .lock()
            .get(&listener.trim().to_lowercase())
            .map(|served| {
                served
                    .iter()
                    .filter(|(_, at)| now.duration_since(**at) < SERVED_FOR)
                    .map(|(key, _)| key.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remembers what a radio gave the listener, forgetting what is older than a week and the
    /// oldest past [`SERVED_KEPT`].
    fn remember_served(&self, listener: Option<&str>, songs: &[Song]) {
        let Some(listener) = listener.filter(|l| !l.trim().is_empty()) else {
            return;
        };
        let now = std::time::Instant::now();
        let mut all = self.served.lock();
        let served = all.entry(listener.trim().to_lowercase()).or_default();
        served.retain(|_, at| now.duration_since(*at) < SERVED_FOR);
        for song in songs {
            let key = seed_normalizer::track_key(&song.artist, &song.title);
            if !key.is_empty() {
                served.shift_remove(&key);
                served.insert(key, now);
            }
        }
        while served.len() > SERVED_KEPT {
            served.shift_remove_index(0);
        }
    }
}

/// One pick as a song the listener can play, marked with its source.
async fn resolve(
    pick: &RadioPick,
    auth: &IndexMap<String, String>,
    resolver: &LastFmRadioTrackResolver,
) -> Option<Song> {
    let song = match (&pick.library_song, &pick.track) {
        (Some(library), _) if pick.needs_reading => resolver.read_library_song(&library.id, auth).await,
        (Some(library), _) => Some(library.clone()),
        (None, Some(track)) => {
            resolver
                .resolve(
                    &track.artist,
                    &track.title,
                    track.duration,
                    auth,
                    track.you_tube_id.as_deref(),
                    track.deezer_track_id.as_deref(),
                )
                .await
        }
        (None, None) => {
            debug!("radio resolve failed for a {} pick", pick.provider);
            None
        }
    };
    song.map(|mut song| {
        song.suggested_by = radio_provider::display_name(Some(&pick.provider)).map(str::to_string);
        song
    })
}

#[cfg(test)]
#[path = "song_radio_service_tests.rs"]
mod tests;
