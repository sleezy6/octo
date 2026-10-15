//! `Octo.Services.Deezer.DeezerResolver`: a Deezer track id turned into its audio.
//!
//! Native account-backed media resolution adapted from V1ck3s/octo-fiesta (GPL-3.0), by way of
//! am-yy/octo. Session tokens stay local to one account attempt; fallback accounts never share
//! cookies.
//!
//! The steps, per account: `deezer.getUserData` with the ARL cookie gives the session's API
//! token and the account's license token; `deezer.pageTrack` gives the track's own token;
//! `media/get_url` trades the two tokens for the address of the stored bytes, which
//! [`super::deezer_decrypted_stream`] turns into audio. A track the account cannot be given is
//! followed to the copy Deezer names for it (`FALLBACK`), then to the track sharing its ISRC,
//! then to another catalog copy, and each of those must be the same recording by artist and
//! title before a byte of it is played.
//!
//! Octo asks Deezer only when it has a reason to, as deemix does, since an ARL that makes
//! unusual traffic risks being flagged: a sign-in is kept and redone only when Deezer says its
//! token is no longer good; an ARL Deezer refused is not tried again until it changes (or
//! someone presses Check now), and a sign-in that failed for want of an answer waits longer
//! each time; a track no account could be given is not asked about again for six hours; and a
//! format the plan or the track does not have is never asked for.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt;
use octo_core::common::dotnet::escape_data_string;
use octo_core::settings::deezer::{DEEZER_FORMAT_FLAC, DEEZER_FORMAT_MP3_320};
use octo_core::settings::{DeezerSettings, SettingsStore};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use super::deezer_decrypted_stream::{STRIPE, StripeDecryptor, decrypted};
use crate::services::framework::MemoryCache;
use crate::services::i_download_service::AudioStream;
use crate::services::metadata::deezer_metadata_service::{DeezerMetadataService, recording_matches};

/// An open Deezer track: decrypted audio, and what to tell the client about it.
pub struct DeezerOpenStream {
    pub audio_stream: AudioStream,
    pub content_type: String,
    pub content_length: Option<u64>,
    /// 200 for the whole song, 206 for a part, 416 for a range past its end.
    pub status_code: u16,
    pub content_range: Option<String>,
    /// The Deezer format the account was given: FLAC, MP3_320 or MP3_128.
    pub format: String,
}

impl DeezerOpenStream {
    /// The answer to a range that starts past the end of a song `total` bytes long.
    pub(super) fn unsatisfiable(total: u64, format: &str) -> Self {
        DeezerOpenStream {
            audio_stream: Box::pin(futures::stream::empty()),
            content_type: "audio/mpeg".to_string(),
            content_length: Some(0),
            status_code: 416,
            content_range: Some(format!("bytes */{total}")),
            format: format.to_string(),
        }
    }
}

/// No account could be given the track when last asked, within [`DeezerResolver::NO_MEDIA_TTL`].
#[derive(Debug, thiserror::Error)]
#[error("Deezer has no full-track media for this track (known from an earlier try).")]
struct NoMediaKnown;

/// What one account attempt signs its calls with.
#[derive(Clone)]
struct Session {
    cookie: String,
    api_token: String,
    license_token: String,
    /// What the sign-in said about the account, for the dashboard.
    facts: AccountFacts,
}

/// Deezer answered and would not sign the account in: the ARL is wrong, expired, or the
/// account has no licence to play whole songs. Any other failure is Deezer not answering.
#[derive(Debug, thiserror::Error)]
#[error("Deezer ARL expired or account has no full-track license.")]
struct AccountRefused;

/// Deezer said the sign-in's token is no longer good: the one thing that is worth a new
/// sign-in. A song Deezer cannot give, or Deezer not answering, is not.
#[derive(Debug, thiserror::Error)]
#[error("Deezer sign-in expired.")]
struct SignInExpired;

/// The account is not tried for now: its last sign-in failed for want of an answer, and the
/// wait after it has not passed.
#[derive(Debug, thiserror::Error)]
#[error("Deezer sign-in paused after a failed attempt.")]
struct SignInPaused;

/// What went wrong the last time an account signed in.
#[derive(Debug, Clone, Copy)]
enum SignInTrouble {
    /// Deezer refused the ARL: not tried again until it changes or someone checks it.
    Refused,
    /// Deezer did not answer: not tried again before `until`; the next wait is twice `wait`.
    Waiting {
        until: tokio::time::Instant,
        wait: Duration,
    },
}

/// What a sign-in says about the account. Each is `None` when the answer does not say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct AccountFacts {
    /// The plan's name as Deezer writes it ("Deezer Premium").
    offer: Option<String>,
    country: Option<String>,
    /// The plan gives FLAC.
    lossless: Option<bool>,
    /// The plan gives 320 kbps MP3.
    high_quality: Option<bool>,
}

/// One account as the dashboard shows it. Never the ARL.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeezerAccountHealth {
    pub signed_in: bool,
    /// Why not, when not: [`ACCOUNT_REFUSED`] or [`ACCOUNT_UNREACHABLE`].
    pub problem: Option<&'static str>,
    pub offer: Option<String>,
    pub country: Option<String>,
    pub lossless: Option<bool>,
    pub high_quality: Option<bool>,
}

/// Deezer answered and refused the ARL.
pub const ACCOUNT_REFUSED: &str = "refused";
/// Deezer did not answer, so nothing is known about the ARL.
pub const ACCOUNT_UNREACHABLE: &str = "unreachable";

/// The last account check: when it ran, for which accounts, and what it found.
struct AccountCheck {
    at: tokio::time::Instant,
    at_utc: chrono::DateTime<chrono::Utc>,
    accounts: Vec<String>,
    health: Vec<DeezerAccountHealth>,
}

/// The stored bytes of one track in one format, as one account was given them.
#[derive(Clone)]
struct Media {
    /// The track the bytes belong to, which is the track their key is made from. Not always
    /// the track asked for: see the module's note on fallbacks.
    track_id: String,
    format: String,
    url: String,
    account: String,
}

