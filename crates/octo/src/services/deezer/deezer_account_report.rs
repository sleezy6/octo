//! What the dashboard says about the Deezer accounts: one verdict for the status card, and the
//! notes a person needs to act on (an ARL that expired, a plan that gives less than Octo is set
//! to ask for). Worked out from a sign-in check ([`DeezerResolver::account_health`]) and the
//! qualities in the settings; nothing here talks to Deezer.

use octo_core::settings::DeezerSettings;
use octo_core::settings::deezer::{DEEZER_FORMAT_FLAC, DEEZER_FORMAT_MP3_320};
use serde::Serialize;

use super::deezer_resolver::{ACCOUNT_REFUSED, DeezerAccountHealth};

/// The accounts as one answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeezerAccountReport {
    /// Deezer can be asked for songs: at least one account signs in.
    pub ok: bool,
    /// Something needs a look although songs still come.
    pub warning: bool,
    /// One line for the status card.
    pub detail: String,
    /// What to do about it, one sentence each. Empty when all is well.
    pub notes: Vec<String>,
}

fn name(index: usize) -> &'static str {
    if index == 0 { "ARL" } else { "fallback ARL" }
}

fn failure(index: usize, account: &DeezerAccountHealth) -> String {
    if account.problem == Some(ACCOUNT_REFUSED) {
        format!("the {} is refused", name(index))
    } else {
        format!("Deezer did not answer for the {}", name(index))
    }
}

