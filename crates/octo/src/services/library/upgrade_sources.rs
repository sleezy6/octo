use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::settings::{
    DeezerSettings, DownloadSource, LidarrSettings, SettingsStore, SoulseekSettings, UpgradeSourceChoice,
};

use crate::services::soulseek::ISoulseekLink;
use crate::services::soulseek::soulseek_link::SoulseekLinkState;
use crate::services::spotiflac::{Connection, SpotiflacClient};

/// How long SpotiFLAC gets to say it is there, when it is asked whether the lossless step can
/// take a search.
const SPOTIFLAC_ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(3);

/// Where Better quality looks for a lossless copy, decided in one place so the dashboard, the
/// apps, the queue and the weekly worker never disagree. Only lossless sources count: YouTube can
/// only produce an MP3, which a Better quality check refuses anyway. Deezer counts, asked for
/// FLAC and nothing lesser. The Soulseek step is the lossless step: Soulseek, and SpotiFLAC
/// when it is on (in the order its placement gives), so with SpotiFLAC on the step is there
/// even while Soulseek is off or not logged in.
pub struct UpgradeSources {
    /// The library action, Soulseek and Lidarr settings, read at every use.
    settings: Arc<SettingsStore>,
    soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    /// SpotiFLAC, asked whether it is there when it alone stands the lossless step up.
    spotiflac: Option<Arc<SpotiflacClient>>,
}

impl UpgradeSources {
    pub fn new(settings: Arc<SettingsStore>, soulseek_link: Option<Arc<dyn ISoulseekLink>>) -> Self {
        UpgradeSources {
            settings,
            soulseek_link,
            spotiflac: None,
        }
    }

    /// The same, with SpotiFLAC to ask whether it is there.
    pub fn with_spotiflac(mut self, spotiflac: Arc<SpotiflacClient>) -> Self {
        self.spotiflac = Some(spotiflac);
        self
    }

    /// Soulseek is switched on and slskd's address and sign-in are set.
    pub fn soulseek_set_up(settings: &SoulseekSettings) -> bool {
        settings.is_on()
            && !dotnet::is_null_or_white_space(settings.base_url.as_deref())
            && !dotnet::is_null_or_white_space(settings.username.as_deref())
            && !dotnet::is_null_or_white_space(settings.password.as_deref())
    }

    /// Lidarr's address, key and the choices it needs to add an album are set.
    pub fn lidarr_set_up(settings: &LidarrSettings) -> bool {
        !dotnet::is_null_or_white_space(settings.base_url.as_deref())
            && !dotnet::is_null_or_white_space(settings.api_key.as_deref())
            && !dotnet::is_null_or_white_space(settings.root_folder_path.as_deref())
            && settings.quality_profile_id > 0
            && settings.metadata_profile_id > 0
    }

    /// A Deezer account is set. Whether its plan gives FLAC is only known when a song is asked
    /// for: an account that is not given one is passed over for the next source.
    pub fn deezer_set_up(settings: &DeezerSettings) -> bool {
        settings.is_configured()
    }

    /// The sources the setting allows, in the order they are tried, set up or not.
    pub fn wanted(&self) -> Vec<DownloadSource> {
        match self.settings.current().library_actions.upgrade_source {
            UpgradeSourceChoice::Soulseek => vec![DownloadSource::Soulseek],
            UpgradeSourceChoice::Lidarr => vec![DownloadSource::Lidarr],
            UpgradeSourceChoice::Deezer => vec![DownloadSource::Deezer],
            // Deezer first: it names the very recording and answers in seconds.
            _ => vec![
                DownloadSource::Deezer,
                DownloadSource::Soulseek,
                DownloadSource::Lidarr,
            ],
        }
    }

    /// The allowed sources that are set up, in the order they are tried.
    pub fn plan(&self) -> Vec<DownloadSource> {
        self.wanted().into_iter().filter(|s| self.set_up(*s)).collect()
    }

    /// Whether any allowed source is set up, so Better quality can be offered at all.
    pub fn ready(&self) -> bool {
        !self.plan().is_empty()
    }

    /// The sources in words, for "Looking for a higher quality copy on X": the ones that are set
    /// up, or, when none is, the ones the setting asks for. The Soulseek step is named for what
    /// it asks: Soulseek, Mornye (SpotiFLAC before its rename), or both.
    pub fn name(&self) -> String {
        let plan = self.plan();
        let sources = if plan.is_empty() { self.wanted() } else { plan };
        let settings = self.settings.current();
        let spotiflac = settings.spotiflac.is_on();
        let soulseek = Self::soulseek_set_up(&settings.soulseek);
        let mut words: Vec<&str> = Vec::new();
        for source in sources {
            let word = match source {
                DownloadSource::Soulseek if spotiflac && soulseek => "Soulseek or Mornye",
                DownloadSource::Soulseek if spotiflac => "Mornye",
                other => Self::word(other),
            };
            if !words.contains(&word) {
                words.push(word);
            }
        }
        words.join(" or ")
    }

