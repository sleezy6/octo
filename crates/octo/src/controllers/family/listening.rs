//! The Family page's Listening: a member's own, and everyone's for the people who manage the
//! family.

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;

use super::{Web, ok, refusal};
use crate::app::AppState;
use crate::services::listening::{self, rollups};

fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn page(state: &AppState, days: i64, user: Option<String>, everyone: bool) -> Response {
    let log = state.listening.clone();
    let keep_days = state.settings.current().listening.effective_keep_days();
    let answer = tokio::task::spawn_blocking(move || {
        let mut page = listening::page(&log, days, user.as_deref(), everyone);
        page["everyone"] = json!(everyone);
        page["keepDays"] = json!(keep_days);
        page
    })
    .await;
    match answer {
        Ok(page) => ok(&page),
        Err(_) => refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Listening could not be read. Try again.",
        ),
    }
}

/// `GET /api/family/me/listening?range=7|30|90`: the signed-in member's own listening.
pub async fn my_listening(State(state): State<AppState>, web: Web, RawQuery(query): RawQuery) -> Response {
    let (signed, _) = match web.member(&state).await {
        Ok(found) => found,
        Err(res) => return res,
    };
    let days = rollups::range_of(query_value(query.as_deref(), "range").as_deref());
    page(&state, days, Some(signed.user().to_string()), false).await
}

/// `GET /api/family/listening?range=7|30|90[&user=]`: everyone's listening with the people
/// grid, or one person's, for someone who manages the family.
pub async fn family_listening(
    State(state): State<AppState>,
    web: Web,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err(res) = web.manager(&state).await {
        return res;
    }
    let days = rollups::range_of(query_value(query.as_deref(), "range").as_deref());
    page(&state, days, query_value(query.as_deref(), "user"), true).await
}