pub struct DeezerResolver {
    /// `deezer-media-api`: the gateway and the media endpoint. 10 s, no redirects followed.
    api_client: reqwest::Client,
    /// `deezer-media-stream`: the CDN. No timeout, because a body stays open for the whole song.
    stream_client: reqwest::Client,
    settings: Arc<SettingsStore>,
    catalog: Arc<DeezerMetadataService>,
    media_cache: MemoryCache<Media>,
    // ponytail: serial media resolution bounds gateway bursts; add per-account gates if contention grows.
    media_gate: tokio::sync::Mutex<()>,
    /// The sign-in each account was last given and when, so a play does not sign in again for
    /// [`Self::SESSION_TTL`]. Held while an account signs in, so plays at the same moment share
    /// one sign-in.
    sessions: tokio::sync::Mutex<std::collections::HashMap<String, (Session, tokio::time::Instant)>>,
    /// Each account whose last sign-in failed, and how: see [`SignInTrouble`].
    trouble: parking_lot::Mutex<HashMap<String, SignInTrouble>>,
    /// Tracks no account could be given, by accounts, track and quality, for [`Self::NO_MEDIA_TTL`].
    no_media: MemoryCache<()>,
    /// The last answer of [`Self::check_accounts`]: when, for which accounts, and what.
    account_check: tokio::sync::Mutex<Option<AccountCheck>>,
    gateway_url: String,
    media_url: String,
    /// Tests serve the CDN over plain HTTP; a real media address must be HTTPS.
    allow_plain_http: bool,
}

impl DeezerResolver {
    pub const API_CLIENT_NAME: &'static str = "deezer-media-api";
    pub const STREAM_CLIENT_NAME: &'static str = "deezer-media-stream";

    pub const GATEWAY_URL: &'static str = "https://www.deezer.com/ajax/gw-light.php";
    pub const MEDIA_URL: &'static str = "https://media.deezer.com/v1/get_url";

    pub const API_TIMEOUT: Duration = Duration::from_secs(10);
    /// How long an account's sign-in is reused. A play that Deezer refuses on a reused sign-in
    /// signs in again once before giving up.
    pub const SESSION_TTL: Duration = Duration::from_secs(10 * 60);
    /// How long a media address is reused.
    const MEDIA_TTL: Duration = Duration::from_secs(120);
    /// How long a track no account could be given is not asked about again.
    pub const NO_MEDIA_TTL: Duration = Duration::from_secs(6 * 60 * 60);
    const NO_MEDIA_CACHE_SIZE: u64 = 4096;
    /// The wait after the first sign-in Deezer did not answer, doubled after each one after it.
    pub const SIGN_IN_FIRST_WAIT: Duration = Duration::from_secs(60);
    /// The longest wait between sign-ins Deezer did not answer.
    pub const SIGN_IN_LONGEST_WAIT: Duration = Duration::from_secs(60 * 60);
    /// What Octo calls itself to Deezer: a desktop Chrome, as deemix does, rather than a bare
    /// "Mozilla/5.0" no browser sends. Worth moving to a newer Chrome now and then.
    pub const USER_AGENT: &'static str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
    const MEDIA_CACHE_SIZE: u64 = 128;
    /// How long the CDN may take to send its headers.
    const HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
    /// How long the CDN may go without sending the next piece of a song while it is being read.
    /// The stream client has no overall timeout (a body stays open for the whole song), so
    /// without this a connection that stalls halfway leaves a download, a cache fill or a play
    /// waiting forever.
    pub const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    /// How long one answer of the dashboard's account check is reused: the dashboard asks
    /// every few seconds, and each real check is a sign-in.
    pub const ACCOUNT_CHECK_TTL: Duration = Duration::from_secs(300);
    /// How often the accounts are checked with nobody looking.
    pub const ACCOUNT_WATCH_EVERY: Duration = Duration::from_secs(30 * 60);

    pub fn new(settings: Arc<SettingsStore>, catalog: Arc<DeezerMetadataService>) -> Self {
        Self::with_endpoints(settings, catalog, Self::GATEWAY_URL, Self::MEDIA_URL, false)
    }