    /// The sources that can take a search right now. The lossless step drops out while slskd
    /// is not logged in (or not set up) and SpotiFLAC, when it is on, does not answer; empty
    /// means every source is out, and the request should wait rather than fail.
    pub async fn available(&self) -> Vec<DownloadSource> {
        let plan = self.plan();
        let Some(link) = self.soulseek_link.as_ref() else {
            return plan;
        };
        if !plan.contains(&DownloadSource::Soulseek) {
            return plan;
        }
        let settings = self.settings.current();
        let slskd_answers = Self::soulseek_set_up(&settings.soulseek)
            && link.read(false).await.map(|reading| reading.link) != Some(SoulseekLinkState::NotLoggedIn);
        if slskd_answers || (settings.spotiflac.is_on() && self.spotiflac_answers().await) {
            plan
        } else {
            plan.into_iter()
                .filter(|s| *s != DownloadSource::Soulseek)
                .collect()
        }
    }

    /// Whether SpotiFLAC is there to take a search: it answers its info call within
    /// [`SPOTIFLAC_ANSWERS_WITHIN`]. Taken as there when no client was given to ask.
    async fn spotiflac_answers(&self) -> bool {
        let Some(client) = self.spotiflac.as_ref() else {
            return true;
        };
        let connection = Connection::of(&client.settings());
        tokio::time::timeout(SPOTIFLAC_ANSWERS_WITHIN, client.info(&connection))
            .await
            .is_ok_and(|answer| answer.is_ok())
    }

    /// Whether the only thing between this request and a search is an outage of the lossless
    /// step: slskd not logged in, and SpotiFLAC off or not answering.
    pub async fn waiting_for_soulseek(&self) -> bool {
        self.plan().contains(&DownloadSource::Soulseek) && self.available().await.is_empty()
    }

    pub fn words(sources: impl IntoIterator<Item = DownloadSource>) -> String {
        let mut words: Vec<&str> = Vec::new();
        for source in sources {
            let word = Self::word(source);
            if !words.contains(&word) {
                words.push(word);
            }
        }
        words.join(" or ")
    }

