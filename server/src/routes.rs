//! The API.
//!
//! Thin on purpose: every handler is a scope check and a call into `Hub`.
//! There must never be a second implementation here.
//!
//! Bounded gestures are `play`, unbounded jobs are `pipeline`.

use crate::auth::{PipelineAuth, PlayAuth};
use crate::{AppState, Failure};
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use two_khz::api::{
    BlockedArtist, Corpus, CrawlStatus, Device, PairingGrant, PipelineStatus, Scope, Stage,
    SyncFile, SyncManifest,
};
use two_khz::qobuz::{RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;

/// How often the SSE task looks for new output. Matches the client's old poll
/// interval, except now it costs one in-memory read rather than a request.
const LOG_POLL: Duration = Duration::from_millis(200);

type Reply<T> = Result<Json<T>, Failure>;

pub fn router(state: AppState) -> Router {
    Router::new()
        // --------------------------------------------------------- browsing
        .route("/api/search", get(search))
        .route("/api/favourites/tracks", get(favourite_tracks))
        .route("/api/favourites/albums", get(favourite_albums))
        .route("/api/favourites/artists", get(favourite_artists))
        .route("/api/playlists", get(playlists).post(export_playlist))
        .route("/api/playlists/{id}", get(playlist_tracks))
        .route("/api/albums/{id}", get(album_tracks))
        .route("/api/albums/{id}/fetch", post(fetch_album))
        .route("/api/artists/{id}/albums", get(artist_albums))
        .route("/api/artists/{id}/similar", get(similar_artists))
        .route("/api/artists/{id}/fetch", post(fetch_artist))
        // --------------------------------------------------------- playback
        .route("/api/tracks/{id}/url", get(file_url))
        // ---------------------------------------------------- text steering
        .route("/api/embed", post(embed))
        .route("/api/embed/available", get(embed_available))
        // ----------------------------------------------------------- hiding
        .route("/api/blocked", get(blocked).post(block))
        .route("/api/blocked/{id}", delete(unblock))
        // ------------------------------------------------------- favourites
        .route(
            "/api/favourites/{kind}/{id}",
            post(favorite_add).delete(favorite_remove),
        )
        // ----------------------------------------------------------- counts
        .route("/api/corpus", get(corpus))
        // --------------------------------------------------------- crawling
        .route("/api/crawl", get(crawl_status))
        .route("/api/crawl/start", post(crawl_start))
        .route("/api/crawl/stop", post(crawl_stop))
        // --------------------------------------------------------- pipeline
        .route("/api/pipeline", get(pipeline_status))
        .route("/api/pipeline/start", post(pipeline_start))
        .route("/api/pipeline/start-full", post(pipeline_start_full))
        .route("/api/pipeline/stop", post(pipeline_stop))
        .route("/api/pipeline/log", get(pipeline_log))
        // ------------------------------------------------------------- sync
        .route("/api/sync/manifest", get(sync_manifest))
        .route("/api/sync/{name}", get(sync_file))
        // ---------------------------------------------------------- devices
        .route("/api/devices", get(devices).post(pair_device))
        .route("/api/devices/{id}", delete(revoke_device))
        // ----------------------------------------------------------- health
        .route("/api/health", get(health))
        .with_state(state)
}

// ------------------------------------------------------------------ queries

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    50
}

#[derive(Deserialize)]
struct CapQuery {
    #[serde(default = "default_cap")]
    cap: usize,
}

fn default_cap() -> usize {
    500
}

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default = "default_similar")]
    limit: usize,
}

fn default_similar() -> usize {
    20
}

#[derive(Deserialize)]
struct FormatQuery {
    #[serde(default = "default_format")]
    format: u32,
}

fn default_format() -> u32 {
    two_khz::qobuz::FORMAT_MP3_320
}

// ----------------------------------------------------------------- browsing

async fn search(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<SearchQuery>,
) -> Reply<SearchResults> {
    Ok(Json(state.hub.search(&query.q, query.limit).await?))
}