    /// A resolver on the given gateway and media endpoint (tests point these at a local server).
    pub fn with_endpoints(
        settings: Arc<SettingsStore>,
        catalog: Arc<DeezerMetadataService>,
        gateway_url: &str,
        media_url: &str,
        allow_plain_http: bool,
    ) -> Self {
        // Neither client keeps cookies: each call carries exactly the cookie it is given.
        let api_client = reqwest::Client::builder()
            .timeout(Self::API_TIMEOUT)
            .user_agent(Self::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a plain HTTP client builds");
        let stream_client = reqwest::Client::builder()
            .user_agent(Self::USER_AGENT)
            .no_gzip()
            .no_deflate()
            .build()
            .expect("a plain HTTP client builds");
        Self {
            api_client,
            stream_client,
            settings,
            catalog,
            media_cache: MemoryCache::new(Self::MEDIA_CACHE_SIZE),
            media_gate: tokio::sync::Mutex::new(()),
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            trouble: parking_lot::Mutex::new(HashMap::new()),
            no_media: MemoryCache::new(Self::NO_MEDIA_CACHE_SIZE),
            account_check: tokio::sync::Mutex::new(None),
            gateway_url: gateway_url.to_string(),
            media_url: media_url.to_string(),
            allow_plain_http,
        }
    }

    // Read at use, so a new ARL or quality reaches the next play without a restart.
    fn deezer_settings(&self) -> DeezerSettings {
        self.settings.current().deezer.clone()
    }

    /// An account is set.
    pub fn is_configured(&self) -> bool {
        self.deezer_settings().is_configured()
    }

    /// The quality of a permanent copy.
    pub fn download_quality(&self) -> String {
        self.deezer_settings().quality
    }

    /// The quality of a song played straight from Deezer.
    pub fn stream_quality(&self) -> String {
        self.deezer_settings().stream_quality
    }

    /// One of the accounts signs in.
    pub async fn is_available(&self) -> bool {
        for arl in self.deezer_settings().accounts() {
            // Try the fallback account without logging credentials.
            if self.sign_in(&arl, true).await.is_ok() {
                return true;
            }
        }
        false
    }

    /// Which of the accounts sign in, in order (`true`: signed in), for the dashboard's test.
    /// An answer is kept for [`Self::ACCOUNT_CHECK_TTL`] while the accounts stay the same; held
    /// while a check runs, so callers at the same moment share one.
    pub async fn check_accounts(&self) -> Vec<bool> {
        let (health, _) = self.account_health(false).await;
        health.into_iter().map(|account| account.signed_in).collect()
    }

    /// Each account, in order, as a sign-in finds it now: whether it signs in, why not, and
    /// what plan it is on. With the time of the check. `fresh` (a person pressing Check now)
    /// signs in again whatever was kept, an ARL Deezer refused or one waiting after a failure
    /// included; otherwise an answer is reused as [`Self::check_accounts`] says, and an account
    /// in trouble is reported as it stands without asking Deezer.
    pub async fn account_health(
        &self,
        fresh: bool,
    ) -> (Vec<DeezerAccountHealth>, chrono::DateTime<chrono::Utc>) {
        let accounts = self.deezer_settings().accounts();
        let mut kept = self.account_check.lock().await;
        if !fresh
            && let Some(check) = kept.as_ref()
            && check.accounts == accounts
            && check.at.elapsed() < Self::ACCOUNT_CHECK_TTL
        {
            return (check.health.clone(), check.at_utc);
        }
        let mut health = Vec::new();
        for arl in &accounts {
            let signed_in = self.sign_in(arl, !fresh).await;
            match &signed_in {
                Ok(session) => self.remember_session(arl, session).await,
                Err(_) => self.forget_session(arl).await,
            }
            health.push(match signed_in {
                Ok(session) => DeezerAccountHealth {
                    signed_in: true,
                    problem: None,
                    offer: session.facts.offer,
                    country: session.facts.country,
                    lossless: session.facts.lossless,
                    high_quality: session.facts.high_quality,
                },
                Err(error) => DeezerAccountHealth {
                    problem: Some(if error.is::<AccountRefused>() {
                        ACCOUNT_REFUSED
                    } else {
                        ACCOUNT_UNREACHABLE
                    }),
                    ..Default::default()
                },
            });
        }
        let at_utc = chrono::Utc::now();
        *kept = Some(AccountCheck {
            at: tokio::time::Instant::now(),
            at_utc,
            accounts,
            health: health.clone(),
        });
        (health, at_utc)
    }

    /// Checks the accounts every [`Self::ACCOUNT_WATCH_EVERY`] and says so in the log when one
    /// stops signing in or signs in again: an ARL expires without a word, and until someone
    /// opens the dashboard the only sign is songs quietly coming from another source.
    pub async fn watch_accounts(
        self: Arc<Self>,
        stopping: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let mut last: Option<(Vec<String>, Vec<Option<&'static str>>)> = None;
        loop {
            let accounts = self.deezer_settings().accounts();
            if accounts.is_empty() {
                last = None;
            } else {
                // Not fresh: the answer kept is older than this wait anyway, and an ARL Deezer
                // refused is not asked about again on a timer.
                let (health, _) = self.account_health(false).await;
                let now: Vec<Option<&'static str>> = health.iter().map(|account| account.problem).collect();
                let before = last
                    .as_ref()
                    .filter(|(checked, _)| *checked == accounts)
                    .map(|(_, problems)| problems.clone());
                for (index, problem) in now.iter().enumerate() {
                    let was = before.as_ref().and_then(|problems| problems.get(index).copied());
                    match account_change(index, was, *problem) {
                        Some((true, message)) => warn!("{message}"),
                        Some((false, message)) => info!("{message}"),
                        None => {}
                    }
                }
                last = Some((accounts, now));
            }
            tokio::select! {
                biased;
                () = stopping.cancelled() => return Ok(()),
                () = tokio::time::sleep(Self::ACCOUNT_WATCH_EVERY) => {}
            }
        }
    }

    /// The catalog id of an outside song, which Octo knows by artist and title (and by ISRC
    /// when an album listing named it). `None` when the catalog has no such song.
    pub async fn track_id_for(&self, artist: &str, title: &str, isrc: Option<&str>) -> Option<String> {
        self.catalog.find_track_id(artist, title, isrc).await
    }

    /// The account's sign-in: the one kept while it is younger than [`Self::SESSION_TTL`], else a
    /// new one, which is kept. The flag is true for a kept one.
    async fn session_for(&self, arl: &str) -> anyhow::Result<(Session, bool)> {
        let mut sessions = self.sessions.lock().await;
        if let Some((session, at)) = sessions.get(arl)
            && at.elapsed() < Self::SESSION_TTL
        {
            return Ok((session.clone(), true));
        }
        sessions.remove(arl);
        let session = self.sign_in(arl, true).await?;
        let now = tokio::time::Instant::now();
        sessions.retain(|_, (_, at)| now.duration_since(*at) < Self::SESSION_TTL);
        sessions.insert(arl.to_string(), (session.clone(), now));
        Ok((session, false))
    }

    /// Makes every kept sign-in `by` older, for a test that waits out [`Self::SESSION_TTL`].
    #[cfg(test)]
    pub(crate) async fn age_sessions(&self, by: Duration) {
        for (_, at) in self.sessions.lock().await.values_mut() {
            *at = at
                .checked_sub(by)
                .expect("the clock is far enough from its start");
        }
    }

    /// Makes every wait after a failed sign-in `by` shorter, for a test that waits one out.
    #[cfg(test)]
    pub(crate) fn age_sign_in_waits(&self, by: Duration) {
        for trouble in self.trouble.lock().values_mut() {
            if let SignInTrouble::Waiting { until, .. } = trouble {
                *until = until
                    .checked_sub(by)
                    .expect("the clock is far enough from its start");
            }
        }
    }

    async fn forget_session(&self, arl: &str) {
        self.sessions.lock().await.remove(arl);
    }

    async fn remember_session(&self, arl: &str, session: &Session) {
        self.sessions
            .lock()
            .await
            .insert(arl.to_string(), (session.clone(), tokio::time::Instant::now()));
    }

    /// A new sign-in for the account, unless `honour_trouble` and its last one failed: an ARL
    /// Deezer refused is not tried again, and one Deezer did not answer waits its turn. What
    /// happens is remembered for the next.
    async fn sign_in(&self, arl: &str, honour_trouble: bool) -> anyhow::Result<Session> {
        if honour_trouble {
            match self.trouble.lock().get(arl).copied() {
                Some(SignInTrouble::Refused) => return Err(AccountRefused.into()),
                Some(SignInTrouble::Waiting { until, .. }) if tokio::time::Instant::now() < until => {
                    return Err(SignInPaused.into());
                }
                _ => {}
            }
        }
        let signed_in = self.authenticate(arl).await;
        self.note_sign_in(arl, &signed_in);
        signed_in
    }

    /// Remembers how a sign-in went: forgets past trouble on success, latches a refused ARL,
    /// and makes one Deezer did not answer wait, longer each time.
    fn note_sign_in(&self, arl: &str, signed_in: &anyhow::Result<Session>) {
        let accounts = self.deezer_settings().accounts();
        let mut trouble = self.trouble.lock();
        // An ARL no longer set is forgotten with whatever went wrong with it.
        trouble.retain(|known, _| accounts.iter().any(|account| account == known));
        match signed_in {
            Ok(_) => {
                if trouble.remove(arl).is_some() {
                    info!("Deezer signs the account in again");
                }
            }
            Err(error) if error.is::<AccountRefused>() => {
                if !matches!(trouble.get(arl), Some(SignInTrouble::Refused)) {
                    info!(
                        "Deezer refused the ARL; Octo stops asking Deezer with it until it is changed or checked again on the dashboard's Deezer page"
                    );
                }
                trouble.insert(arl.to_string(), SignInTrouble::Refused);
            }
            Err(error) => {
                let wait = match trouble.get(arl) {
                    Some(SignInTrouble::Waiting { wait, .. }) => (*wait * 2).min(Self::SIGN_IN_LONGEST_WAIT),
                    _ => Self::SIGN_IN_FIRST_WAIT,
                };
                warn!(
                    "Deezer did not answer a sign-in ({}); the next try waits {} min",
                    kind_of(error),
                    wait.as_secs().div_ceil(60)
                );
                trouble.insert(
                    arl.to_string(),
                    SignInTrouble::Waiting {
                        until: tokio::time::Instant::now() + wait,
                        wait,
                    },
                );
            }
        }
    }

    async fn authenticate(&self, arl: &str) -> anyhow::Result<Session> {
        if arl.len() > 1024 || !arl.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(AccountRefused.into());
        }
        let mut cookie = format!("arl={arl}");
        let response = self
            .api_client
            .post(self.gateway("deezer.getUserData", "null"))
            .header(reqwest::header::COOKIE, &cookie)
            .json(&json!({}))
            .send()
            .await?
            .error_for_status()?;
        let sid = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split(';').next())
            .find(|value| value.starts_with("sid="))
            .map(str::to_string);
        let root: Value = response.json().await?;

        let results = root.get("results");
        let user = results.and_then(|r| r.get("USER"));
        let user_id = user.and_then(|u| text(u, "USER_ID"));
        let api_token = results
            .and_then(|r| text(r, "checkForm"))
            .filter(|t| !t.is_empty());
        let license = user
            .and_then(|u| u.get("OPTIONS"))
            .and_then(|o| text(o, "license_token"))
            .filter(|t| !t.is_empty());
        let (Some(api_token), Some(license_token)) = (api_token, license) else {
            return Err(AccountRefused.into());
        };
        if has_error(&root) || matches!(user_id.as_deref(), None | Some("0")) {
            return Err(AccountRefused.into());
        }
        let options = user.and_then(|u| u.get("OPTIONS"));
        let named = |value: Option<String>| value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let facts = AccountFacts {
            offer: named(results.and_then(|r| text(r, "OFFER_NAME"))),
            country: named(results.and_then(|r| text(r, "COUNTRY"))),
            // Either the web or the mobile right gives the format, as deemix reads them.
            lossless: options.and_then(|o| either_flag(o, "web_lossless", "mobile_lossless")),
            high_quality: options.and_then(|o| either_flag(o, "web_hq", "mobile_hq")),
        };

        if let Some(sid) = sid {
            cookie.push_str("; ");
            cookie.push_str(&sid);
        }
        Ok(Session {
            cookie,
            api_token,
            license_token,
            facts,
        })
    }

