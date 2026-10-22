//! The "Get upcoming songs ahead" card: what is ready, what is being fetched, and clearing the
//! folder. Behind the same guard as the rest of `/api/admin`.

use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::json;
use tracing::info;

use super::request_binding::ok;
use crate::app::AppState;
use crate::http::routes::RouteSet;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/prefetch", get(get_prefetch))
        .route("/api/admin/prefetch/clear", post(clear_prefetch))
}

/// `GET /api/admin/prefetch`: on or off, the songs ready and their size against the cap, the free
/// space on the folder's disk, the songs being fetched and next, the last failures, and why
/// nothing new starts when nothing does.
async fn get_prefetch(State(state): State<AppState>) -> Response {
    ok(&state.prefetch.status().await)
}

/// `POST /api/admin/prefetch/clear`: deletes every song downloaded ahead but one playing now.
async fn clear_prefetch(State(state): State<AppState>) -> Response {
    let cleared = state.prefetch.clear();
    info!("Deleted {cleared} songs downloaded ahead by admin request");
    ok(&json!({ "cleared": cleared }))
}