    pub fn word(source: DownloadSource) -> &'static str {
        match source {
            DownloadSource::Lidarr => "Lidarr",
            DownloadSource::YouTube => "YouTube",
            DownloadSource::Deezer => "Deezer",
            _ => "Soulseek",
        }
    }

    fn set_up(&self, source: DownloadSource) -> bool {
        let settings = self.settings.current();
        match source {
            DownloadSource::Soulseek => {
                Self::soulseek_set_up(&settings.soulseek) || settings.spotiflac.is_on()
            }
            DownloadSource::Lidarr => Self::lidarr_set_up(&settings.lidarr),
            DownloadSource::Deezer => Self::deezer_set_up(&settings.deezer),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Where Better quality looks for a lossless copy: Soulseek, Lidarr, or both in that order.
    //! One place decides it, so the dashboard, the apps and the workers never disagree. Only
    //! sources that are set up count, and a Soulseek outage leaves Lidarr rather than stopping
    //! everything.

    use async_trait::async_trait;
    use chrono::{DateTime, TimeDelta, Utc};
    use octo_core::settings::{AppSettings, LibraryActionSettings};

    use super::*;
    use crate::services::soulseek::soulseek_link::SoulseekServerReading;

    fn slskd() -> SoulseekSettings {
        SoulseekSettings {
            base_url: Some("http://slskd:5030".into()),
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        }
    }

    fn lidarr() -> LidarrSettings {
        LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            root_folder_path: Some("/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            ..Default::default()
        }
    }

    struct FixedLink(SoulseekLinkState);

    #[async_trait]
    impl ISoulseekLink for FixedLink {
        async fn read(&self, _fresh: bool) -> Option<SoulseekServerReading> {
            Some(SoulseekServerReading::new(self.0, None, None, None))
        }

        fn hold_limit(&self) -> TimeDelta {
            TimeDelta::zero()
        }

        fn utc_now(&self) -> DateTime<Utc> {
            Utc::now()
        }

        async fn wait_for_login(&self, _until: DateTime<Utc>) -> bool {
            true
        }
    }

    fn sources(
        choice: UpgradeSourceChoice,
        soulseek: Option<SoulseekSettings>,
        lidarr: Option<LidarrSettings>,
        link: SoulseekLinkState,
    ) -> UpgradeSources {
        UpgradeSources::new(
            Arc::new(SettingsStore::for_tests(AppSettings {
                library_actions: LibraryActionSettings {
                    upgrade_source: choice,
                    ..Default::default()
                },
                soulseek: soulseek.unwrap_or_default(),
                lidarr: lidarr.unwrap_or_default(),
                ..Default::default()
            })),
            Some(Arc::new(FixedLink(link))),
        )
    }

    fn logged_in(
        choice: UpgradeSourceChoice,
        soulseek: Option<SoulseekSettings>,
        lidarr: Option<LidarrSettings>,
    ) -> UpgradeSources {
        sources(choice, soulseek, lidarr, SoulseekLinkState::LoggedIn)
    }

    fn with_deezer(choice: UpgradeSourceChoice, arl: Option<&str>) -> UpgradeSources {
        UpgradeSources::new(
            Arc::new(SettingsStore::for_tests(AppSettings {
                library_actions: LibraryActionSettings {
                    upgrade_source: choice,
                    ..Default::default()
                },
                soulseek: slskd(),
                lidarr: lidarr(),
                deezer: DeezerSettings {
                    arl: arl.map(str::to_string),
                    ..Default::default()
                },
                ..Default::default()
            })),
            Some(Arc::new(FixedLink(SoulseekLinkState::LoggedIn))),
        )
    }

    /// Soulseek switched off is no source, whatever slskd's address and sign-in say.
    #[test]
    fn soulseek_switched_off_is_not_a_source() {
        let off = SoulseekSettings {
            enabled: false,
            ..slskd()
        };
        assert!(!UpgradeSources::soulseek_set_up(&off));
        assert!(UpgradeSources::soulseek_set_up(&slskd()));
        let sources = logged_in(UpgradeSourceChoice::Auto, Some(off.clone()), Some(lidarr()));
        assert_eq!(sources.plan(), [DownloadSource::Lidarr]);
        let alone = logged_in(UpgradeSourceChoice::Soulseek, Some(off), None);
        assert!(!alone.ready());
    }

    /// With a Deezer account set, Automatic asks Deezer first; without one it is as it was.
    #[test]
    fn auto_asks_deezer_first_when_an_account_is_set() {
        let sources = with_deezer(UpgradeSourceChoice::Auto, Some("arl"));
        assert_eq!(
            sources.plan(),
            [
                DownloadSource::Deezer,
                DownloadSource::Soulseek,
                DownloadSource::Lidarr
            ]
        );
        assert_eq!(sources.name(), "Deezer or Soulseek or Lidarr");

        let without = with_deezer(UpgradeSourceChoice::Auto, None);
        assert_eq!(without.plan(), [DownloadSource::Soulseek, DownloadSource::Lidarr]);
    }

    /// Deezer alone: ready with an account, and not without one whatever else is set up.
    #[tokio::test]
    async fn deezer_only_needs_the_account_and_never_waits_for_soulseek() {
        let sources = with_deezer(UpgradeSourceChoice::Deezer, Some("arl"));
        assert_eq!(sources.plan(), [DownloadSource::Deezer]);
        assert!(sources.ready());
        assert_eq!(sources.available().await, [DownloadSource::Deezer]);
        assert!(!sources.waiting_for_soulseek().await);

        let without = with_deezer(UpgradeSourceChoice::Deezer, Some("  "));
        assert!(!without.ready());
        assert_eq!(without.name(), "Deezer");
    }

    #[test]
    fn auto_tries_soulseek_then_lidarr() {
        let sources = logged_in(UpgradeSourceChoice::Auto, Some(slskd()), Some(lidarr()));

        assert_eq!(sources.plan(), [DownloadSource::Soulseek, DownloadSource::Lidarr]);
        assert!(sources.ready());
        assert_eq!(sources.name(), "Soulseek or Lidarr");
    }

    #[test]
    fn auto_uses_only_what_is_set_up() {
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, None, Some(lidarr())).plan(),
            [DownloadSource::Lidarr]
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, None, Some(lidarr())).name(),
            "Lidarr"
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Auto, Some(slskd()), None).plan(),
            [DownloadSource::Soulseek]
        );
    }

    #[test]
    fn a_chosen_source_is_the_only_one() {
        assert_eq!(
            logged_in(UpgradeSourceChoice::Lidarr, Some(slskd()), Some(lidarr())).plan(),
            [DownloadSource::Lidarr]
        );
        assert_eq!(
            logged_in(UpgradeSourceChoice::Soulseek, Some(slskd()), Some(lidarr())).plan(),
            [DownloadSource::Soulseek]
        );
    }

    #[test]
    fn nothing_set_up_is_not_ready_and_names_what_it_wants() {
        let sources = logged_in(UpgradeSourceChoice::Lidarr, Some(slskd()), None);

        assert!(sources.plan().is_empty());
        assert!(!sources.ready());
        assert_eq!(sources.name(), "Lidarr");
    }

    #[test]
    fn lidarr_needs_its_root_folder_and_profiles_to_add_an_album() {
        let half = LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            ..Default::default()
        };

        assert!(!UpgradeSources::lidarr_set_up(&half));
        assert!(UpgradeSources::lidarr_set_up(&lidarr()));
    }

    #[tokio::test]
    async fn a_soulseek_outage_leaves_lidarr() {
        let sources = sources(
            UpgradeSourceChoice::Auto,
            Some(slskd()),
            Some(lidarr()),
            SoulseekLinkState::NotLoggedIn,
        );

        assert_eq!(sources.available().await, [DownloadSource::Lidarr]);
        assert!(!sources.waiting_for_soulseek().await);
    }

    #[tokio::test]
    async fn with_only_soulseek_an_outage_means_wait() {
        let sources = sources(
            UpgradeSourceChoice::Auto,
            Some(slskd()),
            None,
            SoulseekLinkState::NotLoggedIn,
        );

        assert!(sources.available().await.is_empty());
        assert!(sources.waiting_for_soulseek().await);
    }

    #[tokio::test]
    async fn lidarr_alone_never_waits_for_soulseek() {
        let sources = sources(
            UpgradeSourceChoice::Lidarr,
            Some(slskd()),
            Some(lidarr()),
            SoulseekLinkState::NotLoggedIn,
        );

        assert_eq!(sources.available().await, [DownloadSource::Lidarr]);
        assert!(!sources.waiting_for_soulseek().await);
    }

    /// SpotiFLAC rides on the Soulseek step: with it on, the step is planned and searchable
    /// while Soulseek is off or not logged in, and it is named for what it asks.
    #[tokio::test]
    async fn spotiflac_keeps_the_lossless_step_without_soulseek() {
        let settings = |soulseek: Option<SoulseekSettings>| {
            Arc::new(SettingsStore::for_tests(AppSettings {
                library_actions: LibraryActionSettings {
                    upgrade_source: UpgradeSourceChoice::Soulseek,
                    ..Default::default()
                },
                soulseek: soulseek.unwrap_or_default(),
                spotiflac: octo_core::settings::SpotiflacSettings {
                    enabled: true,
                    url: "http://spotiflac:8000".into(),
                    ..Default::default()
                },
                ..Default::default()
            }))
        };
        let alone = UpgradeSources::new(
            settings(None),
            Some(Arc::new(FixedLink(SoulseekLinkState::NotLoggedIn))),
        );
        assert_eq!(alone.plan(), [DownloadSource::Soulseek]);
        assert_eq!(alone.available().await, [DownloadSource::Soulseek]);
        assert_eq!(alone.name(), "Mornye");
        let both = UpgradeSources::new(settings(Some(slskd())), None);
        assert_eq!(both.name(), "Soulseek or Mornye");
    }

    /// With slskd not logged in, the lossless step is there only while SpotiFLAC answers: a
    /// SpotiFLAC outage with no other source means wait, as a Soulseek one does, rather than
    /// a run that finds nothing and stamps the song for four weeks.
    #[tokio::test]
    async fn the_lossless_step_waits_while_neither_slskd_nor_spotiflac_answers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                upgrade_source: UpgradeSourceChoice::Soulseek,
                ..Default::default()
            },
            soulseek: slskd(),
            spotiflac: octo_core::settings::SpotiflacSettings {
                enabled: true,
                url: server.uri(),
                ..Default::default()
            },
            ..Default::default()
        }));
        let sources = UpgradeSources::new(
            settings.clone(),
            Some(Arc::new(FixedLink(SoulseekLinkState::NotLoggedIn))),
        )
        .with_spotiflac(Arc::new(SpotiflacClient::new(settings.clone())));

        // Nothing mounted: SpotiFLAC's address answers 404, which is not SpotiFLAC.
        assert!(sources.available().await.is_empty());
        assert!(sources.waiting_for_soulseek().await);

        Mock::given(method("GET"))
            .and(path("/api/v1/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "SpotiFLAC", "version": "5.0.5", "api_version": "v1",
                "multiuser": false, "authenticated": true
            })))
            .mount(&server)
            .await;
        assert_eq!(sources.available().await, [DownloadSource::Soulseek]);
        assert!(!sources.waiting_for_soulseek().await);

        // slskd back: SpotiFLAC is not asked.
        let logged_in = UpgradeSources::new(
            settings.clone(),
            Some(Arc::new(FixedLink(SoulseekLinkState::LoggedIn))),
        )
        .with_spotiflac(Arc::new(SpotiflacClient::new(settings)));
        server.reset().await;
        assert_eq!(logged_in.available().await, [DownloadSource::Soulseek]);
    }
}