    /// Which tracks are known to have no audio, for these accounts at this quality: a new ARL
    /// or another quality asks again.
    fn no_media_key(accounts: &[String], track_id: &str, quality: &str) -> String {
        format!("{}\0{track_id}\0{quality}", accounts.join("\u{1}"))
    }

    fn media_key(account: &str, track_id: &str, quality: &str) -> String {
        format!("{account}\0{track_id}\0{quality}")
    }

    async fn resolve_media(
        &self,
        track_id: &str,
        quality: &str,
        rejected_accounts: &HashSet<String>,
    ) -> anyhow::Result<Media> {
        if !valid_track_id(track_id) {
            anyhow::bail!("Invalid Deezer track ID.");
        }
        let settings = self.deezer_settings();
        let none_key = Self::no_media_key(&settings.accounts(), track_id, quality);
        if self.no_media.get(&none_key).is_some() {
            anyhow::bail!(NoMediaKnown);
        }
        // An address already held is handed back without queueing: a seek into the song that is
        // playing must not wait behind another song's sign-in and lookups.
        for arl in settings
            .accounts()
            .into_iter()
            .filter(|arl| !rejected_accounts.contains(arl))
        {
            if let Some(cached) = self.media_cache.get(&Self::media_key(&arl, track_id, quality)) {
                return Ok(cached);
            }
        }
        let _gate = self.media_gate.lock().await;
        if self.no_media.get(&none_key).is_some() {
            anyhow::bail!(NoMediaKnown);
        }
        // Remembered as having none only when every account was asked and answered so.
        let mut every_account_said_none = rejected_accounts.is_empty();
        for arl in settings
            .accounts()
            .into_iter()
            .filter(|arl| !rejected_accounts.contains(arl))
        {
            let key = Self::media_key(&arl, track_id, quality);
            if let Some(cached) = self.media_cache.get(&key) {
                return Ok(cached);
            }
            match self.resolve_with_account(&arl, track_id, quality).await {
                Ok(Some(found)) => {
                    self.media_cache.set(key, found.clone(), 1, Self::MEDIA_TTL);
                    return Ok(found);
                }
                Ok(None) => {}
                // The error's text can name an address; its kind is enough to tell what failed.
                Err(e) => {
                    every_account_said_none = false;
                    debug!("Deezer media attempt failed: {}", kind_of(&e));
                }
            }
        }
        if every_account_said_none {
            self.no_media.set(none_key, (), 1, Self::NO_MEDIA_TTL);
        }
        anyhow::bail!("No full-track Deezer media available. Check ARL, subscription and region.")
    }

