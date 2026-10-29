//! Reads Apple Music's public chart, New and room pages (`octo_core::charts::apple_pages` parses
//! them). Every request waits its turn at one gate, one a second, and a page Apple says is not
//! there is not asked for again for an hour, so however many listeners and apps ask, Apple sees
//! a few requests an hour per list.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::charts::apple_pages::{
    ApplePage, ApplePageError, PageLoops, RoomKind, SongRoom, is_country_code, parse_animated_art,
    parse_countries, parse_song_rooms, parse_songs, room_for,
};
use octo_core::charts::chart_settings::{APPLE_COUNTRIES, OVERALL};
use parking_lot::Mutex;
use tokio::time::Instant;

use crate::services::framework::http::client_builder;
use crate::services::framework::memory_cache::MemoryCache;

/// The most of a page that is read. A chart page is about 250 KB, the New page about 2.3 MB.
pub const MAX_BODY: usize = 8 * 1024 * 1024;
/// How far apart two requests to Apple are.
pub const INTERVAL: Duration = Duration::from_secs(1);
/// How long the country Apple sees the server in is kept.
const COUNTRY_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long the New page's rooms are kept: a new week's room is noticed within the hour.
const ROOMS_TTL: Duration = Duration::from_secs(60 * 60);
/// How long a page Apple said is not there is not asked for again.
const MISSING_TTL: Duration = Duration::from_secs(60 * 60);
/// How long a failed New page is not asked for again.
const FAILED_TTL: Duration = Duration::from_secs(10 * 60);
/// How long Apple is not asked again after it could not be reached at all (no connection, or no
/// answer in time): a server that cannot reach Apple must not wait out the timeout on every
/// request that asks for its country or a chart.
const UNREACHABLE_TTL: Duration = Duration::from_secs(5 * 60);
/// How long a fetched page is kept here: long enough that finding the country, which reads the
/// overall chart, does not read it a second time a moment later.
const PAGE_TTL: Duration = Duration::from_secs(10 * 60);
/// How much of an album page is read past the tall clip's key: the clip object is under 1 KB.
const TALL_LOOP_TAIL: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    /// Apple answered 404 or 500: the country has no such chart, or the room is gone.
    NoSuchPage,
    /// The New page names no room of that kind this week.
    NoRoom,
    /// The page passed [`MAX_BODY`].
    TooBig,
    /// The page no longer has the shape Octo reads.
    ChangedShape,
    /// Apple could not be reached or answered something else.
    Unavailable(String),
}

impl PageError {
    /// The error in a person's words, for the dashboard.
    pub fn describe(&self) -> String {
        match self {
            PageError::NoSuchPage => "Apple has no such list in this country".into(),
            PageError::NoRoom => "Apple's New page has no such list this week".into(),
            PageError::TooBig => "Apple's page was too big to read".into(),
            PageError::ChangedShape => "Apple's page changed shape".into(),
            PageError::Unavailable(why) => format!("Apple Music could not be reached ({why})"),
        }
    }
}

impl From<ApplePageError> for PageError {
    fn from(_: ApplePageError) -> Self {
        PageError::ChangedShape
    }
}

/// How many interactive reads in a row go before the oldest background read gets a turn.
const NOW_STREAK: usize = 4;

/// Whether a read is interactive (`now`) or background (`behind`). Clones share one flag, so a
/// read already waiting at the gate turns urgent when a request asks for the same answer.
#[derive(Debug, Clone)]
pub struct LaneCell(Arc<AtomicBool>);

impl LaneCell {
    pub fn now() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    pub fn behind() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn is_now(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Makes the read interactive. True when it was not already.
    pub fn make_now(&self) -> bool {
        !self.0.swap(true, Ordering::SeqCst)
    }
}

/// One request at a time, [`INTERVAL`] apart, for every page this reads. Interactive reads go
/// before background ones; after [`NOW_STREAK`] interactive grants in a row the oldest background
/// read goes, so a steady interactive load never starves the background.
pub struct Gate {
    interval: Duration,
    next: tokio::sync::Mutex<Option<Instant>>,
    /// Interactive reads waiting.
    waiting_now: AtomicUsize,
    /// Interactive grants since the last background one.
    now_streak: AtomicUsize,
    /// The next ticket for a background read.
    behind_order: AtomicU64,
    /// The tickets of the background reads waiting, the oldest first.
    serving_behind: Mutex<BTreeSet<u64>>,
    /// Signaled whenever a waiter leaves or a read turns urgent.
    freed: tokio::sync::Notify,
}

/// An interactive read waiting at the gate.
struct NowWait<'a>(&'a Gate);

