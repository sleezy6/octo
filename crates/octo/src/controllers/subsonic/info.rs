//! `getAlbumInfo2`/`getAlbumInfo` (one answer for both, `albumInfo`, as Navidrome gives) and
//! `getArtistInfo2`/`getArtistInfo` (each answers its own element, `artistInfo2`/`artistInfo`).

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};

use super::action_helpers::SubsonicCall;
use crate::app::AppState;

pub async fn get_album_info2(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "");
    let (is_external, provider, external_id) = state.local_library.parse_song_id(id);

    if is_external {
        let album = state
            .metadata_service
            .get_album(
                provider.as_deref().unwrap_or(""),
                external_id.as_deref().unwrap_or(""),
            )
            .await;
        let url = album.and_then(|album| album.cover_art_url).unwrap_or_default();
        return builder
            .create_info_response(
                format,
                "albumInfo",
                &[
                    ("notes", ""),
                    ("smallImageUrl", &url),
                    ("mediumImageUrl", &url),
                    ("largeImageUrl", &url),
                ],
            )
            .into_response();
    }

    // Always the v2 endpoint upstream, even for a v1 request.
    match call
        .proxy
        .relay_safe("rest/getAlbumInfo2", &call.parameters)
        .await
    {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => builder.create_response(format, "albumInfo").into_response(),
    }
}

/// Related artists an outside artist's page lists when the client names no count, and the
/// most it ever lists.
const SIMILAR_ARTISTS: i32 = 20;
const MAX_SIMILAR_ARTISTS: i32 = 50;

/// Which of the artist info pair a client called: each answers its own element and asks
/// Navidrome for its own version.
#[derive(Debug, Clone, Copy)]
enum ArtistInfoVersion {
    V1,
    V2,
}

impl ArtistInfoVersion {
    fn element(self) -> &'static str {
        match self {
            ArtistInfoVersion::V1 => "artistInfo",
            ArtistInfoVersion::V2 => "artistInfo2",
        }
    }

    fn upstream(self) -> &'static str {
        match self {
            ArtistInfoVersion::V1 => "rest/getArtistInfo",
            ArtistInfoVersion::V2 => "rest/getArtistInfo2",
        }
    }
}

pub async fn get_artist_info(State(state): State<AppState>, req: Request) -> Response {
    artist_info(state, req, ArtistInfoVersion::V1).await
}

pub async fn get_artist_info2(State(state): State<AppState>, req: Request) -> Response {
    artist_info(state, req, ArtistInfoVersion::V2).await
}

async fn artist_info(state: AppState, req: Request, version: ArtistInfoVersion) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "");
    let (is_external, provider, external_id) = state.local_library.parse_song_id(id);

    if is_external {
        let provider = provider.as_deref().unwrap_or("");
        let external_id = external_id.as_deref().unwrap_or("");
        let artist = state.metadata_service.get_artist(provider, external_id).await;
        let name = artist.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let url = artist.and_then(|artist| artist.image_url).unwrap_or_default();
        let count = call
            .param_or("count", "")
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|c| *c >= 0)
            .unwrap_or(SIMILAR_ARTISTS)
            .min(MAX_SIMILAR_ARTISTS);
        let (biography, similar) = tokio::join!(
            state.last_fm.artist_biography(&name),
            state
                .metadata_service
                .get_related_artists(provider, external_id, count),
        );
        let biography = biography.unwrap_or_default();
        return builder
            .create_artist_info_response(
                format,
                version.element(),
                &[
                    ("biography", &biography),
                    ("smallImageUrl", &url),
                    ("mediumImageUrl", &url),
                    ("largeImageUrl", &url),
                ],
                &similar,
            )
            .into_response();
    }

    match call.proxy.relay_safe(version.upstream(), &call.parameters).await {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => builder.create_response(format, version.element()).into_response(),
    }
}