    /// The media of `track_id` as this account is given it, or of the same recording under
    /// another id. `None` when the account is given none of them.
    async fn resolve_with_account(
        &self,
        arl: &str,
        track_id: &str,
        quality: &str,
    ) -> anyhow::Result<Option<Media>> {
        let (session, reused) = self.session_for(arl).await?;
        let first = self.resolve_with_session(&session, arl, track_id, quality).await;
        // A kept sign-in may have gone stale before its time. Only Deezer saying so is worth a
        // new sign-in: a song it cannot give, or no answer, would only sign in again for nothing.
        if reused && first.as_ref().is_err_and(|e| e.is::<SignInExpired>()) {
            debug!("Deezer said a kept sign-in is no longer good; signing in again");
            self.forget_session(arl).await;
            let (session, _) = self.session_for(arl).await?;
            return self.resolve_with_session(&session, arl, track_id, quality).await;
        }
        first
    }

    async fn resolve_with_session(
        &self,
        session: &Session,
        arl: &str,
        track_id: &str,
        quality: &str,
    ) -> anyhow::Result<Option<Media>> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut candidate = track_id.to_string();
        let mut candidate_reason = "requested";
        let mut original_identity: Option<(String, String)> = None;
        for _ in 0..3 {
            if !visited.insert(candidate.clone()) {
                break;
            }
            let page = self
                .post(
                    &self.gateway("deezer.pageTrack", &session.api_token),
                    &json!({ "SNG_ID": candidate }),
                    Some(&session.cookie),
                )
                .await?;
            if token_refused(&page) {
                return Err(SignInExpired.into());
            }
            let Some(data) = page
                .get("results")
                .and_then(|r| r.get("DATA"))
                .filter(|_| !has_error(&page))
            else {
                break;
            };
            let actual_id = text(data, "SNG_ID").unwrap_or_else(|| candidate.clone());
            if !valid_track_id(&actual_id) {
                break;
            }

            let candidate_identity = page_track_identity(data);
            if candidate == track_id && actual_id == track_id {
                if original_identity.is_none() {
                    original_identity = candidate_identity.clone();
                }
            } else if candidate == track_id && original_identity.is_none() {
                original_identity = self.catalog.track_identity_by_id(track_id).await;
            }

            let substituted = candidate != track_id || actual_id != candidate;
            let identity_matches =
                !substituted || same_recording(original_identity.as_ref(), candidate_identity.as_ref());
            let substitution_reason = if actual_id == candidate {
                candidate_reason.to_string()
            } else {
                format!("{candidate_reason}+sng-id")
            };
            if substituted {
                debug!(
                    "Deezer source identity check requested {track_id}, candidate {candidate}, actual {actual_id}, reason {substitution_reason}, matched {identity_matches}"
                );
            }
            if !identity_matches {
                break;
            }

            let formats: Vec<Value> = DeezerSettings::formats(quality)
                .iter()
                .filter(|format| format_possible(format, &session.facts, data))
                .map(|format| json!({ "cipher": "BF_CBC_STRIPE", "format": format }))
                .collect();
            if formats.is_empty() {
                debug!("Deezer track {actual_id} has none of the formats asked for that the plan gives");
            }
            if let Some(track_token) =
                text(data, "TRACK_TOKEN").filter(|t| !t.is_empty() && !formats.is_empty())
            {
                let media = self
                    .post(
                        &self.media_url,
                        &json!({
                            "license_token": session.license_token,
                            "media": [{ "type": "FULL", "formats": formats }],
                            "track_tokens": [track_token],
                        }),
                        None,
                    )
                    .await?;
                if license_refused(&media) {
                    return Err(SignInExpired.into());
                }
                if let Some(mut found) = self.select_media(&media, &actual_id, quality) {
                    found.account = arl.to_string();
                    debug!(
                        "Deezer source selected requested {track_id}, candidate {candidate}, actual {actual_id}, reason {substitution_reason}, format {}",
                        found.format
                    );
                    return Ok(Some(found));
                }
            }

            // Alternative bytes must use the alternative track's Blowfish key.
            let fallback = data.get("FALLBACK").and_then(|alt| text(alt, "SNG_ID"));
            if let Some(fallback) = fallback.filter(|id| valid_track_id(id) && !visited.contains(id)) {
                candidate = fallback;
                candidate_reason = "provider-fallback";
                continue;
            }
            let usable = |id: &Option<String>| {
                id.as_ref()
                    .is_some_and(|id| valid_track_id(id) && !visited.contains(id))
            };
            let mut id = None;
            let mut reason = "isrc";
            if let Some(isrc) = text(data, "ISRC").filter(|isrc| !isrc.is_empty()) {
                id = self.catalog.track_id_for_isrc(&isrc).await;
            }
            if !usable(&id) {
                reason = "catalog-alternative";
                id = match &candidate_identity {
                    Some((artist, title)) => {
                        self.catalog
                            .find_alternative_track_id(artist, title, &visited)
                            .await
                    }
                    None => None,
                };
            }
            if !usable(&id) {
                break;
            }
            candidate = id.expect("a usable id is an id");
            candidate_reason = reason;
        }
        Ok(None)
    }

    /// The best format the answer holds an address for, in the order `quality` asks.
    fn select_media(&self, root: &Value, track_id: &str, quality: &str) -> Option<Media> {
        let media = root.get("data")?.as_array()?.first()?.get("media")?.as_array()?;
        for format in DeezerSettings::formats(quality) {
            for item in media {
                if text(item, "format").as_deref() != Some(*format) {
                    continue;
                }
                let Some(sources) = item.get("sources").and_then(Value::as_array) else {
                    continue;
                };
                for source in sources {
                    let Some(url) = source.get("url").and_then(Value::as_str) else {
                        continue;
                    };
                    let secure = reqwest::Url::parse(url).is_ok_and(|u| {
                        u.scheme() == "https" || (self.allow_plain_http && u.scheme() == "http")
                    });
                    if secure {
                        return Some(Media {
                            track_id: track_id.to_string(),
                            format: format.to_string(),
                            url: url.to_string(),
                            account: String::new(),
                        });
                    }
                }
            }
        }
        None
    }

    /// The audio of a track at `quality` or the best lesser format, the whole of it or the
    /// bytes `range_header` asks for. `None` when no account could be given it.
    pub async fn open_stream(
        &self,
        track_id: &str,
        range_header: Option<&str>,
        quality: &str,
    ) -> Option<DeezerOpenStream> {
        let mut rejected_accounts: HashSet<String> = HashSet::new();
        let mut refreshed = false;
        for _ in 0..3 {
            let media = match self.resolve_media(track_id, quality, &rejected_accounts).await {
                Ok(media) => media,
                Err(e) if e.is::<NoMediaKnown>() || e.is::<AccountRefused>() || e.is::<SignInPaused>() => {
                    // Said once already: when first found, or by the account watch.
                    debug!("Deezer stream unavailable for track {track_id}: {e}");
                    return None;
                }
                Err(e) => {
                    warn!("Deezer stream unavailable for track {track_id}: {e}");
                    return None;
                }
            };
            match self.open_media(&media, range_header).await {
                Ok(opened) => return Some(opened),
                Err(e) => {
                    warn!("Deezer stream unavailable for track {track_id}: {}", kind_of(&e));
                    // A media address goes stale: ask for a new one once, then leave the
                    // account that keeps handing out one that does not open.
                    self.media_cache
                        .forget(&Self::media_key(&media.account, track_id, quality));
                    if refreshed {
                        rejected_accounts.insert(media.account.clone());
                    }
                    refreshed = true;
                }
            }
        }
        None
    }

    async fn send_stream(&self, url: &str, range_from: Option<u64>) -> anyhow::Result<reqwest::Response> {
        Ok(self.send_stream_raw(url, range_from).await?.error_for_status()?)
    }

    /// [`Self::send_stream`] without turning an error status into an error, so a seek past the
    /// end (416) can still be read for the song's size.
    async fn send_stream_raw(&self, url: &str, range_from: Option<u64>) -> anyhow::Result<reqwest::Response> {
        let mut request = self.stream_client.get(url);
        if let Some(from) = range_from {
            request = request.header(reqwest::header::RANGE, format!("bytes={from}-"));
        }
        tokio::time::timeout(Self::HEADERS_TIMEOUT, request.send())
            .await
            .map_err(|_| anyhow::anyhow!("Deezer CDN did not answer in time."))?
            .map_err(Into::into)
    }

    async fn open_media(
        &self,
        media: &Media,
        range_header: Option<&str>,
    ) -> anyhow::Result<DeezerOpenStream> {
        let stripe = STRIPE as u64;
        // A seek into the song asks the CDN for the stripe it falls in straight away, and learns
        // the song's size from the answer's Content-Range: one connection, not one for the size
        // and another for the bytes. The stripe start, because decryption starts at a stripe.
        let seek_from = range_header
            .and_then(parse_single_range)
            .and_then(|range| match range {
                ByteRange::From(from, _) => Some(from / stripe * stripe),
                ByteRange::Suffix(_) => None,
            })
            .filter(|aligned| *aligned > 0);

        let mut opened = None;
        if let Some(aligned) = seek_from {
            let response = self.send_stream_raw(&media.url, Some(aligned)).await?;
            match response.status() {
                reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                    if let Some(total) = content_range_total(&response) {
                        return Ok(DeezerOpenStream::unsatisfiable(total, &media.format));
                    }
                    // No size given: ask for the whole song below.
                }
                reqwest::StatusCode::PARTIAL_CONTENT => {
                    if content_range_start(&response) != Some(aligned) {
                        anyhow::bail!("Deezer CDN returned an unexpected byte range.");
                    }
                    let total = content_range_total(&response)
                        .ok_or_else(|| anyhow::anyhow!("Deezer CDN gave no size with a byte range."))?;
                    opened = Some((response, Some(total), aligned / stripe, aligned));
                }
                // The whole song from its first byte, the range ignored: skip up to the start.
                _ => {
                    let response = response.error_for_status()?;
                    let total = response.content_length();
                    opened = Some((response, total, 0, 0));
                }
            }
        }
        // The whole song, or a range from its first stripe or counted from its end: the size
        // comes first, from the whole song's headers.
        let (response, total, start_stripe, from_byte) = match opened {
            Some(opened) => opened,
            None => {
                let response = self.send_stream(&media.url, None).await?;
                let total = response.content_length();
                (response, total, 0, 0)
            }
        };

        let content_type = content_type(&media.format).to_string();
        let mut response = response;
        let mut start_stripe = start_stripe;
        let mut skip = 0u64;
        let mut length = total;
        let mut status = 200u16;
        let mut content_range = None;

        if let Some(total) = total.filter(|t| *t > 0)
            && let Some(window) = byte_window(range_header, total)
        {
            let ByteWindow::Part { start, end } = window else {
                return Ok(DeezerOpenStream::unsatisfiable(total, &media.format));
            };
            length = Some(end - start + 1);
            status = 206;
            content_range = Some(format!("bytes {start}-{end}/{total}"));
            skip = start - from_byte;
            let aligned = start / stripe * stripe;
            if from_byte == 0 && seek_from.is_none() && aligned > 0 {
                // A range counted from the end: its start is known only now.
                drop(response);
                // Fetch to EOF so the final encrypted stripe is never truncated.
                response = self.send_stream(&media.url, Some(aligned)).await?;
                skip = start;
                if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                    if content_range_start(&response) != Some(aligned) {
                        anyhow::bail!("Deezer CDN returned an unexpected byte range.");
                    }
                    start_stripe = aligned / stripe;
                    skip = start - aligned;
                }
                // Else the CDN sent the whole song from its first byte: skip up to the start.
            }
        }

        let decryptor = StripeDecryptor::new(&media.track_id, start_stripe, skip, length);
        Ok(DeezerOpenStream {
            audio_stream: decrypted(
                idle_limited(
                    response.bytes_stream().map_err(std::io::Error::other),
                    Self::BODY_IDLE_TIMEOUT,
                ),
                decryptor,
            ),
            content_type,
            content_length: length,
            status_code: status,
            content_range,
            format: media.format.clone(),
        })
    }

    /// Saves a track at the download quality (or the best lesser format) to
    /// `{dest_without_ext}.flac` or `.mp3`, never over a file already there. The saved path, or
    /// `None` when no account could be given the track. A copy that came short is deleted and
    /// is an error.
    pub async fn download(&self, track_id: &str, dest_without_ext: &str) -> anyhow::Result<Option<String>> {
        self.download_as(track_id, dest_without_ext, &self.download_quality())
            .await
    }

    /// [`Self::download`] at a quality of the caller's: `FLAC_ONLY` saves a FLAC or nothing.
    pub async fn download_as(
        &self,
        track_id: &str,
        dest_without_ext: &str,
        quality: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(opened) = self.open_stream(track_id, None, quality).await else {
            return Ok(None);
        };
        let extension = if opened.format == DEEZER_FORMAT_FLAC {
            ".flac"
        } else {
            ".mp3"
        };
        let path = format!("{dest_without_ext}{extension}");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        let written: anyhow::Result<u64> = async {
            let mut audio = opened.audio_stream;
            let mut size = 0u64;
            while let Some(chunk) = audio.try_next().await? {
                file.write_all(&chunk).await?;
                size += chunk.len() as u64;
            }
            file.flush().await?;
            if size == 0 || opened.content_length.is_some_and(|expected| expected != size) {
                anyhow::bail!("Incomplete Deezer download.");
            }
            Ok(size)
        }
        .await;
        drop(file);
        match written {
            Ok(_) => Ok(Some(path)),
            Err(e) => {
                let _ = tokio::fs::remove_file(&path).await;
                Err(e)
            }
        }
    }

    async fn post(&self, url: &str, body: &Value, cookie: Option<&str>) -> anyhow::Result<Value> {
        let mut request = self.api_client.post(url).json(body);
        if let Some(cookie) = cookie.filter(|c| !c.trim().is_empty()) {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        Ok(request.send().await?.error_for_status()?.json().await?)
    }

    fn gateway(&self, method: &str, token: &str) -> String {
        format!(
            "{}?method={method}&input=3&api_version=1.0&api_token={}",
            self.gateway_url,
            escape_data_string(token)
        )
    }
}