impl Drop for NowWait<'_> {
    fn drop(&mut self) {
        self.0.waiting_now.fetch_sub(1, Ordering::SeqCst);
        self.0.freed.notify_waiters();
    }
}

/// A background read waiting at the gate, by its ticket.
struct BehindWait<'a> {
    gate: &'a Gate,
    ticket: u64,
}

impl Drop for BehindWait<'_> {
    fn drop(&mut self) {
        self.gate.serving_behind.lock().remove(&self.ticket);
        self.gate.freed.notify_waiters();
    }
}

impl Gate {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: tokio::sync::Mutex::new(None),
            waiting_now: AtomicUsize::new(0),
            now_streak: AtomicUsize::new(0),
            behind_order: AtomicU64::new(0),
            serving_behind: Mutex::new(BTreeSet::new()),
            freed: tokio::sync::Notify::new(),
        }
    }

    /// Waits for a background turn. Waiters go one after another.
    pub async fn wait(&self) {
        self.wait_in(&LaneCell::behind(), || false).await;
    }

    /// Waits for a background turn unless, when its place comes up, `answered` says it needs
    /// none (a request before it already learned the answer). True when the request may go.
    pub async fn wait_unless(&self, answered: impl Fn() -> bool) -> bool {
        self.wait_in(&LaneCell::behind(), answered).await
    }

    /// Makes the waiting background reads look at their lane again, after one turned urgent.
    pub fn wake_behind(&self) {
        self.freed.notify_waiters();
    }

    fn join_now(&self) -> NowWait<'_> {
        self.waiting_now.fetch_add(1, Ordering::SeqCst);
        NowWait(self)
    }

    fn join_behind(&self) -> BehindWait<'_> {
        let ticket = self.behind_order.fetch_add(1, Ordering::SeqCst);
        self.serving_behind.lock().insert(ticket);
        BehindWait { gate: self, ticket }
    }

    /// Whether a background read may queue for the slot: it is the oldest background read, and
    /// either nothing interactive is waiting or interactive reads have had their run. Background
    /// reads so go one at a time in the order they came.
    fn behind_may_go(&self, ticket: u64) -> bool {
        self.serving_behind.lock().first() == Some(&ticket)
            && (self.waiting_now.load(Ordering::SeqCst) == 0
                || self.now_streak.load(Ordering::SeqCst) >= NOW_STREAK)
    }

    /// Whether an interactive read lets the background read that is due go first.
    fn now_should_yield(&self) -> bool {
        self.now_streak.load(Ordering::SeqCst) >= NOW_STREAK && !self.serving_behind.lock().is_empty()
    }

    /// Waits for this read's turn in its lane unless, when its place comes up, `answered` says
    /// it needs none. True when the read may go.
    pub async fn wait_in(&self, lane: &LaneCell, answered: impl Fn() -> bool) -> bool {
        let mut now_wait: Option<NowWait<'_>> = None;
        let mut behind_wait: Option<BehindWait<'_>> = None;
        if lane.is_now() {
            now_wait = Some(self.join_now());
        } else {
            behind_wait = Some(self.join_behind());
        }
        loop {
            let note = self.freed.notified();
            tokio::pin!(note);
            note.as_mut().enable();
            if behind_wait.is_some() && lane.is_now() {
                behind_wait = None;
                now_wait = Some(self.join_now());
            }
            if let Some(behind) = &behind_wait
                && !self.behind_may_go(behind.ticket)
            {
                note.await;
                continue;
            }
            let mut next = if behind_wait.is_some() {
                // A background read queued here looks again when a read turns urgent.
                tokio::select! {
                    guard = self.next.lock() => guard,
                    () = note.as_mut() => continue,
                }
            } else {
                self.next.lock().await
            };
            if let Some(behind) = &behind_wait {
                if lane.is_now() {
                    continue;
                }
                if !self.behind_may_go(behind.ticket) {
                    continue;
                }
            } else if self.now_should_yield() {
                drop(next);
                note.await;
                continue;
            }
            if answered() {
                return false;
            }
            if let Some(at) = *next
                && at > Instant::now()
            {
                tokio::time::sleep_until(at).await;
            }
            *next = Some(Instant::now() + self.interval);
            if now_wait.is_some() {
                self.now_streak.fetch_add(1, Ordering::SeqCst);
            } else {
                self.now_streak.store(0, Ordering::SeqCst);
            }
            return true;
        }
    }
}

