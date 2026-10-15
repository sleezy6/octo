//! `octoAnimatedArtwork` version 1: Apple Music's animated album covers for the Octo apps, from
//! the owner's own server of m8tec/apple-music-animated-artworks (`Metadata:AnimatedArtworkUrl`).
//!
//! - `getAnimatedArtwork?id=<album id>` (a library album or an outside one), or
//!   `?artist=&album=`: `animatedArtwork` with `url` (square) and `urlTall` (tall), HLS
//!   playlists relayed through this server, relative to the server address as `rest/` is (see
//!   [`super::animated_relay`]); no `animatedArtwork` when the album has none. Apple's own
//!   addresses never leave Octo, so an app that plays these never contacts Apple.
//! - `getAlbum` in JSON carries the same `animatedArtwork` on the album when it is known, or
//!   comes back within a moment; otherwise the next getAlbum has it.
//!
//! The extension is listed, and the field written, only while a server is set. Other Subsonic
//! apps never see a difference: they ignore a field they do not know.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use octo_core::common::dotnet::is_blank;
use serde_json::Value;

use super::animated_relay::stream_path;
use super::caller_checks::{SubsonicCall, as_json, refuse_unless_signed_in};
use super::family::octo_reply;
use crate::app::AppState;
use crate::http::error::AppResult;
use crate::http::routes::RouteSet;
use crate::services::cover_art::{AnimatedCover, AnimatedLookup};
use crate::services::i_music_metadata_service::IMusicMetadataService;

/// The extension's name and version, as `getOpenSubsonicExtensions` lists it.
pub const EXTENSION: (&str, i32) = ("octoAnimatedArtwork", 1);

/// How long getAlbum waits for an album the server has not been asked about yet.
pub const ALBUM_WAIT: Duration = Duration::from_millis(1500);

/// How long getAnimatedArtwork waits: the app asked for exactly this.
pub const ASK_WAIT: Duration = Duration::from_secs(12);

/// The longest getAlbum answer read to add the field; anything longer goes out as it came.
const MAX_ALBUM_BODY: usize = 8 * 1024 * 1024;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic(
            "getAnimatedArtwork",
            get(get_animated_artwork).post(get_animated_artwork),
        )
        .merge(super::animated_relay::routes())
}

/// `animatedArtwork` as the apps read it: the square (`url`) and tall (`urlTall`) playlists
/// relayed through this server, relative to the server address as `rest/` is. Apple's own
/// addresses never leave Octo. `None` when the cover has no relay key (it is not known).
fn as_value(state: &AppState, artist: &str, album: &str, cover: &AnimatedCover) -> Option<Value> {
    let relay = state.animated_artwork.relay_key(artist, album)?;
    let mut map = serde_json::Map::new();
    if cover.url.is_some() {
        map.insert("url".into(), Value::String(stream_path(&relay, false)));
    }
    if cover.url_tall.is_some() {
        map.insert("urlTall".into(), Value::String(stream_path(&relay, true)));
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

pub async fn get_animated_artwork(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = match call.format().to_ascii_lowercase().as_str() {
        "json" | "jsonp" => "json".to_string(),
        _ => "xml".to_string(),
    };
    let builder = &state.subsonic_response_builder;
    if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
        return Ok(refused);
    }
    if !state.animated_artwork.enabled() {
        return Ok(builder
            .create_error(&format, 0, "Animated artwork is not set up on this server")
            .into_response());
    }

    let id = call.param_opt("id").unwrap_or("").trim().to_string();
    let (artist, album) = if is_blank(&id) {
        (
            call.param_opt("artist").unwrap_or("").trim().to_string(),
            call.param_opt("album").unwrap_or("").trim().to_string(),
        )
    } else {
        match album_names(&state, &call, &id).await {
            Some(names) => names,
            None => {
                return Ok(builder
                    .create_error(&format, 70, "Album not found")
                    .into_response());
            }
        }
    };
    if is_blank(&artist) || is_blank(&album) {
        return Ok(builder
            .create_error(&format, 10, "Missing id, or artist and album")
            .into_response());
    }

    match state
        .animated_artwork
        .find_within(&artist, &album, ASK_WAIT)
        .await
    {
        AnimatedLookup::Found(cover) => match as_value(&state, &artist, &album, &cover) {
            Some(value) => Ok(octo_reply(&state, &format, Some(("animatedArtwork", &value)))),
            None => Ok(octo_reply(&state, &format, None)),
        },
        AnimatedLookup::None => Ok(octo_reply(&state, &format, None)),
        AnimatedLookup::Unavailable => Ok(builder
            .create_error(
                &format,
                0,
                "The animated artwork server did not answer; try again later",
            )
            .into_response()),
    }
}

/// The artist and title of an album id: an outside album from the catalog, a library album
/// from Navidrome as the caller.
async fn album_names(state: &AppState, call: &SubsonicCall, id: &str) -> Option<(String, String)> {
    let (outside, provider, external_id) = state.local_library.parse_song_id(id);
    if outside {
        let album = state
            .music_metadata
            .get_album(
                provider.as_deref().unwrap_or_default(),
                external_id.as_deref().unwrap_or_default(),
            )
            .await?;
        return Some((album.artist, album.title));
    }
    let mut parameters = as_json(&call.parameters);
    parameters.insert("id".into(), id.to_string());
    let relay = call.proxy.relay_safe("rest/getAlbum", &parameters).await?;
    let document: Value = serde_json::from_slice(&relay.body).ok()?;
    let album = document.get("subsonic-response")?.get("album")?;
    let text = |key: &str| album.get(key).and_then(Value::as_str).map(str::to_string);
    Some((text("artist")?, text("name")?))
}

/// A getAlbum answer with `animatedArtwork` on its album, when the server is set, the answer is
/// JSON, and the album has an animated cover that is known or comes back within
/// [`ALBUM_WAIT`]. Anything else goes out as it came.
pub async fn with_album_artwork(state: &AppState, response: Response) -> Response {
    if !state.animated_artwork.enabled() || !response.status().is_success() {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    if !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_ALBUM_BODY).await else {
        // Too long to read whole: the body is gone, so say so rather than send half.
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let unchanged = |parts: axum::http::response::Parts, bytes: bytes::Bytes| {
        Response::from_parts(parts, Body::from(bytes))
    };
    let Ok(mut document) = serde_json::from_slice::<Value>(&bytes) else {
        return unchanged(parts, bytes);
    };
    let names = document
        .get("subsonic-response")
        .and_then(|envelope| envelope.get("album"))
        .and_then(|album| {
            let text = |key: &str| album.get(key).and_then(Value::as_str).map(str::to_string);
            Some((text("artist")?, text("name")?))
        });
    let Some((artist, album_name)) = names else {
        return unchanged(parts, bytes);
    };
    let AnimatedLookup::Found(cover) = state
        .animated_artwork
        .find_within(&artist, &album_name, ALBUM_WAIT)
        .await
    else {
        return unchanged(parts, bytes);
    };
    let Some(album) = document
        .get_mut("subsonic-response")
        .and_then(|envelope| envelope.get_mut("album"))
        .and_then(Value::as_object_mut)
    else {
        return unchanged(parts, bytes);
    };
    let Some(value) = as_value(state, &artist, &album_name, &cover) else {
        return unchanged(parts, bytes);
    };
    album.insert("animatedArtwork".into(), value);
    let body = serde_json::to_vec(&document).unwrap_or_else(|_| bytes.to_vec());
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(body))
}