/// The content type of a Deezer format.
pub fn content_type(format: &str) -> &'static str {
    if format == DEEZER_FORMAT_FLAC {
        "audio/flac"
    } else {
        "audio/mpeg"
    }
}

/// A `pageTrack` record's artist and title, the title with its version ("(Live)") when it has one.
fn page_track_identity(data: &Value) -> Option<(String, String)> {
    let artist = text(data, "ART_NAME").filter(|a| !a.trim().is_empty())?;
    let title = text(data, "SNG_TITLE").filter(|t| !t.trim().is_empty())?;
    let version = data
        .get("VERSION")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty());
    Some((
        artist,
        match version {
            Some(version) => format!("{title} {version}"),
            None => title,
        },
    ))
}

fn same_recording(expected: Option<&(String, String)>, candidate: Option<&(String, String)>) -> bool {
    match (expected, candidate) {
        (Some((artist, title)), Some((got_artist, got_title))) => {
            recording_matches(Some(artist), Some(title), Some(got_artist), Some(got_title))
        }
        _ => false,
    }
}

/// A string or number property as text.
fn text(element: &Value, name: &str) -> Option<String> {
    match element.get(name)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// What the account watch says when account `index` (0 is the ARL, 1 the fallback) is found
/// with `now` after `was` (`None`: never checked before): a warning (`true`) when it starts to
/// fail, a note when it signs in again, nothing while it stays as it was.
pub(super) fn account_change(
    index: usize,
    was: Option<Option<&'static str>>,
    now: Option<&'static str>,
) -> Option<(bool, String)> {
    let which = if index == 0 { "ARL" } else { "fallback ARL" };
    if was == Some(now) {
        return None;
    }
    match now {
        Some(ACCOUNT_REFUSED) => Some((
            true,
            format!(
                "Deezer refuses the {which}: it has expired or the account cannot play whole songs. Songs come from the other sources until a new one is saved."
            ),
        )),
        Some(_) => Some((
            true,
            format!("Deezer did not answer the sign-in check for the {which}; it is checked again later."),
        )),
        None if was.is_some() => Some((false, format!("Deezer signs the {which} in again."))),
        None => None,
    }
}

/// `body`, ended with an error when it goes `limit` without a piece while it is being read.
/// The time only runs while a reader asks for the next piece, so a player that pauses is not
/// cut off by its own pause.
pub(super) fn idle_limited<S>(
    body: S,
    limit: Duration,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static
where
    S: futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static,
{
    futures::stream::unfold(Some(Box::pin(body)), move |body| async move {
        let mut body = body?;
        match tokio::time::timeout(limit, futures::StreamExt::next(&mut body)).await {
            Ok(Some(piece)) => Some((piece, Some(body))),
            Ok(None) => None,
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("Deezer sent nothing for {} seconds", limit.as_secs()),
                )),
                None,
            )),
        }
    })
}

