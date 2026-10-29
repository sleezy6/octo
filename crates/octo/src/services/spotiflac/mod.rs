//! The person's own Mornye (SpotiFLAC before its rename) server, reached through its web API (`spotiflac --web`): the
//! client the download path asks for songs, the gate that keeps to the number of songs asked for
//! at once, and the dashboard's Test, which says plainly what is missing.
//!
//! Octo sends Mornye a catalog link and a folder, and takes the file Mornye writes there.
//! How Mornye gets the audio is up to the extensions the person installed in it.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet::escape_data_string;
use octo_core::settings::{SettingsStore, SpotiflacSettings};
use octo_core::spotiflac::{
    CatalogTrack, ErrorAnswer, ExtensionsAnswer, Job, SearchAnswer, ServerInfo, path_for_spotiflac,
};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::Notify;

use crate::services::http_client_factory::{
    self, connect_failure_message, status_failure_message, timeout_message,
};

/// The cookies the web token is sent in: Mornye's, and SpotiFLAC's from before its rename, so
/// either one reads it.
pub const TOKEN_COOKIES: [&str; 2] = ["mornye_web_token", "spotiflac_web_token"];

/// Where Mornye's downloads land, under Octo's music folder: a dot folder, which Navidrome's
/// scanner skips.
pub const SPOTIFLAC_FOLDER: &str = ".octo-incoming/spotiflac";

/// How long one call to Mornye's API may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A call to Mornye that did not work, in words a person can act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpotiflacError {
    #[error("Octo could not reach Mornye at {url}. {why}")]
    Unreachable { url: String, why: String },
    #[error(
        "Mornye refused the web token. Set the token Mornye was started with: SPOTIFLAC_WEB_TOKEN with Octo's spotiflac service, otherwise its --web-token or MORNYE_WEB_TOKEN."
    )]
    Unauthorized,
    #[error(
        "{url} answers, but not as Mornye's web API (or an older SpotiFLAC's). Check that Mornye runs with --web and that the address points at it."
    )]
    NotSpotiflac { url: String },
    #[error("Mornye said: {0}")]
    Said(String),
}

/// One connection's address and token, as the settings or a Test hold them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub url: String,
    pub token: String,
}

impl Connection {
    pub fn of(settings: &SpotiflacSettings) -> Connection {
        Connection {
            url: settings.effective_url(),
            token: settings.token.trim().to_string(),
        }
    }
}

/// The client. Settings are read on every call, so a change on the dashboard applies at once.
pub struct SpotiflacClient {
    settings: Arc<SettingsStore>,
    http: reqwest::Client,
    busy: Mutex<usize>,
    freed: Notify,
    poll_interval: Duration,
    song_timeout: Option<Duration>,
}

/// A place among the songs asked for at once, given back when dropped.
pub struct Slot<'a> {
    client: &'a SpotiflacClient,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut busy = self.client.busy.lock();
        *busy = busy.saturating_sub(1);
        drop(busy);
        self.client.freed.notify_waiters();
    }
}

impl SpotiflacClient {
    pub fn new(settings: Arc<SettingsStore>) -> SpotiflacClient {
        Self::with_poll_interval(settings, Duration::from_secs(2))
    }

    /// The same, asking about a job this often (tests ask quickly).
    pub fn with_poll_interval(settings: Arc<SettingsStore>, poll_interval: Duration) -> SpotiflacClient {
        SpotiflacClient {
            settings,
            http: http_client_factory::builder()
                .timeout(CALL_TIMEOUT)
                .build()
                .expect("the Mornye client builds"),
            busy: Mutex::new(0),
            freed: Notify::new(),
            poll_interval,
            song_timeout: None,
        }
    }

    /// The same, with one song's time fixed instead of read from the settings (tests wait
    /// briefly).
    pub fn with_song_timeout(mut self, timeout: Duration) -> SpotiflacClient {
        self.song_timeout = Some(timeout);
        self
    }

    /// How long one song may take.
    pub fn song_timeout(&self) -> Duration {
        self.song_timeout
            .unwrap_or_else(|| Duration::from_secs(self.settings().effective_timeout_seconds()))
    }

    /// The settings as they are now.
    pub fn settings(&self) -> SpotiflacSettings {
        self.settings.current().spotiflac.clone()
    }