/// The verdict on `accounts` (the ARL, then the fallback when one is set), for a server that
/// keeps songs at `settings.quality` and plays them at `settings.stream_quality` when
/// `streams` (outside songs play from Deezer).
pub fn report(
    accounts: &[DeezerAccountHealth],
    settings: &DeezerSettings,
    streams: bool,
) -> DeezerAccountReport {
    let Some((active_index, active)) = accounts.iter().enumerate().find(|(_, account)| account.signed_in)
    else {
        let refused = accounts
            .iter()
            .any(|account| account.problem == Some(ACCOUNT_REFUSED));
        return DeezerAccountReport {
            ok: false,
            warning: false,
            detail: if refused {
                "ARL refused or expired".to_string()
            } else {
                "Deezer did not answer, so the ARL could not be checked".to_string()
            },
            notes: if refused {
                vec![
                    "Deezer refuses the ARL: it has expired, or the account cannot play whole songs. Sign in at deezer.com, copy the new arl cookie and save it here. Until then songs come from the other sources."
                        .to_string(),
                ]
            } else {
                vec!["Deezer did not answer the sign-in check. Nothing is known about the ARL; check again in a moment.".to_string()]
            },
        };
    };

    let mut notes = Vec::new();
    let mut detail = match &active.offer {
        Some(offer) => format!("signed in ({offer})"),
        None => "signed in".to_string(),
    };
    for (index, account) in accounts.iter().enumerate() {
        if account.signed_in {
            continue;
        }
        let what = failure(index, account);
        if index < active_index {
            detail = format!("{what}; the {} signs in", name(active_index));
        } else {
            detail = format!("{detail}; {what}");
        }
        notes.push(if account.problem == Some(ACCOUNT_REFUSED) {
            format!(
                "Deezer refuses the {}: it has expired, or the account cannot play whole songs. Save a new one, or clear the field.",
                name(index)
            )
        } else {
            format!(
                "Deezer did not answer the sign-in check for the {}. Check again in a moment.",
                name(index)
            )
        });
    }

    // A plan that gives less than Octo asks for is not an error to Deezer: it hands over the
    // lesser format and says nothing.
    let asks = |quality: &str, format: &str| DeezerSettings::formats(quality)[0] == format;
    let mut flac_missed = false;
    if active.lossless == Some(false) {
        if asks(&settings.quality, DEEZER_FORMAT_FLAC) {
            flac_missed = true;
            notes.push(
                "Songs are kept as FLAC, but this plan gives no FLAC: they arrive as MP3, and Better quality finds nothing on Deezer.".to_string(),
            );
        }
        if streams && asks(&settings.stream_quality, DEEZER_FORMAT_FLAC) {
            flac_missed = true;
            notes.push("Outside songs are set to play as FLAC, but this plan gives no FLAC: they play as MP3 under a FLAC name, which some apps refuse. Set the stream quality to MP3 320.".to_string());
        }
    }
    if active.high_quality == Some(false) && active.lossless != Some(true) {
        let keeps =
            asks(&settings.quality, DEEZER_FORMAT_MP3_320) || asks(&settings.quality, DEEZER_FORMAT_FLAC);
        let plays = streams && asks(&settings.stream_quality, DEEZER_FORMAT_MP3_320);
        if keeps || plays {
            notes.push("This plan gives 128 kbps MP3 only, so that is what Deezer hands over whatever quality is asked for.".to_string());
        }
    }
    if flac_missed && !detail.contains(';') {
        detail = format!("{detail}; no FLAC on this plan");
    }
    DeezerAccountReport {
        ok: true,
        warning: !notes.is_empty(),
        detail,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::deezer::deezer_resolver::ACCOUNT_UNREACHABLE;

    fn signed_in(
        offer: Option<&str>,
        lossless: Option<bool>,
        high_quality: Option<bool>,
    ) -> DeezerAccountHealth {
        DeezerAccountHealth {
            signed_in: true,
            offer: offer.map(str::to_string),
            lossless,
            high_quality,
            ..Default::default()
        }
    }

    fn failed(problem: &'static str) -> DeezerAccountHealth {
        DeezerAccountHealth {
            problem: Some(problem),
            ..Default::default()
        }
    }

    #[test]
    fn an_account_on_a_lossless_plan_is_simply_signed_in() {
        let report = report(
            &[signed_in(Some("Deezer Premium"), Some(true), Some(true))],
            &DeezerSettings::default(),
            true,
        );
        assert!(report.ok && !report.warning);
        assert_eq!(report.detail, "signed in (Deezer Premium)");
        assert!(report.notes.is_empty());
        // An answer that does not say what the plan gives is not guessed at.
        let unknown = super::report(&[signed_in(None, None, None)], &DeezerSettings::default(), true);
        assert_eq!((unknown.detail.as_str(), unknown.warning), ("signed in", false));
    }

    #[test]
    fn a_refused_arl_says_how_to_replace_it_and_an_unanswered_check_does_not_blame_it() {
        let refused = report(&[failed(ACCOUNT_REFUSED)], &DeezerSettings::default(), false);
        assert!(!refused.ok);
        assert_eq!(refused.detail, "ARL refused or expired");
        assert!(
            refused.notes[0].contains("copy the new arl cookie"),
            "{:?}",
            refused.notes
        );

        let silent = report(&[failed(ACCOUNT_UNREACHABLE)], &DeezerSettings::default(), false);
        assert!(!silent.ok);
        assert_eq!(
            silent.detail,
            "Deezer did not answer, so the ARL could not be checked"
        );
        assert!(!silent.notes[0].contains("expired"), "{:?}", silent.notes);
    }

    #[test]
    fn a_failing_account_beside_a_working_one_is_a_warning() {
        let on_fallback = report(
            &[failed(ACCOUNT_REFUSED), signed_in(None, None, None)],
            &DeezerSettings::default(),
            false,
        );
        assert!(on_fallback.ok && on_fallback.warning);
        assert_eq!(
            on_fallback.detail,
            "the ARL is refused; the fallback ARL signs in"
        );

        let fallback_out = report(
            &[
                signed_in(Some("Deezer Family"), Some(true), Some(true)),
                failed(ACCOUNT_UNREACHABLE),
            ],
            &DeezerSettings::default(),
            false,
        );
        assert!(fallback_out.ok && fallback_out.warning);
        assert_eq!(
            fallback_out.detail,
            "signed in (Deezer Family); Deezer did not answer for the fallback ARL"
        );
    }

    #[test]
    fn a_plan_that_gives_less_than_is_asked_for_is_said() {
        // FLAC kept (the default), on a plan without it.
        let free = signed_in(Some("Deezer Free"), Some(false), Some(false));
        let kept = report(std::slice::from_ref(&free), &DeezerSettings::default(), false);
        assert!(kept.ok && kept.warning);
        assert_eq!(kept.detail, "signed in (Deezer Free); no FLAC on this plan");
        assert_eq!(kept.notes.len(), 2, "{:?}", kept.notes);
        assert!(kept.notes[0].contains("Better quality finds nothing"));
        assert!(kept.notes[1].contains("128 kbps"));

        // Playing as FLAC matters only while outside songs play from Deezer.
        let flac_stream = DeezerSettings {
            quality: "MP3_128".into(),
            stream_quality: "FLAC".into(),
            ..Default::default()
        };
        let premium = signed_in(Some("Deezer Premium"), Some(false), Some(true));
        assert!(
            report(std::slice::from_ref(&premium), &flac_stream, false)
                .notes
                .is_empty()
        );
        let playing = report(std::slice::from_ref(&premium), &flac_stream, true);
        assert_eq!(playing.notes.len(), 1);
        assert!(playing.notes[0].contains("Set the stream quality to MP3 320"));

        // Asking only for what the plan gives is no warning.
        let modest = DeezerSettings {
            quality: "MP3_320".into(),
            ..Default::default()
        };
        let fine = report(std::slice::from_ref(&premium), &modest, true);
        assert!(!fine.warning, "{:?}", fine.notes);
    }
}
