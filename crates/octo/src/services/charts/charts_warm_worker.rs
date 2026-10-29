//! Fetches every chart list that is on, for the server's country and every country a listener
//! chose as their own or pinned, so a listener's app finds them on its first look after a start
//! and at each new period: Symfonium and the like list playlists rarely, and a chart that takes a
//! few seconds to arrive would otherwise be missing until the next time they look. Only the
//! shared charts are fetched here; each listener's own library match is still made in their own
//! request, with their own sign-in.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::Clock;
use octo_core::library::generated_playlist_service::period_index;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::services::common::TopSongsService;
use crate::services::family::NavidromeAdmin;
use crate::services::library::ChartPlaylistService;
use crate::services::library::chart_playlist_service::REFRESH_HOURS;

/// How long after the start the first fetch waits, so the server finishes starting first.
const START_DELAY: Duration = Duration::from_secs(10);
/// A little after each period starts, so a clock a moment behind does not fetch the old period.
const AFTER_PERIOD: Duration = Duration::from_secs(30);

pub struct ChartsWarmWorker {
    top_songs: TopSongsService,
    playlists: ChartPlaylistService,
    clock: Clock,
    /// Navidrome's people, to drop the chart choices of those it no longer has.
    people: Option<NavidromeAdmin>,
}

impl ChartsWarmWorker {
    pub fn new(top_songs: TopSongsService, playlists: ChartPlaylistService, clock: Clock) -> Self {
        Self {
            top_songs,
            playlists,
            clock,
            people: None,
        }
    }

    /// The worker checking each period that the people it has choices for are still Navidrome's.
    pub fn with_people(mut self, admin: NavidromeAdmin) -> Self {
        self.people = Some(admin);
        self
    }

    /// Drops the chart choices and kept lists of people Navidrome no longer has, and moves a
    /// renamed person's row to their new name. Only a complete, trusted list of Navidrome's
    /// people changes anything. A row saved while the list was being read is left alone.
    async fn prune_people(&self) {
        let Some(admin) = &self.people else {
            return;
        };
        let store = self.top_songs.listeners();
        let seen = store.revisions();
        let Some(users) = admin.verified_users().await else {
            debug!("Chart countries: Navidrome's people could not be read, dropping no one");
            return;
        };
        let people: Vec<(String, String)> = users
            .iter()
            .map(|user| (user.id().to_string(), user.user_name().to_string()))
            .collect();
        let done = store.reconcile(&people, &seen);
        for (old, new) in &done.renamed {
            info!("Chart countries: {old} is now {new}");
            self.playlists.forget_person(old);
        }
        for gone in &done.dropped {
            self.playlists.forget_person(gone);
        }
        if !done.dropped.is_empty() {
            info!(
                "Chart countries: dropped {} people Navidrome no longer has",
                done.dropped.len()
            );
        }
    }

    /// Fetches every list that is on, Popular right now first, for the server's country and then
    /// each country listeners chose. A genre a country lacks answers at once without asking
    /// anyone (`TopSongsService::chart_in`).
    pub async fn warm_once(&self) -> usize {
        self.prune_people().await;
        let on = self.playlists.lists_on();
        if on.is_empty() {
            return 0;
        }
        // The country and its menu first, so the lists below are read for them.
        self.top_songs.learn().await;
        let mut fetched = 0;
        for chart in &on {
            if !self.top_songs.chart(chart).await.songs.is_empty() {
                fetched += 1;
            }
        }
        let server = self.top_songs.country().await;
        let mut asked = on.len();
        for country in self.top_songs.listeners().countries() {
            if Some(&country) == server.as_ref() || !self.top_songs.has_country(&country) {
                continue;
            }
            self.top_songs.learn_in(Some(country.clone())).await;
            for chart in &on {
                asked += 1;
                if !self
                    .top_songs
                    .chart_in(Some(country.clone()), chart)
                    .await
                    .songs
                    .is_empty()
                {
                    fetched += 1;
                }
            }
        }
        debug!("chart lists warmed: {fetched} of {asked}");
        fetched
    }

    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        tokio::select! {
            _ = tokio::time::sleep(START_DELAY) => {}
            _ = stopping.cancelled() => return Ok(()),
        }
        while !stopping.is_cancelled() {
            tokio::select! {
                _ = self.warm_once() => {}
                _ = stopping.cancelled() => return Ok(()),
            }
            tokio::select! {
                _ = tokio::time::sleep(until_next_period(self.clock.now())) => {}
                _ = stopping.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }
}

/// How long until the next period starts, plus [`AFTER_PERIOD`].
pub fn until_next_period(now: DateTime<Utc>) -> Duration {
    let next = period_index(now, REFRESH_HOURS) + 1;
    let start = DateTime::UNIX_EPOCH + TimeDelta::hours(next * i64::from(REFRESH_HOURS));
    (start - now).to_std().unwrap_or_default() + AFTER_PERIOD
}

#[cfg(test)]
#[path = "charts_warm_worker_tests.rs"]
mod tests;