    /// On, with an address.
    pub fn is_on(&self) -> bool {
        self.settings.current().spotiflac.is_on()
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Waits for a place among the songs asked for at once.
    pub async fn slot(&self) -> Slot<'_> {
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            {
                let mut busy = self.busy.lock();
                if *busy < self.settings().effective_at_once() {
                    *busy += 1;
                    return Slot { client: self };
                }
            }
            freed.await;
        }
    }

    fn request(
        &self,
        connection: &Connection,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::RequestBuilder {
        let builder = self
            .http
            .request(method, format!("{}{path}", connection.url.trim_end_matches('/')));
        if connection.token.is_empty() {
            builder
        } else {
            builder.header(
                reqwest::header::COOKIE,
                TOKEN_COOKIES
                    .iter()
                    .map(|name| format!("{name}={}", connection.token))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        }
    }

    async fn send<T: DeserializeOwned>(
        &self,
        connection: &Connection,
        request: reqwest::RequestBuilder,
    ) -> Result<Option<T>, SpotiflacError> {
        let response = request.send().await.map_err(|e| SpotiflacError::Unreachable {
            url: connection.url.clone(),
            why: if e.is_timeout() {
                timeout_message(CALL_TIMEOUT)
            } else {
                connect_failure_message(&e)
            },
        })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(SpotiflacError::Unauthorized);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let said = serde_json::from_str::<ErrorAnswer>(&body)
                .ok()
                .map(|answer| match answer.detail.filter(|d| !d.trim().is_empty()) {
                    Some(detail) if !answer.error.is_empty() => format!("{} {detail}", answer.error),
                    Some(detail) => detail,
                    None => answer.error,
                })
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| status_failure_message(status));
            return Err(SpotiflacError::Said(said));
        }
        serde_json::from_str::<T>(&body)
            .map(Some)
            .map_err(|_| SpotiflacError::NotSpotiflac {
                url: connection.url.clone(),
            })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        connection: &Connection,
        path: &str,
    ) -> Result<Option<T>, SpotiflacError> {
        self.send(connection, self.request(connection, reqwest::Method::GET, path))
            .await
    }

    /// `GET /api/v1/info`.
    pub async fn info(&self, connection: &Connection) -> Result<ServerInfo, SpotiflacError> {
        match self.get::<ServerInfo>(connection, "/api/v1/info").await? {
            Some(info) if info.is_spotiflac() => Ok(info),
            _ => Err(SpotiflacError::NotSpotiflac {
                url: connection.url.clone(),
            }),
        }
    }

    /// `GET /api/v1/extensions`.
    pub async fn extensions(&self, connection: &Connection) -> Result<ExtensionsAnswer, SpotiflacError> {
        Ok(self
            .get::<ExtensionsAnswer>(connection, "/api/v1/extensions")
            .await?
            .unwrap_or_default())
    }

    /// `GET /api/v1/search?q=`: the catalog tracks Mornye finds for the words.
    pub async fn search(
        &self,
        connection: &Connection,
        query: &str,
    ) -> Result<Vec<CatalogTrack>, SpotiflacError> {
        let request = self
            .request(connection, reqwest::Method::GET, "/api/v1/search")
            .query(&[("q", query), ("limit", "10")]);
        Ok(self
            .send::<SearchAnswer>(connection, request)
            .await?
            .map(|answer| answer.tracks)
            .unwrap_or_default())
    }

    /// `POST /api/v1/downloads`: asks Mornye to save the track at `link` into `output_dir`, a
    /// folder on Mornye's side.
    pub async fn queue(
        &self,
        connection: &Connection,
        link: &str,
        settings: &SpotiflacSettings,
        output_dir: &str,
    ) -> Result<Job, SpotiflacError> {
        #[derive(Serialize)]
        struct Ask<'a> {
            url: &'a str,
            quality: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            services: Option<Vec<String>>,
            output_dir: &'a str,
        }
        let services = settings.effective_services();
        let ask = Ask {
            url: link,
            quality: settings.effective_quality(),
            services: (!services.is_empty()).then_some(services),
            output_dir,
        };
        let request = self
            .request(connection, reqwest::Method::POST, "/api/v1/downloads")
            .json(&ask);
        self.send::<Job>(connection, request)
            .await?
            .ok_or_else(|| SpotiflacError::NotSpotiflac {
                url: connection.url.clone(),
            })
    }

    /// `GET /api/v1/downloads/{id}`, or None when Mornye no longer knows the job.
    pub async fn job(&self, connection: &Connection, id: &str) -> Result<Option<Job>, SpotiflacError> {
        self.get::<Job>(
            connection,
            &format!("/api/v1/downloads/{}", escape_data_string(id)),
        )
        .await
    }

    /// `POST /api/v1/downloads/{id}/cancel`. A failure is only logged: the job folder is
    /// removed either way.
    pub async fn cancel(&self, connection: &Connection, id: &str) {
        let request = self.request(
            connection,
            reqwest::Method::POST,
            &format!("/api/v1/downloads/{}/cancel", escape_data_string(id)),
        );
        if let Err(e) = self.send::<serde_json::Value>(connection, request).await {
            tracing::debug!("Mornye did not cancel job {id}: {e}");
        }
    }

    /// The dashboard's Test: whether Mornye answers at the address, with the token, has an
    /// extension to download with, and whether Octo can make the folder downloads land in.
    /// Nothing is downloaded.
    pub async fn test(
        &self,
        connection: &Connection,
        enabled: bool,
        folder: Option<&str>,
        download_path: &str,
    ) -> TestReport {
        let mut report = TestReport::default();
        if connection.url.is_empty() {
            report.fail(
                "No address yet. Type the address of your Mornye server, for example http://spotiflac:8000.",
            );
            return report.finish();
        }
        if !(connection.url.starts_with("http://") || connection.url.starts_with("https://")) {
            report.fail("The address must start with http:// or https://.");
            return report.finish();
        }
        if let Some(problem) = SpotiflacSettings::token_problem(&connection.token) {
            report.fail(problem);
            return report.finish();
        }
        match self.info(connection).await {
            Ok(info) => {
                report.version = Some(info.version.clone()).filter(|v| !v.is_empty());
                let version = report
                    .version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default();
                report.pass(&format!("Mornye{version} answers at {}.", connection.url));
                if info.multiuser {
                    report.fail(
                        "Mornye runs with --web-multiuser, where it saves into each account's own folder. \
                         Run it with a web token instead (SPOTIFLAC_WEB_TOKEN with Octo's spotiflac service), so Octo can say where to save.",
                    );
                }
                if connection.token.is_empty() && !info.authenticated {
                    report.warn(
                        "Mornye has no web token, so anyone who can reach its address can use it. \
                         Set SPOTIFLAC_WEB_TOKEN (Mornye's own name: MORNYE_WEB_TOKEN) and the same token here.",
                    );
                }
            }
            Err(e) => {
                report.fail(&e.to_string());
                return report.finish();
            }
        }
        match self.extensions(connection).await {
            Ok(extensions) => {
                let installed = extensions.installed();
                report.extensions = Some(installed);
                if installed == 0 {
                    report.fail(
                        "Mornye has no extensions installed, so it cannot download anything yet. \
                         Mornye installs every extension in your registry by itself, when it starts and again every hour: \
                         check the registry address (SPOTIFLAC_REGISTRIES with Octo's spotiflac service, MORNYE_REGISTRIES \
                         on a Mornye you run yourself), then restart Mornye.",
                    );
                } else {
                    report.pass(&format!(
                        "{installed} {} installed in Mornye.",
                        if installed == 1 { "extension" } else { "extensions" }
                    ));
                }
            }
            Err(e) => report.warn(&format!("Octo could not read Mornye's extensions. {e}")),
        }
        if download_path.trim().is_empty() {
            report.fail("Octo has no music folder yet, so it has nowhere for Mornye's downloads to land.");
        } else {
            let octo_side = format!("{}/{SPOTIFLAC_FOLDER}", download_path.trim_end_matches('/'));
            match make_shared_folder(&octo_side) {
                Ok(()) => {
                    let theirs = path_for_spotiflac(&octo_side, download_path, folder);
                    report.pass(&format!(
                        "Downloads land in {theirs} on Mornye's side, which must be the same folder as {octo_side} in Octo."
                    ));
                }
                Err(e) => report.fail(&format!("Octo cannot make the folder {octo_side}: {e}.")),
            }
        }
        if !enabled {
            report.warn("The switch is off, so downloads do not ask Mornye yet. Turn it on and save.");
        }
        report.finish()
    }
}