pub struct AppleMusicPages {
    client: reqwest::Client,
    base: String,
    genres_base: String,
    gate: Gate,
    country: MemoryCache<String>,
    rooms: MemoryCache<Arc<Vec<SongRoom>>>,
    pages: MemoryCache<Arc<ApplePage>>,
    missing: MemoryCache<PageError>,
    countries: Mutex<Vec<String>>,
}

impl AppleMusicPages {
    pub const BASE: &'static str = "https://music.apple.com";
    pub const GENRES_BASE: &'static str = "https://itunes.apple.com";

    pub fn new() -> Self {
        Self::with_base_url(Self::BASE, INTERVAL)
    }

    /// Pages from another host (a test's mock server), with another gap between requests.
    pub fn with_base_url(base: &str, interval: Duration) -> Self {
        let client = client_builder()
            .gzip(true)
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(octo_core::common::octo_user_agent::value())
            .build()
            .expect("the Apple Music client builds");
        Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            genres_base: Self::GENRES_BASE.to_string(),
            gate: Gate::new(interval),
            country: MemoryCache::new(16),
            rooms: MemoryCache::new(256),
            pages: MemoryCache::new(64),
            missing: MemoryCache::new(1024),
            countries: Mutex::new(Vec::new()),
        }
    }

    /// The genre list read from another host (a test's mock server).
    pub fn with_genres_base_url(mut self, base: &str) -> Self {
        self.genres_base = base.trim_end_matches('/').to_string();
        self
    }

    /// A genre's English name from Apple's US genre list, or None when it cannot be read or the
    /// name is not in Latin script. Takes its turn at the shared gate; a failure is not
    /// remembered, so the next ask tries again.
    pub async fn genre_english_name(&self, id: &str) -> Option<String> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let url = format!(
            "{}/WebObjects/MZStoreServices.woa/ws/genres?id={id}&cc=us",
            self.genres_base
        );
        self.gate.wait().await;
        let response = self.client.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.bytes().await.ok()?;
        if body.len() > MAX_BODY {
            return None;
        }
        let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
        let name = json.get(id)?.get("name")?.as_str()?.trim();
        (!name.is_empty() && octo_core::charts::chart_settings::is_latin_script(name))
            .then(|| name.to_string())
    }

    /// The gate every page waits at, for waking background reads that turned urgent.
    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    /// [`Self::chart_on`] in the background lane.
    pub async fn chart(&self, country: Option<&str>, genre: &str) -> Result<Arc<ApplePage>, PageError> {
        self.chart_on(country, genre, &LaneCell::behind()).await
    }

    /// A chart: the overall one (`34`) or a genre's, for a country, or for the server's own when
    /// `country` is None (Apple sends a link without one on to the country it sees the server in).
    pub async fn chart_on(
        &self,
        country: Option<&str>,
        genre: &str,
        lane: &LaneCell,
    ) -> Result<Arc<ApplePage>, PageError> {
        let country = match country {
            Some(code) if is_country_code(code) => Some(code),
            Some(_) => return Err(PageError::Unavailable("not a country".into())),
            None => None,
        };
        let key = country.map(|code| page_key(code, genre));
        if let Some(key) = &key
            && let Some(page) = self.pages.get(key)
        {
            return Ok(page);
        }
        let mut url = match country {
            Some(code) => format!("{}/{code}/new/top-charts/songs", self.base),
            None => format!("{}/new/top-charts/songs", self.base),
        };
        if genre != OVERALL {
            url.push_str("/?genreId=");
            url.push_str(genre);
        }
        // A page fetched while this read waited costs no slot.
        let held = || key.as_ref().is_some_and(|key| self.pages.get(key).is_some());
        let Some(html) = self.fetch(&url, lane, held).await? else {
            return key
                .as_ref()
                .and_then(|key| self.pages.get(key))
                .ok_or_else(|| PageError::Unavailable("the page was dropped".into()));
        };
        if genre == OVERALL {
            let countries = parse_countries(&html);
            if !countries.is_empty() {
                *self.countries.lock() = countries;
            }
        }
        let page = Arc::new(parse_songs(&html)?);
        if is_country_code(&page.country) {
            if country.is_none() {
                self.country.set("country", page.country.clone(), 1, COUNTRY_TTL);
            }
            self.pages
                .set(page_key(&page.country, genre), page.clone(), 1, PAGE_TTL);
        }
        Ok(page)
    }

    /// [`Self::detected_country_on`] in the background lane.
    pub async fn detected_country(&self) -> Option<String> {
        self.detected_country_on(&LaneCell::behind()).await
    }

    /// The country Apple sees the server in, or None while that cannot be learned.
    pub async fn detected_country_on(&self, lane: &LaneCell) -> Option<String> {
        if let Some(code) = self.known_country() {
            return Some(code);
        }
        let page = self.chart_on(None, OVERALL, lane).await.ok()?;
        is_country_code(&page.country).then(|| page.country.clone())
    }

    /// The country Apple was last seen to put the server in, without asking: None until it was
    /// learned.
    pub fn known_country(&self) -> Option<String> {
        self.country.get("country")
    }

    /// [`Self::rooms_on`] in the background lane.
    pub async fn rooms(&self, country: &str) -> Result<Arc<Vec<SongRoom>>, PageError> {
        self.rooms_on(country, &LaneCell::behind()).await
    }

    /// The New page's song rooms this week, asked for in English (so their titles can be
    /// matched), and again without when English is refused.
    pub async fn rooms_on(&self, country: &str, lane: &LaneCell) -> Result<Arc<Vec<SongRoom>>, PageError> {
        if !is_country_code(country) {
            return Err(PageError::Unavailable("not a country".into()));
        }
        if let Some(rooms) = self.rooms.get(country) {
            return Ok(rooms);
        }
        let failed_key = format!("rooms|{country}");
        if let Some(error) = self.missing.get(&failed_key) {
            return Err(error);
        }
        let english = format!("{}/{country}/new?l=en-US", self.base);
        let html = match self.fetch_html(&english, lane).await {
            Ok(html) => Ok(html),
            Err(PageError::NoSuchPage | PageError::Unavailable(_)) => {
                self.fetch_html(&format!("{}/{country}/new", self.base), lane)
                    .await
            }
            Err(other) => Err(other),
        };
        let rooms = html.and_then(|html| parse_song_rooms(&html).map_err(PageError::from));
        match rooms {
            Ok(rooms) => {
                let rooms = Arc::new(rooms);
                self.rooms.set(country, rooms.clone(), 1, ROOMS_TTL);
                Ok(rooms)
            }
            Err(error) => {
                self.missing.set(failed_key, error.clone(), 1, FAILED_TTL);
                Err(error)
            }
        }
    }

    /// [`Self::room_on`] in the background lane.
    pub async fn room(&self, country: &str, kind: RoomKind) -> Result<(String, Arc<ApplePage>), PageError> {
        self.room_on(country, kind, &LaneCell::behind()).await
    }

    /// This week's Best New Songs or Trending Songs: the room's id and its songs.
    pub async fn room_on(
        &self,
        country: &str,
        kind: RoomKind,
        lane: &LaneCell,
    ) -> Result<(String, Arc<ApplePage>), PageError> {
        let rooms = self.rooms_on(country, lane).await?;
        let room = room_for(&rooms, kind).ok_or(PageError::NoRoom)?;
        let key = page_key(country, &format!("room:{}", room.room_id));
        if let Some(page) = self.pages.get(&key) {
            return Ok((room.room_id.clone(), page));
        }
        let url = format!("{}/{country}/room/{}", self.base, room.room_id);
        let Some(html) = self.fetch(&url, lane, || self.pages.get(&key).is_some()).await? else {
            return self
                .pages
                .get(&key)
                .map(|page| (room.room_id.clone(), page))
                .ok_or_else(|| PageError::Unavailable("the page was dropped".into()));
        };
        let page = Arc::new(parse_songs(&html)?);
        self.pages.set(key, page.clone(), 1, PAGE_TTL);
        Ok((room.room_id.clone(), page))
    }

    /// [`Self::album_animated_art_on`] in the background lane.
    pub async fn album_animated_art(&self, country: &str, album_id: u64) -> Result<PageLoops, PageError> {
        self.album_animated_art_on(country, album_id, &LaneCell::behind())
            .await
    }

    /// An album's animated art from its page in `country`'s store, then the US store when that
    /// store has no such page. The page is read only up to the end of the tall clip, which
    /// sits about two thirds of the way in.
    pub async fn album_animated_art_on(
        &self,
        country: &str,
        album_id: u64,
        lane: &LaneCell,
    ) -> Result<PageLoops, PageError> {
        let mut stores = vec![country];
        if country != "us" {
            stores.push("us");
        }
        let mut last = PageError::NoSuchPage;
        for store in stores.into_iter().filter(|c| is_country_code(c)) {
            let url = format!("{}/{store}/album/{album_id}", self.base);
            match self.fetch_through_tall_loop(&url, lane).await {
                Ok(html) => return Ok(parse_animated_art(&html)),
                Err(PageError::NoSuchPage) => {}
                Err(other) => last = other,
            }
        }
        Err(last)
    }

    /// The page at `url` up to [`TALL_LOOP_TAIL`] past the tall clip's key, or whole when it has
    /// none. Takes its turn at the gate like every other page.
    async fn fetch_through_tall_loop(&self, url: &str, lane: &LaneCell) -> Result<String, PageError> {
        const TALL: &[u8] = b"\"motionDetailTall\":";
        if let Some(error) = self.missing.get(url) {
            return Err(error);
        }
        self.gate.wait_in(lane, || false).await;
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| PageError::Unavailable(short(&e)))?;
        let status = response.status();
        if status.as_u16() == 404 {
            self.missing.set(url, PageError::NoSuchPage, 1, MISSING_TTL);
            return Err(PageError::NoSuchPage);
        }
        if !status.is_success() {
            return Err(PageError::Unavailable(format!("HTTP {}", status.as_u16())));
        }
        let mut body: Vec<u8> = Vec::with_capacity(512 * 1024);
        let mut stop_at: Option<usize> = None;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| PageError::Unavailable(short(&e)))?
        {
            // Only the new bytes, and the key's length before them, are searched.
            let from = body.len().saturating_sub(TALL.len());
            body.extend_from_slice(&chunk);
            if stop_at.is_none()
                && let Some(found) = body[from..].windows(TALL.len()).position(|w| w == TALL)
            {
                stop_at = Some(from + found + TALL_LOOP_TAIL);
            }
            if stop_at.is_some_and(|at| body.len() >= at) {
                break;
            }
            if body.len() > MAX_BODY {
                return Err(PageError::TooBig);
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Whether [`Self::countries`] holds this country, in any case, without copying the list.
    pub fn has_country(&self, code: &str) -> bool {
        let code = code.trim();
        let read = self.countries.lock();
        if read.is_empty() {
            APPLE_COUNTRIES.iter().any(|c| c.eq_ignore_ascii_case(code))
        } else {
            read.iter().any(|c| c.eq_ignore_ascii_case(code))
        }
    }

    /// Apple Music's countries, from the last overall chart's country switcher, else the list
    /// as it was on 2026-10-07.
    pub fn countries(&self) -> Vec<String> {
        let read = self.countries.lock().clone();
        if read.is_empty() {
            APPLE_COUNTRIES.iter().map(|c| (*c).to_string()).collect()
        } else {
            read
        }
    }

    /// The page at `url` whole, read in `lane`, for a page nothing else could have fetched
    /// meanwhile.
    async fn fetch_html(&self, url: &str, lane: &LaneCell) -> Result<String, PageError> {
        match self.fetch(url, lane, || false).await? {
            Some(html) => Ok(html),
            None => Err(PageError::Unavailable("the page was held".into())),
        }
    }

    /// The page at `url` whole, read in `lane`; Ok(None) when `held` says the answer arrived
    /// while this read waited, so it takes no slot.
    async fn fetch(
        &self,
        url: &str,
        lane: &LaneCell,
        held: impl Fn() -> bool,
    ) -> Result<Option<String>, PageError> {
        if let Some(error) = self.missing.get(url) {
            return Err(error);
        }
        // A request that waited in the queue while the one before it failed takes that failure;
        // one whose answer arrived meanwhile takes that.
        if !self
            .gate
            .wait_in(lane, || held() || self.missing.get(url).is_some())
            .await
        {
            if let Some(error) = self.missing.get(url) {
                return Err(error);
            }
            if held() {
                return Ok(None);
            }
        }
        let mut response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(e) => {
                let error = PageError::Unavailable(short(&e));
                self.missing.set(url, error.clone(), 1, UNREACHABLE_TTL);
                return Err(error);
            }
        };
        let status = response.status();
        if status.as_u16() == 404 || status.as_u16() == 500 {
            self.missing.set(url, PageError::NoSuchPage, 1, MISSING_TTL);
            return Err(PageError::NoSuchPage);
        }
        if !status.is_success() {
            return Err(PageError::Unavailable(format!("HTTP {}", status.as_u16())));
        }
        if response.content_length().is_some_and(|n| n > MAX_BODY as u64) {
            return Err(PageError::TooBig);
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| PageError::Unavailable(short(&e)))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(PageError::TooBig);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(String::from_utf8_lossy(&body).into_owned()))
    }
}

impl Default for AppleMusicPages {
    fn default() -> Self {
        Self::new()
    }
}

fn page_key(country: &str, page: &str) -> String {
    format!("{country}|{page}")
}

/// A transport error in a few words: the kind of failure, never the URL.
fn short(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "no connection".into()
    } else {
        "the answer broke off".into()
    }
}

#[cfg(test)]
#[path = "apple_music_pages_tests.rs"]
mod tests;
