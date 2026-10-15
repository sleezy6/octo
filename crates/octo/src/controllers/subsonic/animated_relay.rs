//! `/animated-artwork/{key}/{file}`: an album's animated cover relayed through Octo, so an app
//! plays it from its own server and never contacts Apple itself.
//!
//! `{key}` is the random relay key of a cover Octo found (getAnimatedArtwork and getAlbum hand
//! it out as `animatedArtwork`'s `url` / `urlTall`). `{file}` is `square.m3u8` or `tall.m3u8` for the
//! cover's top playlist, or a name Octo wrote into a playlist it relayed: the address of the
//! next playlist or video file, base64url, with its extension. Playlists come back with every
//! address rewritten that way, and relative to the playlist, so the app's player follows them
//! back to Octo, whatever address or path prefix it reaches Octo by. Video files are passed
//! through as they arrive, Range and all.
//!
//! Only `https://mvod.itunes.apple.com/itunes-assets/...` is ever fetched, and only for a key
//! Octo handed out, so the relay is no open proxy. No sign-in is asked for: a player follows
//! playlist links without adding one, and the key is unguessable.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::TryStreamExt;
use reqwest::Url;
use tracing::debug;

use crate::app::AppState;
use crate::http::routes::RouteSet;

/// The path the relay answers under, without its leading slash: what `url` and `urlTall` start
/// with.
pub const RELAY_PREFIX: &str = "animated-artwork";

/// The one host the relay fetches from.
const APPLE_HOST: &str = "mvod.itunes.apple.com";
/// And the path everything it fetches lies under.
const APPLE_PATH: &str = "/itunes-assets/";

/// The longest playlist read to rewrite.
const MAX_PLAYLIST: usize = 4 * 1024 * 1024;

pub fn routes() -> RouteSet {
    RouteSet::new().route_with_head("/animated-artwork/{key}/{file}", get(relay))
}

/// The app-facing address of a cover's square or tall playlist, relative to the server address
/// the app already uses (as `rest/` is).
pub fn stream_path(relay_key: &str, tall: bool) -> String {
    format!(
        "{RELAY_PREFIX}/{relay_key}/{}",
        if tall { "tall.m3u8" } else { "square.m3u8" }
    )
}

/// Whether Octo relays this address: https on Apple's video host, under its asset path. A test
/// names its mock server's origin instead.
pub(crate) fn allowed(url: &Url, test_origin: Option<&str>) -> bool {
    if !url.path().starts_with(APPLE_PATH) {
        return false;
    }
    match test_origin {
        Some(origin) => url.as_str().starts_with(&format!("{origin}/")),
        None => url.scheme() == "https" && url.host_str().is_some_and(|h| h.eq_ignore_ascii_case(APPLE_HOST)),
    }
}

/// The name a relayed playlist gives an address: base64url of it, then the extension of its
/// last path segment (only so the name reads as what it is).
pub(crate) fn file_name(url: &Url) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(url.as_str());
    let extension = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|last| last.rsplit_once('.'))
        .map(|(_, ext)| ext)
        .filter(|ext| !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    format!("{encoded}.{extension}")
}

/// The address a relayed name stands for.
fn address_of(file: &str) -> Option<Url> {
    let encoded = file.split('.').next()?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    Url::parse(std::str::from_utf8(&bytes).ok()?).ok()
}

/// `playlist` with every address in it (the URI lines and every `URI="..."`) resolved against
/// `base` and replaced by its relayed name.
pub(crate) fn rewrite_playlist(playlist: &str, base: &Url) -> String {
    let relayed = |reference: &str| -> String {
        match base.join(reference.trim()) {
            Ok(url) => file_name(&url),
            Err(_) => reference.to_string(),
        }
    };
    let mut out = String::with_capacity(playlist.len() * 2);
    for line in playlist.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push_str(line);
        } else if trimmed.starts_with('#') {
            out.push_str(&rewrite_uri_attributes(line, &relayed));
        } else {
            out.push_str(&relayed(trimmed));
        }
        out.push('\n');
    }
    out
}

/// A tag line with each `URI="..."` value replaced.
fn rewrite_uri_attributes(line: &str, relayed: &dyn Fn(&str) -> String) -> String {
    const ATTRIBUTE: &str = "URI=\"";
    let mut out = String::with_capacity(line.len() * 2);
    let mut rest = line;
    while let Some(at) = rest.find(ATTRIBUTE) {
        let value_start = at + ATTRIBUTE.len();
        let Some(length) = rest[value_start..].find('"') else {
            break;
        };
        out.push_str(&rest[..value_start]);
        out.push_str(&relayed(&rest[value_start..value_start + length]));
        out.push('"');
        rest = &rest[value_start + length + 1..];
    }
    out.push_str(rest);
    out
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn relay(
    State(state): State<AppState>,
    Path((key, file)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(cover) = state.animated_artwork.relayed_cover(&key) else {
        return not_found();
    };
    let target = match file.as_str() {
        "square.m3u8" => cover.url.as_deref().and_then(|u| Url::parse(u).ok()),
        "tall.m3u8" => cover.url_tall.as_deref().and_then(|u| Url::parse(u).ok()),
        other => address_of(other),
    };
    let test_origin = state.animated_artwork.relay_test_origin();
    let Some(target) = target.filter(|url| allowed(url, test_origin)) else {
        return not_found();
    };

    let client = state.animated_artwork.relay_client();
    let mut request = client.get(target.clone());
    if let Some(range) = headers.get(header::RANGE) {
        request = request.header(header::RANGE, range.clone());
    }
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(e) => {
            debug!("Animated cover relay could not reach Apple: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        return status.into_response();
    }

    let is_playlist = target.path().ends_with(".m3u8")
        || upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("mpegurl"));
    if is_playlist {
        let Ok(bytes) = read_capped(upstream, MAX_PLAYLIST).await else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut response = Response::new(Body::from(rewrite_playlist(&text, &target)));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.apple.mpegurl"),
        );
        return response;
    }

    let mut response_headers = HeaderMap::new();
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            response_headers.insert(name, value.clone());
        }
    }
    let body = Body::from_stream(upstream.bytes_stream().map_err(std::io::Error::other));
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

/// The body, refused past `cap` bytes.
async fn read_capped(response: reqwest::Response, cap: usize) -> Result<Vec<u8>, ()> {
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.try_next().await.map_err(|_| ())? {
        if out.len() + chunk.len() > cap {
            return Err(());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
#[path = "animated_relay_tests.rs"]
mod tests;