/// Makes a folder Mornye writes into. Mornye's container usually runs as another user
/// than Octo's, so the folder is open to every user.
pub fn make_shared_folder(path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        let mut at = Path::new(path);
        // The folder and its parents up to the incoming folder.
        for _ in 0..3 {
            let _ = std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o777));
            if at.file_name().is_some_and(|name| name == ".octo-incoming") {
                break;
            }
            match at.parent() {
                Some(parent) => at = parent,
                None => break,
            }
        }
    }
    Ok(())
}

/// One line of a Test: passed, a warning, or what is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestCheck {
    pub state: &'static str,
    pub text: String,
}

/// What a Test found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TestReport {
    pub ok: bool,
    pub message: String,
    pub version: Option<String>,
    pub extensions: Option<usize>,
    pub checks: Vec<TestCheck>,
}

impl TestReport {
    fn pass(&mut self, text: &str) {
        self.checks.push(TestCheck {
            state: "ok",
            text: text.to_string(),
        });
    }

    fn warn(&mut self, text: &str) {
        self.checks.push(TestCheck {
            state: "warn",
            text: text.to_string(),
        });
    }

    fn fail(&mut self, text: &str) {
        self.checks.push(TestCheck {
            state: "fail",
            text: text.to_string(),
        });
    }

    fn finish(mut self) -> TestReport {
        let failed = self.checks.iter().find(|check| check.state == "fail");
        self.ok = failed.is_none();
        self.message = match failed {
            Some(check) => check.text.clone(),
            None if self.checks.iter().any(|check| check.state == "warn") => {
                "Mornye is ready, with a warning below.".to_string()
            }
            None => "Mornye is ready.".to_string(),
        };
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        json!(self)
    }
}

#[cfg(test)]
#[path = "spotiflac_client_tests.rs"]
mod tests;
