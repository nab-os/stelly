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
use stelly_core::api::{
    BlockedArtist, Corpus, CrawlStatus, Device, GenerateRequest, OrderRequest, PairingGrant,
    PipelineStatus, Recipe, Scope, SpaceIndex, SpaceInfo, SpaceSearch, SpaceSort, Stage, Step,
    Target, TrackMeta, TracksRequest, User, Imported,
};
use stelly_core::session::{Command, Session, Update};
use stelly_core::qobuz::{RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::compression::CompressionLayer;

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
        .route("/api/artists/{id}", get(artist))
        .route("/api/artists/{id}/albums", get(artist_albums))
        .route("/api/artists/{id}/similar", get(similar_artists))
        // --------------------------------------------------------- playback
        .route("/api/tracks/{id}/url", get(file_url))
        .route("/api/session", get(session).post(session_command))
        .route("/api/session/events", get(session_events))
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
        .route("/api/favourites/import", post(import_favourites))
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
        .route("/api/pipeline/add", post(pipeline_add))
        .route("/api/pipeline/stop", post(pipeline_stop))
        .route("/api/pipeline/log", get(pipeline_log))
        // ------------------------------------------------------------ space
        .route("/api/space", get(space_info))
        // zstd, for whoever asks: the index and the map are the only big
        // bodies here, the map's labels most of all.
        .route("/api/space/index", get(space_index).layer(CompressionLayer::new()))
        .route("/api/space/points", get(space_points).layer(CompressionLayer::new()))
        .route("/api/space/points/meta", get(space_points_meta).layer(CompressionLayer::new()))
        .route("/api/space/tracks", post(space_tracks))
        .route("/api/space/search", get(space_search).layer(CompressionLayer::new()))
        .route("/api/space/generate", post(space_generate))
        .route("/api/space/order", post(space_order))
        // ---------------------------------------------------------- devices
        .route("/api/devices", get(devices).post(pair_device))
        .route("/api/devices/{id}", delete(revoke_device))
        .route("/api/me", get(me))
        .route("/api/users", get(users).post(add_user))
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
    stelly_core::qobuz::FORMAT_MP3_320
}

// ----------------------------------------------------------------- browsing

async fn search(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<SearchQuery>,
) -> Reply<SearchResults> {
    Ok(Json(state.hub.search(&query.q, query.limit).await?))
}