/// A yes or no the gateway writes as `true`, `1` or `"1"`. `None` when it is not there.
/// True when either flag is, false when neither is and one says so, `None` when neither says.
fn either_flag(element: &Value, first: &str, second: &str) -> Option<bool> {
    match (flag(element, first), flag(element, second)) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (None, None) => None,
        _ => Some(false),
    }
}

/// Whether `format` is worth asking `get_url` for: the plan gives it (as far as the sign-in
/// says) and the track has a file in it (as far as its `FILESIZE_<format>` says; a track that
/// does not say is asked). Asking for one that is not there only earns an error from Deezer.
fn format_possible(format: &str, facts: &AccountFacts, track: &Value) -> bool {
    let plan_gives = match format {
        DEEZER_FORMAT_FLAC => facts.lossless != Some(false),
        DEEZER_FORMAT_MP3_320 => facts.high_quality != Some(false),
        _ => true,
    };
    let has_file = match track.get(format!("FILESIZE_{format}")) {
        Some(Value::String(size)) => size.trim().parse::<u64>().map_or(true, |size| size > 0),
        Some(Value::Number(size)) => size.as_u64().is_none_or(|size| size > 0),
        _ => true,
    };
    plan_gives && has_file
}

/// The gateway refused the call's token: the sign-in is no longer good.
fn token_refused(root: &Value) -> bool {
    let Some(Value::Object(error)) = root.get("error") else {
        return false;
    };
    error.contains_key("VALID_TOKEN_REQUIRED")
        || error
            .get("GATEWAY_ERROR")
            .and_then(Value::as_str)
            .is_some_and(|message| message.to_ascii_lowercase().contains("invalid api token"))
}