async fn favourite_tracks(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteTrack>> {
    Ok(Json(state.hub.favourite_tracks(query.cap).await?))
}

async fn favourite_albums(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteAlbum>> {
    Ok(Json(state.hub.favourite_albums(query.cap).await?))
}

async fn favourite_artists(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteArtist>> {
    Ok(Json(state.hub.favourite_artists(query.cap).await?))
}

async fn playlists(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemotePlaylist>> {
    Ok(Json(state.hub.playlists(query.cap).await?))
}

async fn playlist_tracks(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteTrack>> {
    Ok(Json(state.hub.playlist_tracks(id, query.cap).await?))
}

async fn album_tracks(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<String>,
) -> Reply<Vec<RemoteTrack>> {
    Ok(Json(state.hub.album_tracks(&id).await?))
}

async fn artist_albums(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteAlbum>> {
    Ok(Json(state.hub.artist_albums(id, query.cap).await?))
}

async fn similar_artists(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<LimitQuery>,
) -> Reply<Vec<RemoteArtist>> {
    Ok(Json(state.hub.similar_artists(id, query.limit).await?))
}

// ----------------------------------------------------------------- playback

#[derive(serde::Serialize)]
struct StreamUrl {
    url: String,
}

/// Mint a signed Qobuz URL. The client streams it from Qobuz's CDN; audio
/// never proxies through here. If their URLs turn out to be IP-bound, this is
/// the route that has to start proxying.
async fn file_url(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<FormatQuery>,
) -> Reply<StreamUrl> {
    Ok(Json(StreamUrl {
        url: state.hub.file_url(id, query.format).await?,
    }))
}

#[derive(Deserialize)]
struct ExportBody {
    name: String,
    track_ids: Vec<i64>,
}

#[derive(serde::Serialize)]
struct Created {
    id: i64,
}

async fn export_playlist(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<ExportBody>,
) -> Reply<Created> {
    Ok(Json(Created {
        id: state
            .hub
            .export_playlist(&body.name, &body.track_ids)
            .await?,
    }))
}

// ------------------------------------------------------------ text steering

#[derive(Deserialize)]
struct EmbedBody {
    phrase: String,
}

#[derive(serde::Serialize)]
struct Embedding {
    embedding: Vec<f32>,
}

#[derive(serde::Serialize)]
struct Steering {
    available: bool,
}

/// The one space operation that crosses the wire. The tower is 479MB and its
/// answer is 512 floats, which is exactly the shape that belongs on a server.
async fn embed(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<EmbedBody>,
) -> Reply<Embedding> {
    Ok(Json(Embedding {
        embedding: state.hub.embed(&body.phrase).await?,
    }))
}

async fn embed_available(State(state): State<AppState>, _: PlayAuth) -> Reply<Steering> {
    Ok(Json(Steering {
        available: state.hub.can_steer().await.unwrap_or(false),
    }))
}

// ------------------------------------------------------------------- hiding

async fn blocked(State(state): State<AppState>, _: PlayAuth) -> Reply<Vec<BlockedArtist>> {
    Ok(Json(state.hub.blocked_artists().await?))
}

#[derive(Deserialize)]
struct BlockBody {
    artist_id: i64,
    name: String,
}

/// Hiding is `play`, not `pipeline`: a listening preference that only filters
/// and is reversible. `block --purge`, which deletes, is not exposed over HTTP
/// at all.
async fn block(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<BlockBody>,
) -> Reply<serde_json::Value> {
    state.hub.block_artist(body.artist_id, &body.name).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn unblock(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
) -> Reply<serde_json::Value> {
    state.hub.unblock_artist(id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Following an artist or liking a track/album is `play`, not `pipeline`: it
/// changes the Qobuz account's own favourites, not this app's catalogue.
async fn favorite_add(
    State(state): State<AppState>,
    _: PlayAuth,
    Path((kind, id)): Path<(String, String)>,
) -> Reply<serde_json::Value> {
    state.hub.favorite_add(&kind, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn favorite_remove(
    State(state): State<AppState>,
    _: PlayAuth,
    Path((kind, id)): Path<(String, String)>,
) -> Reply<serde_json::Value> {
    state.hub.favorite_remove(&kind, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------- extending the catalogue

#[derive(serde::Serialize)]
struct Fetched {
    count: usize,
}

/// Bounded, so `play`: one album is one request, one discography a handful.
async fn fetch_album(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<String>,
) -> Reply<Fetched> {
    Ok(Json(Fetched {
        count: state.hub.fetch_album(&id).await?,
    }))
}

async fn fetch_artist(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
) -> Reply<Fetched> {
    Ok(Json(Fetched {
        count: state.hub.fetch_artist(id).await?,
    }))
}

async fn corpus(State(state): State<AppState>, _: PlayAuth) -> Reply<Corpus> {
    Ok(Json(state.hub.corpus().await?))
}

// ----------------------------------------------------------------- crawling

async fn crawl_status(State(state): State<AppState>, _: PlayAuth) -> Reply<CrawlStatus> {
    Ok(Json(state.hub.crawl_status().await?))
}

#[derive(Deserialize)]
struct CrawlBody {
    #[serde(default = "default_distance")]
    max_distance: i64,
}

fn default_distance() -> i64 {
    two_khz::api::DEFAULT_MAX_DISTANCE
}

/// Unbounded, so `pipeline`: this runs until the frontier empties.
async fn crawl_start(
    State(state): State<AppState>,
    _: PipelineAuth,
    Json(body): Json<CrawlBody>,
) -> Reply<serde_json::Value> {
    state.hub.crawl_start(body.max_distance).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn crawl_stop(State(state): State<AppState>, _: PipelineAuth) -> Reply<serde_json::Value> {
    state.hub.crawl_stop().await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ----------------------------------------------------------------- pipeline

async fn pipeline_status(State(state): State<AppState>, _: PlayAuth) -> Reply<PipelineStatus> {
    Ok(Json(state.hub.pipeline_status().await?))
}

#[derive(Deserialize)]
struct StageBody {
    stage: Stage,
}

async fn pipeline_start(
    State(state): State<AppState>,
    _: PipelineAuth,
    Json(body): Json<StageBody>,
) -> Reply<serde_json::Value> {
    state.hub.pipeline_start(body.stage).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn pipeline_start_full(
    State(state): State<AppState>,
    _: PipelineAuth,
) -> Reply<serde_json::Value> {
    state.hub.pipeline_start_full().await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn pipeline_stop(State(state): State<AppState>, _: PipelineAuth) -> Reply<serde_json::Value> {
    state.hub.pipeline_stop().await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Stream the running stage's output.
///
/// SSE rather than a poll: `layout` can be silent for minutes. Keep-alives go
/// out regardless so a proxy does not time out during that silence, and a
/// connecting client gets the retained buffer first.
async fn pipeline_log(State(state): State<AppState>, _: PlayAuth) -> impl IntoResponse {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(256);
    let hub = state.hub.clone();

    tokio::spawn(async move {
        let mut cursor = 0u64;
        loop {
            let Ok(slice) = hub.pipeline_log_since(cursor).await else {
                return;
            };
            for line in slice.lines {
                if sender.send(Ok(Event::default().data(line))).await.is_err() {
                    // The client went away; nothing else to do.
                    return;
                }
            }
            cursor = slice.cursor;
            tokio::time::sleep(LOG_POLL).await;
        }
    });

    Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::default())
}

// --------------------------------------------------------------------- sync

/// What a client needs to navigate: the vectors, the manifest describing them,
/// and a catalogue. Not `two_khz.db` itself, clients get `catalog.db`, the
/// slim projection built by `two-khz-server sync-catalog`.
const SYNCED: [&str; 4] = ["space.bin", "space.json", "semantic_pca.bin", "catalog.db"];

async fn sync_manifest(State(state): State<AppState>, _: PlayAuth) -> Reply<SyncManifest> {
    let mut files = Vec::new();

    for name in SYNCED {
        let path = state.data_dir.join(name);
        let Ok(bytes) = tokio::fs::read(&path).await else {
            // A corpus without a layout has no space.bin yet; that is a state
            // to report as "nothing to sync", not an error.
            continue;
        };
        files.push(SyncFile {
            name: name.to_string(),
            bytes: bytes.len() as u64,
            digest: two_khz::api::digest(&bytes),
        });
    }

    Ok(Json(SyncManifest {
        generation: state.hub.pipeline_status().await?.generation,
        files,
    }))
}

async fn sync_file(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, Failure> {
    // Allow-list rather than sanitising: the set is fixed and small, so there
    // is no reason to accept a path at all.
    if !SYNCED.contains(&name.as_str()) {
        return Err(Failure::not_found(format!("{name} is not a synced file")));
    }

    let path = state.data_dir.join(&name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|err| Failure::not_found(format!("{}: {err}", path.display())))?;

    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    ))
}

// ------------------------------------------------------------------ devices

async fn devices(State(state): State<AppState>, _: PipelineAuth) -> Reply<Vec<Device>> {
    Ok(Json(state.auth.list()?))
}

#[derive(Deserialize)]
struct PairBody {
    name: String,
    scope: Scope,
}

async fn pair_device(
    State(state): State<AppState>,
    _: PipelineAuth,
    Json(body): Json<PairBody>,
) -> Reply<PairingGrant> {
    Ok(Json(state.auth.issue(&body.name, body.scope)?))
}

async fn revoke_device(
    State(state): State<AppState>,
    _: PipelineAuth,
    Path(id): Path<i64>,
) -> Reply<serde_json::Value> {
    state.auth.revoke(id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ------------------------------------------------------------------- health

/// The one unauthenticated route. Says nothing except that something is
/// listening, which is what a health check is for.
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "service": "two-khz-server" }))
}