/// The asking device's user's likes, nobody else's.
async fn favourite_tracks(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteTrack>> {
    Ok(Json(state.hub.favourite_tracks(device.user_id, query.cap).await?))
}

async fn favourite_albums(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteAlbum>> {
    Ok(Json(state.hub.favourite_albums(device.user_id, query.cap).await?))
}

async fn favourite_artists(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Query(query): Query<CapQuery>,
) -> Reply<Vec<RemoteArtist>> {
    Ok(Json(state.hub.favourite_artists(device.user_id, query.cap).await?))
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

async fn artist(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
) -> Reply<RemoteArtist> {
    Ok(Json(state.hub.artist(id).await?))
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

// ------------------------------------------------------------------ session

async fn session(State(state): State<AppState>, PlayAuth(device): PlayAuth) -> Reply<Session> {
    Ok(Json(state.playback.of(device.user_id).snapshot()))
}

async fn session_command(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Json(command): Json<Command>,
) -> Reply<serde_json::Value> {
    state
        .playback
        .of(device.user_id)
        .command(&device, command)
        .map_err(|_| Failure::conflict("the session changed before this reached it"))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Push the session to one device on every change.
///
/// Opening this is also what makes a device show up in the output picker, so
/// it is held open for as long as the app runs. The queue is left out of an
/// update when it has not changed since the last one on this stream, which is
/// most of them: a position report every few seconds should not resend
/// hundreds of tracks to every device.
async fn session_events(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
) -> impl IntoResponse {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    let (mut published, listening) = state.playback.of(device.user_id).connect(&device);

    tokio::spawn(async move {
        let _listening = listening;
        let mut sent_queue: Option<u64> = None;
        loop {
            tokio::select! {
                changed = published.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                // Without this a closed stream would only be noticed on the
                // next change, and the device would stay listed until then.
                _ = sender.closed() => return,
            }

            let mut session = published.borrow_and_update().clone();
            let same_queue = sent_queue == Some(session.queue_version);
            if same_queue {
                session.queue = Vec::new();
            }
            sent_queue = Some(session.queue_version);

            let update = Update {
                you: device.id,
                same_queue,
                session,
            };
            let Ok(event) = Event::default().json_data(&update) else {
                return;
            };
            if sender.send(Ok(event)).await.is_err() {
                return;
            }
        }
    });

    Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::default())
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
    PlayAuth(device): PlayAuth,
    Json(body): Json<ExportBody>,
) -> Reply<Created> {
    Ok(Json(Created {
        id: state
            .hub
            .export_playlist(&device.user, &body.name, &body.track_ids)
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

/// Hiding is `pipeline`: it only filters and is reversible, but it is the
/// whole family's space it filters, so it is for whoever looks after the
/// server. `block --purge`, which deletes, is not exposed over HTTP at all.
async fn block(
    State(state): State<AppState>,
    _: PipelineAuth,
    Json(body): Json<BlockBody>,
) -> Reply<serde_json::Value> {
    state.hub.block_artist(body.artist_id, &body.name).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn unblock(
    State(state): State<AppState>,
    _: PipelineAuth,
    Path(id): Path<i64>,
) -> Reply<serde_json::Value> {
    state.hub.unblock_artist(id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Liking is `play`: it is the asking user's own list, kept here, and never
/// reaches the Qobuz account.
async fn favorite_add(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Path((kind, id)): Path<(String, String)>,
) -> Reply<serde_json::Value> {
    state.hub.favorite_add(&kind, &id, device.user_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn favorite_remove(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Path((kind, id)): Path<(String, String)>,
) -> Reply<serde_json::Value> {
    state.hub.favorite_remove(&kind, &id, device.user_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Copy the Qobuz account's favourites into the asking user's likes.
async fn import_favourites(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
) -> Reply<Imported> {
    Ok(Json(state.hub.import_favourites(device.user_id).await?))
}

// ------------------------------------------------------------------- counts

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
    stelly_core::api::DEFAULT_MAX_DISTANCE
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

/// `play`, though it runs stages: it is bounded, one track, album or
/// discography, and it is what the detail pages offer on every device.
async fn pipeline_add(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(target): Json<Target>,
) -> Reply<serde_json::Value> {
    state.hub.pipeline_add(target).await?;
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

// -------------------------------------------------------------------- space

fn not_built() -> Failure {
    Failure::unavailable("no space has been built yet, run build-space and layout")
}

/// Run something against the loaded space off the async threads: a radio
/// walk scans the corpus once per step.
async fn on_space<T: Send + 'static>(
    state: &AppState,
    work: impl FnOnce(&crate::space::SpaceService) -> Option<T> + Send + 'static,
) -> Result<T, Failure> {
    let space = state.hub.space().clone();
    tokio::task::spawn_blocking(move || work(&space))
        .await
        .map_err(|err| Failure::from(anyhow::anyhow!("the worker thread panicked: {err}")))?
        .ok_or_else(not_built)
}

async fn space_info(State(state): State<AppState>, _: PlayAuth) -> Reply<SpaceInfo> {
    let tower = state.hub.can_steer().await.unwrap_or(false);
    Ok(Json(on_space(&state, move |space| Some(space.info(tower))).await?))
}

async fn space_index(State(state): State<AppState>, _: PlayAuth) -> Reply<SpaceIndex> {
    let derived = state.hub.space().derived().ok_or_else(not_built)?;
    Ok(Json(derived.index.clone()))
}

async fn space_points(State(state): State<AppState>, _: PlayAuth) -> Result<impl IntoResponse, Failure> {
    let derived = state.hub.space().derived().ok_or_else(not_built)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        derived.points.clone(),
    ))
}

async fn space_points_meta(State(state): State<AppState>, _: PlayAuth) -> Result<impl IntoResponse, Failure> {
    let derived = state.hub.space().derived().ok_or_else(not_built)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        derived.meta.clone(),
    ))
}

async fn space_tracks(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<TracksRequest>,
) -> Reply<Vec<TrackMeta>> {
    Ok(Json(on_space(&state, move |space| space.tracks(&body.ids)).await?))
}

#[derive(Deserialize)]
struct SpaceSearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    sort: SpaceSort,
    #[serde(default)]
    desc: bool,
    #[serde(default = "default_space_limit")]
    limit: usize,
}

fn default_space_limit() -> usize {
    720
}

async fn space_search(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<SpaceSearchQuery>,
) -> Reply<SpaceSearch> {
    Ok(Json(
        on_space(&state, move |space| space.search(&query.q, query.sort, query.desc, query.limit)).await?,
    ))
}

async fn space_generate(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<GenerateRequest>,
) -> Reply<Vec<Step>> {
    // Embedded here rather than by the client, which saves a phone one round
    // trip and 512 floats each way.
    let embedding = match &body.recipe {
        Recipe::Drift { phrase, .. } | Recipe::Mood { phrase } => Some(state.hub.embed(phrase).await?),
        _ => None,
    };
    Ok(Json(
        on_space(&state, move |space| space.generate(&body, embedding.as_deref())).await?,
    ))
}

async fn space_order(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<OrderRequest>,
) -> Reply<Vec<i64>> {
    Ok(Json(on_space(&state, move |space| space.order(&body)).await?))
}

// ------------------------------------------------------------------ devices

/// The asking device, with whose it is and what it may do, so a client can
/// say so and leave out what it would be refused.
async fn me(PlayAuth(device): PlayAuth) -> Json<Device> {
    Json(device)
}

/// `pipeline` sees the whole family's devices, anyone else their own.
fn owned_by(device: &Device) -> Option<i64> {
    match device.scope {
        Scope::Pipeline => None,
        Scope::Play => Some(device.user_id),
    }
}

async fn devices(State(state): State<AppState>, PlayAuth(device): PlayAuth) -> Reply<Vec<Device>> {
    Ok(Json(state.auth.list(owned_by(&device))?))
}

#[derive(Deserialize)]
struct PairBody {
    name: String,
    scope: Scope,
    /// Whose device it is, by name. The pairing device's own user when
    /// absent.
    #[serde(default)]
    user: Option<String>,
}

async fn pair_device(
    State(state): State<AppState>,
    PipelineAuth(device): PipelineAuth,
    Json(body): Json<PairBody>,
) -> Reply<PairingGrant> {
    let user = state
        .auth
        .user_for_pairing(Some(body.user.as_deref().unwrap_or(&device.user)))?;
    Ok(Json(state.auth.issue(&body.name, body.scope, &user)?))
}

/// `play` may revoke its own user's devices, a lost phone should not need
/// whoever has the pipeline device to hand.
async fn revoke_device(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Path(id): Path<i64>,
) -> Reply<serde_json::Value> {
    state.auth.revoke(id, owned_by(&device))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn users(State(state): State<AppState>, _: PipelineAuth) -> Reply<Vec<User>> {
    Ok(Json(state.auth.users()?))
}

#[derive(Deserialize)]
struct UserBody {
    name: String,
}

/// Someone new in the family, for `pipeline`: the same trust as pairing them
/// a device, which is the next thing to do.
async fn add_user(
    State(state): State<AppState>,
    _: PipelineAuth,
    Json(body): Json<UserBody>,
) -> Reply<User> {
    Ok(Json(state.auth.add_user(&body.name)?))
}

// ------------------------------------------------------------------- health

/// The one unauthenticated route. Says nothing except that something is
/// listening, which is what a health check is for.
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "service": "stelly-server" }))
}