/// `get_url` refused the license token (errors 1000 to 1002): the sign-in is no longer good.
fn license_refused(root: &Value) -> bool {
    root.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("errors").and_then(Value::as_array))
        .flatten()
        .filter_map(|error| error.get("code").and_then(Value::as_i64))
        .any(|code| (1000..=1002).contains(&code))
}

fn flag(element: &Value, name: &str) -> Option<bool> {
    match element.get(name)? {
        Value::Bool(value) => Some(*value),
        Value::Number(number) => number.as_i64().map(|value| value != 0),
        Value::String(text) => match text.trim() {
            "1" | "true" => Some(true),
            "0" | "false" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// The gateway reports failure in an `error` that is not empty: it answers 200 either way.
fn has_error(root: &Value) -> bool {
    match root.get("error") {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
        Some(_) => true,
    }
}

pub(super) fn valid_track_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && id.parse::<i64>().is_ok_and(|value| value > 0)
}

/// What failed, without the error's own text (which may hold an address or a cookie).
fn kind_of(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<reqwest::Error>() {
        Some(e) if e.is_timeout() => "timeout",
        Some(e) if e.is_connect() => "connection failed",
        Some(e) if e.is_status() => "refused by the server",
        Some(e) if e.is_decode() => "unreadable answer",
        Some(_) => "request failed",
        None if error.downcast_ref::<std::io::Error>().is_some() => "read or write failed",
        None => "not available",
    }
}

/// One byte range of a `Range` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange {
    /// `bytes=from-` or `bytes=from-to`.
    From(u64, Option<u64>),
    /// `bytes=-count`: the last `count` bytes.
    Suffix(u64),
}

/// The bytes a `Range` header asks for of a song `total` bytes long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ByteWindow {
    /// `start` to `end`, both included and both inside the song.
    Part { start: u64, end: u64 },
    /// A range that starts past the end.
    Unsatisfiable,
}

/// The window `range_header` names, or `None` for the whole song: no header, or one that is
/// not exactly one range in bytes.
pub(super) fn byte_window(range_header: Option<&str>, total: u64) -> Option<ByteWindow> {
    let range = range_header.and_then(parse_single_range)?;
    if total == 0 {
        return Some(ByteWindow::Unsatisfiable);
    }
    let (start, end) = match range {
        ByteRange::From(from, to) => (from, to.unwrap_or(total - 1).min(total - 1)),
        ByteRange::Suffix(count) => (total.saturating_sub(count), total - 1),
    };
    Some(if start >= total || start > end {
        ByteWindow::Unsatisfiable
    } else {
        ByteWindow::Part { start, end }
    })
}

/// A `Range` header naming exactly one range in bytes; anything else is read as no range.
fn parse_single_range(header: &str) -> Option<ByteRange> {
    let (unit, spec) = header.trim().split_once('=')?;
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return None;
    }
    let (from, to) = spec.trim().split_once('-')?;
    let (from, to) = (from.trim(), to.trim());
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    match (from.is_empty(), to.is_empty()) {
        (true, true) => None,
        (true, false) => Some(ByteRange::Suffix(number(to)?)),
        (false, true) => Some(ByteRange::From(number(from)?, None)),
        (false, false) => {
            let (from, to) = (number(from)?, number(to)?);
            (from <= to).then_some(ByteRange::From(from, Some(to)))
        }
    }
}

/// Where a 206's `Content-Range: bytes from-to/total` starts.
fn content_range_start(response: &reqwest::Response) -> Option<u64> {
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    let range = value.trim().strip_prefix("bytes")?.trim();
    range.split_once('-')?.0.trim().parse().ok()
}

/// The song's size from a `Content-Range: bytes from-to/total` or `bytes */total`.
fn content_range_total(response: &reqwest::Response) -> Option<u64> {
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    value.rsplit_once('/')?.1.trim().parse().ok()
}

#[cfg(test)]
#[path = "deezer_resolver_tests.rs"]
mod tests;
