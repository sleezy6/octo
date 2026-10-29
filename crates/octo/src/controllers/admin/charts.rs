//! `GET /api/admin/charts`: what the dashboard's Charts card shows: the chart country (set,
//! detected, used), Apple Music's countries, the lists that can be turned on (Best New Songs,
//! Trending Songs, the country's genres), how each list that is on was last fetched, and the
//! other countries read because people chose them. `/api/admin/charts/listeners`: each person's
//! own chart country and pinned countries. Behind the sign-in gate like every admin route.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use octo_core::charts::chart_settings::{
    self, ChartCountry, MAX_PINNED, OVERALL, PinsError, genre_name, parse_country,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::plumbing::{error_json, object_result, ok};
use crate::app::AppState;
use crate::middleware::admin_sign_in_gate::{Caller, caller_of};
use crate::services::admin::{AdminCaller, AdminCallerKind};
use crate::services::charts::ListenerCharts;
use crate::services::common::TopSongsService;

/// The words a caller who may not choose each person's charts is answered with.
pub const SIGN_IN_FOR_CHART_PEOPLE: &str =
    "Sign in with your Navidrome admin account to choose each person's charts.";

pub async fn get_charts(State(state): State<AppState>) -> Response {
    let top_songs = &state.top_songs;
    let lists = state.settings.current().generated_playlists.clone();
    let detected = top_songs.apple().known_country();
    let (used, menu) = top_songs.menu().await;
    let on = lists.chart_ids();

    let mut choices: Vec<Value> = [chart_settings::NEW_SONGS, chart_settings::TRENDING]
        .iter()
        .map(|id| {
            let name = TopSongsService::names(&menu, id)
                .map(|(name, _)| name)
                .unwrap_or_default();
            json!({ "id": id, "name": name, "on": on.iter().any(|c| c == id) })
        })
        .collect();
    choices.extend(menu.iter().filter(|g| g.id != OVERALL).map(|g| {
        json!({
            "id": g.id,
            "name": genre_name(&menu, &g.id).unwrap_or_else(|| g.name.clone()),
            "on": on.contains(&g.id),
        })
    }));

    let fetches = top_songs.fetches();
    let listed: Vec<Value> = state
        .chart_playlists
        .lists_on()
        .iter()
        .map(|chart| {
            let name = TopSongsService::names(&menu, chart).map(|(name, _)| name);
            let in_country = chart_settings::kind(chart) != Some(chart_settings::ChartKind::Genre)
                || menu.iter().any(|g| &g.id == chart);
            // The server's own fetch: read for its country, or for Apple's own pick of it.
            let fetch = fetches
                .iter()
                .filter(|f| &f.chart == chart && (f.asked == used || f.asked.is_none()))
                .max_by_key(|f| f.fetched_utc);
            json!({
                "chart": chart,
                "name": name.unwrap_or_else(|| format!("Genre {chart}")),
                "on": true,
                "inCountry": in_country,
                "source": fetch.map(|f| f.source.clone()),
                "songs": fetch.map(|f| f.songs),
                "fetchedUtc": fetch.map(|f| f.fetched_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                "error": fetch.and_then(|f| f.error.clone()),
                "roomId": fetch.and_then(|f| f.room_id.clone()),
            })
        })
        .collect();

    let listener_countries: Vec<String> = top_songs
        .listeners()
        .countries()
        .into_iter()
        .filter(|c| Some(c) != used.as_ref())
        .collect();

    ok(&json!({
        "country": {
            "setting": lists.chart_country,
            "used": used,
            "detected": detected,
        },
        "countries": top_songs.apple().countries(),
        "popularNow": lists.popular_now,
        "choices": choices,
        "lists": listed,
        "listenerCountries": listener_countries,
    }))
}

/// Whether a caller may see and choose anyone's chart countries: the dashboard, a Navidrome
/// admin, or anyone while the sign-in is off (countries are nobody's secret). A listener's app
/// saves its own with setChartCountry and pinChartCountry.
fn may_choose_anyones(caller: Option<&AdminCaller>) -> bool {
    caller.is_some_and(|c| {
        matches!(
            c.kind,
            AdminCallerKind::Dashboard | AdminCallerKind::NavidromeAdmin | AdminCallerKind::SignInOff
        )
    })
}

/// Everyone whose charts can be chosen, by name: everyone Octo has recorded plays of, and
/// everyone who chose, once each whatever the case. Reading the plays is blocking file work.
async fn chart_people(state: &AppState) -> Vec<String> {
    let listening = state.listening.clone();
    let mut names: Vec<String> =
        tokio::task::spawn_blocking(move || listening.people().into_iter().map(|(_, name)| name).collect())
            .await
            .unwrap_or_default();
    for (user, _) in state.top_songs.listeners().all() {
        if !names.iter().any(|name| name.eq_ignore_ascii_case(&user)) {
            names.push(user);
        }
    }
    names.retain(|name| state.top_songs.listeners().is_known(name));
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

async fn listeners_value(state: &AppState) -> Value {
    let top_songs = &state.top_songs;
    let people: Vec<Value> = chart_people(state)
        .await
        .into_iter()
        .map(|user| {
            let mine = top_songs.listeners().get(&user);
            let unavailable: Vec<&String> = std::iter::once(&mine.country)
                .chain(&mine.pinned)
                .filter(|c| !c.is_empty() && !top_songs.has_country(c))
                .collect();
            json!({
                "user": user,
                "country": if mine.country.is_empty() { "auto" } else { mine.country.as_str() },
                "pinned": mine.pinned,
                "revision": mine.revision,
                "notOnAppleMusic": unavailable,
            })
        })
        .collect();
    json!({ "people": people, "maxPinned": MAX_PINNED })
}

async fn listeners_answer(state: &AppState) -> Response {
    ok(&listeners_value(state).await)
}

/// `GET /api/admin/charts/listeners`: each person's own chart country (`auto` while they follow
/// the server's) and pinned countries.
pub async fn get_listeners(State(state): State<AppState>, caller: Caller) -> Response {
    if !may_choose_anyones(caller_of(&caller)) {
        return error_json(StatusCode::FORBIDDEN, SIGN_IN_FOR_CHART_PEOPLE);
    }
    listeners_answer(&state).await
}

#[derive(Debug, Deserialize)]
struct ListenerChoice {
    #[serde(default)]
    user: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    pinned: Vec<String>,
    /// The revision of the person's row the page was showing; 0 when the page had none.
    #[serde(default)]
    revision: u64,
}

/// `PUT /api/admin/charts/listeners` with `{"user", "country", "pinned", "revision"}`: saves that
/// person's own chart country (`auto` follows the server's) and up to three pinned countries,
/// each one Apple Music has (or one they already chose). Answers as the GET does. If the person's
/// row has changed since `revision`, nothing is saved and the answer is a 409 with the latest.
pub async fn put_listeners(State(state): State<AppState>, caller: Caller, body: Bytes) -> Response {
    if !may_choose_anyones(caller_of(&caller)) {
        return error_json(StatusCode::FORBIDDEN, SIGN_IN_FOR_CHART_PEOPLE);
    }
    let Ok(choice) = serde_json::from_slice::<ListenerChoice>(&body) else {
        return error_json(StatusCode::BAD_REQUEST, "Send the person and their countries.");
    };
    let user = choice.user.trim().to_string();
    if user.is_empty() {
        return error_json(StatusCode::BAD_REQUEST, "Name the person.");
    }
    let top_songs = &state.top_songs;
    if !top_songs.listeners().is_known(&user) {
        return error_json(StatusCode::NOT_FOUND, "Navidrome has no such person.");
    }
    let before = top_songs.listeners().get(&user);
    let asked = choice.country.trim().to_ascii_lowercase();
    let country = if asked.is_empty() || asked == "auto" {
        String::new()
    } else {
        match parse_country(&asked) {
            ChartCountry::Fixed(code) if top_songs.has_country(&code) || code == before.country => code,
            _ => {
                return error_json(
                    StatusCode::BAD_REQUEST,
                    &format!("Apple Music has no charts for {asked}."),
                );
            }
        }
    };
    let home = if country.is_empty() {
        top_songs.country().await
    } else {
        Some(country.clone())
    };
    let pinned = match chart_settings::parse_pins(choice.pinned.iter().map(String::as_str), home.as_deref()) {
        Ok(pinned) => pinned,
        Err(PinsError::TooMany) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("Pin at most {MAX_PINNED} countries."),
            );
        }
        Err(PinsError::NotACountry(part)) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("Apple Music has no charts for {part}."),
            );
        }
    };
    if let Some(missing) = pinned
        .iter()
        .find(|c| !top_songs.has_country(c) && !before.pinned.contains(c))
    {
        return error_json(
            StatusCode::BAD_REQUEST,
            &format!("Apple Music has no charts for {missing}."),
        );
    }
    let saved = ListenerCharts {
        country,
        pinned,
        ..Default::default()
    };
    info!(
        "Charts for {user} set on the dashboard: {}, pinned {}",
        if saved.country.is_empty() {
            "the server's country"
        } else {
            saved.country.as_str()
        },
        if saved.pinned.is_empty() {
            "none".to_string()
        } else {
            saved.pinned.join(", ")
        }
    );
    let refused = top_songs
        .listeners()
        .update_if(&user, choice.revision, |mine| {
            mine.country = saved.country;
            mine.pinned = saved.pinned;
        })
        .is_err();
    if refused {
        let mut latest = listeners_value(&state).await;
        latest["error"] = json!("Changed elsewhere just now; showing the latest.");
        return object_result(StatusCode::CONFLICT, &latest);
    }
    listeners_answer(&state).await
}

#[cfg(test)]
#[path = "charts_tests.rs"]
mod tests;
