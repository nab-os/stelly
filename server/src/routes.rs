//! The API.
//!
//! Thin on purpose: every handler is a scope check and a call into `Hub`.
//! There must never be a second implementation here.
//!
//! Bounded gestures are `play`, unbounded jobs are `pipeline`.

use crate::auth::{AdminAuth, PlayAuth};
use crate::provider::{self, Timings};
use crate::space::Prepared;
use crate::{AppState, Failure};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use stelly_core::api::{
    BlockedArtist, Corpus, CrawlStatus, Device, GenerateRequest, OrderRequest, PairingGrant,
    PipelineStatus, Recipe, Scope, SpaceInfo, SpaceSearch, SpaceSort, Stage, Step,
    Target, TrackMeta, TracksRequest, User, Word,
};
use stelly_core::session::{Command, Session};
use stelly_core::catalogue::{Capabilities, Quality, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack};
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::compression::CompressionLayer;

/// The most tracks one generate request gets: the app's slider stops at 60
/// and its longest list at 500. Radio and path cost a pass over the whole
/// space per track, and every client waits on the engine while one runs.
const GENERATE_CAP: usize = 500;

/// A search, or a page's album, artist or discography, past this is logged
/// with where its time went.
const SLOW_BROWSE: Duration = Duration::from_secs(2);

type Reply<T> = Result<Json<T>, Failure>;

pub fn router(state: AppState) -> Router {
    Router::new()
        // --------------------------------------------------------- browsing
        .route("/api/search", get(search))
        .route("/api/capabilities", get(capabilities))
        .route("/api/providers", get(providers))
        .route("/api/favourites/tracks", get(favourite_tracks))
        .route("/api/favourites/albums", get(favourite_albums))
        .route("/api/favourites/artists", get(favourite_artists))
        .route("/api/releases", get(recent_releases))
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
        .route("/api/space/tracks", post(space_tracks))
        .route("/api/space/search", get(space_search))
        .route("/api/space/generate", post(space_generate))
        .route("/api/space/order", post(space_order))
        .route("/api/space/describe/{id}", get(space_describe))
        .route("/api/tuning", get(tuning).put(set_tuning))
        .route("/api/mix/status", get(mix_status))
        .route("/api/mix/library", get(mix_library))
        .route("/api/mix/tracks", post(mix_tracks))
        .route("/api/mix/tracks/{id}", get(mix_track))
        .route("/api/mix/tracks/{id}/analyse", post(mix_track_analyse))
        .route("/api/mix/state/{id}", get(mix_state))
        .route("/api/mix/briefs", post(mix_briefs))
        .route("/api/mix/queue", get(mix_queue))
        .route("/api/mix/liked", get(mix_liked))
        .route("/api/mix/select", post(mix_select))
        .route("/api/mix/next/{id}", get(mix_next))
        .route("/api/mix/set", post(mix_set))
        .route("/api/mix/find", get(mix_find))
        .route("/api/mix/analyse", get(mix_job).post(mix_analyse))
        .route("/api/mix/analyse/stop", post(mix_stop))
        // ---------------------------------------------------------- devices
        .route("/api/devices", get(devices).post(pair_device))
        .route("/api/devices/{id}", delete(revoke_device))
        .route("/api/me", get(me))
        .route("/api/users", get(users).post(add_user))
        // ----------------------------------------------------------- health
        .route("/api/health", get(health))
        .with_state(state.clone())
        // zstd, for whoever asks, on every route: favourites, search,
        // generate and the session are tens to hundreds of KB of JSON. The
        // default predicate leaves the event streams alone, so they are not
        // buffered, and skips bodies too small to gain anything.
        .layer(CompressionLayer::new())
        // Added after the layer, so outside it: their bodies come compressed
        // already, once per change of the space.
        .merge(
            Router::new()
                .route("/api/space/index", get(space_index))
                .route("/api/space/points", get(space_points))
                .route("/api/space/points/meta", get(space_points_meta))
                .with_state(state),
        )
}

// ------------------------------------------------------------------ queries

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
    /// Comma-separated provider names to ask; all of them when absent.
    #[serde(default)]
    providers: Option<String>,
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
struct DaysQuery {
    #[serde(default = "default_days")]
    days: u32,
}

fn default_days() -> u32 {
    90
}

#[derive(Deserialize)]
struct QualityQuery {
    #[serde(default)]
    quality: Quality,
}

// ----------------------------------------------------------------- browsing

/// Answered with a `Server-Timing` saying where the time went: each
/// provider's wait for its answer, then its items' ids. One slow enough to
/// notice says so in the log as well. Each device searches as a search box
/// of its own: a provider that queues searches drops the device's last one
/// once its next comes, and never another device's.
async fn search(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Query(query): Query<SearchQuery>,
) -> Result<Response, Failure> {
    let timings = Timings::default();
    let (only, session) = (names(query.providers.as_deref()), provider::search_session(device.id));
    timed("a search", &timings, state.hub.search(&query.q, query.limit, &only, &session, &timings)).await
}

/// `answer`, with a `Server-Timing` saying where `timings` says its time
/// went, and the whole of it as `total`. One slow enough to notice says so
/// in the log as well, as `what`: never an id or a name.
async fn timed<T: serde::Serialize>(
    what: &str,
    timings: &Timings,
    answer: impl std::future::Future<Output = anyhow::Result<T>>,
) -> Result<Response, Failure> {
    let started = Instant::now();
    let answer = answer.await;
    let took = timings.header(started.elapsed());
    if started.elapsed() >= SLOW_BROWSE {
        eprintln!("{what} took {:.1}s: {took}", started.elapsed().as_secs_f64());
    }
    Ok(([(header::HeaderName::from_static("server-timing"), took)], Json(answer?)).into_response())
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

/// What the asking user's liked artists have released lately.
async fn recent_releases(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Query(query): Query<DaysQuery>,
) -> Reply<Vec<RemoteAlbum>> {
    Ok(Json(state.hub.recent_releases(device.user_id, query.days.clamp(1, 365)).await?))
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

// An album's, an artist's, their discography and those like them: each
// answered with a `Server-Timing` as a search is, each provider's wait and
// its items' ids, or `cache` alone when what was held answered.

async fn album_tracks(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<String>,
) -> Result<Response, Failure> {
    let timings = Timings::default();
    timed("an album", &timings, state.hub.album_tracks(&id, &timings)).await
}

async fn artist(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
) -> Result<Response, Failure> {
    let timings = Timings::default();
    timed("an artist", &timings, state.hub.artist(id, &timings)).await
}

/// Up to `cap`, newest first. A shorter ask is not always the start of a
/// longer one: see `Provider::releases`.
async fn artist_albums(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<CapQuery>,
) -> Result<Response, Failure> {
    let timings = Timings::default();
    timed("a discography", &timings, state.hub.artist_albums(id, query.cap, &timings)).await
}

async fn similar_artists(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Failure> {
    let timings = Timings::default();
    timed("an artist's similar artists", &timings, state.hub.similar_artists(id, query.limit, &timings)).await
}

// ----------------------------------------------------------------- playback

#[derive(serde::Serialize)]
struct StreamUrl {
    url: String,
}

/// Where the provider says a track's audio is. The client streams it from
/// there; audio never proxies through here. A provider whose URLs only this
/// machine can reach is the case that would have to start proxying.
async fn file_url(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<QualityQuery>,
) -> Reply<StreamUrl> {
    Ok(Json(StreamUrl {
        url: state.hub.file_url(id, query.quality).await?,
    }))
}

fn names(list: Option<&str>) -> Vec<String> {
    list.unwrap_or_default().split(',').map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).collect()
}

/// Each provider, most preferred first, so the app can search one of them.
async fn providers(State(state): State<AppState>, _: PlayAuth) -> Reply<Vec<Capabilities>> {
    Ok(Json(state.hub.providers().await?))
}

/// What the music provider is and can do, so the app offers only that.
async fn capabilities(State(state): State<AppState>, _: PlayAuth) -> Reply<Capabilities> {
    Ok(Json(state.hub.capabilities().await?))
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

/// An event stream, marked so that a reverse proxy passes each event on as it
/// is written. nginx otherwise holds a response back until its buffer fills,
/// and a session update, which the app waits on before it can play, sat there
/// until whatever came next pushed it through.
fn unbuffered<S>(events: Sse<S>) -> impl IntoResponse
where
    Sse<S>: IntoResponse,
{
    ([("x-accel-buffering", "no")], events)
}

/// Push the session to one device on every change.
///
/// Opening this is also what makes a device show up in the output picker, so
/// it is held open for as long as the app runs. The queue is left out of an
/// update when it has not changed since the last one on this stream, which is
/// most of them: a position report every few seconds should not resend
/// hundreds of tracks to every device. When it has, an app that asks for
/// splices gets only what changed.
async fn session_events(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Query(query): Query<EventsQuery>,
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

            let session = published.borrow_and_update().clone();
            let update = session.update(device.id, sent_queue, query.splices);
            sent_queue = Some(session.queue_version());
            if sender.send(Ok(Event::default().data(update))).await.is_err() {
                return;
            }
        }
    });

    unbuffered(Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Whether the app applies `Update::splice`. Apps from before it do not
    /// ask, and get the whole queue on every change as they always did.
    #[serde(default)]
    splices: bool,
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
        embedding: state.hub.embed(&body.phrase).await?.to_vec(),
    }))
}

async fn embed_available(State(state): State<AppState>, _: PlayAuth) -> Reply<Steering> {
    Ok(Json(Steering {
        available: state.hub.can_steer(),
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
    _: AdminAuth,
    Json(body): Json<BlockBody>,
) -> Reply<serde_json::Value> {
    state.hub.block_artist(body.artist_id, &body.name).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn unblock(
    State(state): State<AppState>,
    _: AdminAuth,
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

/// The asking user's weights on the space's blocks; nobody else's.
async fn tuning(State(state): State<AppState>, PlayAuth(device): PlayAuth) -> Reply<HashMap<String, f32>> {
    Ok(Json(state.hub.tuning(device.user_id).await?))
}

/// Keep them, the whole set as the client has it. None is back to defaults.
async fn set_tuning(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Json(weights): Json<HashMap<String, f32>>,
) -> Reply<serde_json::Value> {
    state.hub.set_tuning(device.user_id, weights).await?;
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
    _: AdminAuth,
    Json(body): Json<CrawlBody>,
) -> Reply<serde_json::Value> {
    state.hub.crawl_start(body.max_distance).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn crawl_stop(State(state): State<AppState>, _: AdminAuth) -> Reply<serde_json::Value> {
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
    _: AdminAuth,
    Json(body): Json<StageBody>,
) -> Reply<serde_json::Value> {
    state.hub.pipeline_start(body.stage).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn pipeline_start_full(
    State(state): State<AppState>,
    _: AdminAuth,
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

async fn pipeline_stop(State(state): State<AppState>, _: AdminAuth) -> Reply<serde_json::Value> {
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
            // Asleep until a line comes in, or the client goes.
            tokio::select! {
                _ = hub.pipeline_log_past(cursor) => {}
                _ = sender.closed() => return,
            }
        }
    });

    unbuffered(Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::default()))
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
    let tower = state.hub.can_steer();
    Ok(Json(on_space(&state, move |space| Some(space.info(tower))).await?))
}

async fn space_index(State(state): State<AppState>, _: PlayAuth, headers: HeaderMap) -> Result<Response, Failure> {
    let derived = state.hub.space().derived().ok_or_else(not_built)?;
    Ok(prepared(&headers, &derived.index_body))
}

async fn space_points(State(state): State<AppState>, _: PlayAuth, headers: HeaderMap) -> Result<Response, Failure> {
    let derived = state.hub.space().derived().ok_or_else(not_built)?;
    Ok(prepared(&headers, &derived.points))
}

#[derive(Deserialize)]
struct MetaQuery {
    /// `binary` for `map::meta_binary`. A server from before it ignores the
    /// parameter and sends JSON, which the app tells apart by its first byte.
    #[serde(default)]
    format: Option<String>,
}

async fn space_points_meta(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<MetaQuery>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    let derived = state.hub.space().derived().ok_or_else(not_built)?;
    let body = match query.format.as_deref() {
        Some("binary") => &derived.meta_binary,
        _ => &derived.meta,
    };
    Ok(prepared(&headers, body))
}

/// One of the space's cached bodies as this client can take it: 304 when it
/// holds this one already, the compressed copy when it accepts zstd, the raw
/// one otherwise. Shared, never copied.
fn prepared(headers: &HeaderMap, body: &Prepared) -> Response {
    let tag = (header::ETAG, body.tag.clone());
    if holds(headers, &body.tag) {
        return (StatusCode::NOT_MODIFIED, [tag]).into_response();
    }
    let vary = (header::VARY, "accept-encoding".to_string());
    let content_type = (header::CONTENT_TYPE, body.content_type.to_string());
    if accepts_zstd(headers) {
        let encoding = (header::CONTENT_ENCODING, "zstd".to_string());
        ([tag, vary, content_type, encoding], body.zstd.clone()).into_response()
    } else {
        ([tag, vary, content_type], body.raw.clone()).into_response()
    }
}

/// Whether `If-None-Match` names this tag. Weak tags count: the bodies are
/// the same bytes either way.
fn holds(headers: &HeaderMap, tag: &str) -> bool {
    header_items(headers, header::IF_NONE_MATCH)
        .any(|item| item == "*" || item.trim_start_matches("W/") == tag)
}

/// Whether `Accept-Encoding` takes zstd, and not at q=0.
fn accepts_zstd(headers: &HeaderMap) -> bool {
    header_items(headers, header::ACCEPT_ENCODING).any(|item| {
        let mut parts = item.split(';').map(str::trim);
        let named = parts.next().is_some_and(|coding| coding.eq_ignore_ascii_case("zstd"));
        let refused = parts.any(|param| {
            param
                .strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q == 0.0)
        });
        named && !refused
    })
}

/// The comma-separated items of every instance of a header.
fn header_items(headers: &HeaderMap, name: header::HeaderName) -> impl Iterator<Item = &str> {
    headers
        .get_all(name)
        .into_iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
}

async fn space_tracks(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<TracksRequest>,
) -> Reply<Vec<TrackMeta>> {
    Ok(Json(on_space(&state, move |space| space.tracks(&body.ids)).await?))
}

/// How many words a description runs to.
const DESCRIBED_BY: usize = 12;

/// A track in words: the vocabulary scored against its audio.
async fn space_describe(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
) -> Reply<Vec<Word>> {
    let vocabulary = state.hub.vocabulary().await?;
    Ok(Json(
        on_space(&state, move |space| space.describe(id, &vocabulary, DESCRIBED_BY)).await?,
    ))
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
    Json(mut body): Json<GenerateRequest>,
) -> Reply<Vec<Step>> {
    // Clamped rather than refused, so a client asking for more still gets a
    // list.
    body.count = body.count.min(GENERATE_CAP);
    // Embedded here rather than by the client, which saves a phone one round
    // trip and 512 floats each way.
    let embedding = match &body.recipe {
        Recipe::Drift { phrase, .. } | Recipe::Mood { phrase } => Some(state.hub.embed(phrase).await?),
        _ => None,
    };
    Ok(Json(
        on_space(&state, move |space| space.generate(&body, embedding.as_deref().map(Vec::as_slice))).await?,
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

/// `admin` sees the whole family's devices, anyone else their own.
fn owned_by(device: &Device) -> Option<i64> {
    match device.scope {
        Scope::Admin => None,
        Scope::Play => Some(device.user_id),
    }
}

/// Run something against the devices and users off the async threads: it
/// reads and writes the database, and a write can wait on a stage's lock.
async fn on_auth<T: Send + 'static>(
    state: &AppState,
    work: impl FnOnce(&crate::auth::AuthStore) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, Failure> {
    let auth = state.auth.clone();
    Ok(tokio::task::spawn_blocking(move || work(&auth))
        .await
        .map_err(|err| anyhow::anyhow!("the worker thread panicked: {err}"))??)
}

async fn devices(State(state): State<AppState>, PlayAuth(device): PlayAuth) -> Reply<Vec<Device>> {
    Ok(Json(on_auth(&state, move |auth| auth.list(owned_by(&device))).await?))
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

/// `play` may pair its own user's next device, for `play` only: setting up a
/// new phone should not need whoever has the admin device to hand either.
async fn pair_device(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Json(body): Json<PairBody>,
) -> Reply<PairingGrant> {
    if device.scope == Scope::Play {
        if body.scope != Scope::Play {
            return Err(Failure::forbidden("only an admin device can pair an admin one"));
        }
        if body.user.as_deref().is_some_and(|user| user != device.user) {
            return Err(Failure::forbidden("only an admin device can pair someone else's"));
        }
    }
    let grant = on_auth(&state, move |auth| {
        let user = auth.user_for_pairing(Some(body.user.as_deref().unwrap_or(&device.user)))?;
        auth.issue(&body.name, body.scope, &user)
    })
    .await?;
    Ok(Json(grant))
}

/// `play` may revoke its own user's devices, a lost phone should not need
/// whoever has the admin device to hand.
async fn revoke_device(
    State(state): State<AppState>,
    PlayAuth(device): PlayAuth,
    Path(id): Path<i64>,
) -> Reply<serde_json::Value> {
    on_auth(&state, move |auth| auth.revoke(id, owned_by(&device))).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn users(State(state): State<AppState>, _: AdminAuth) -> Reply<Vec<User>> {
    Ok(Json(on_auth(&state, |auth| auth.users()).await?))
}

#[derive(Deserialize)]
struct UserBody {
    name: String,
}

/// Someone new in the family, for `pipeline`: the same trust as pairing them
/// a device, which is the next thing to do.
async fn add_user(
    State(state): State<AppState>,
    _: AdminAuth,
    Json(body): Json<UserBody>,
) -> Reply<User> {
    Ok(Json(on_auth(&state, move |auth| auth.add_user(&body.name)).await?))
}

// ------------------------------------------------------------------- health

/// The one unauthenticated route. Says nothing except that something is
/// listening, which is what a health check is for.
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "service": "stelly-server" }))
}

// ---------------------------------------------------------------------- mix
//
// Whole tracks analysed for mixing: what stelly-mix's clients ask, in the
// shapes `stelly_mix::api` gives them. Reading is `play`; starting an
// analysis, hours of downloading and CPU, is `admin`.

async fn mix_status(State(state): State<AppState>, _: PlayAuth) -> Reply<stelly_mix::api::Status> {
    let mix = state.hub.mix().mix().await?;
    Ok(Json(stelly_mix::api::Status { tracks: mix.len(), version: crate::mix::VERSION }))
}

async fn mix_library(State(state): State<AppState>, _: PlayAuth) -> Reply<Vec<stelly_mix::api::TrackSummary>> {
    let mix = state.hub.mix().mix().await?;
    Ok(Json(tokio::task::spawn_blocking(move || mix.summaries()).await.map_err(anyhow::Error::from)?))
}

/// Several entries at once, the ones the library holds of those asked: what a
/// client mirroring the library fetches, a few hundred a request.
async fn mix_tracks(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<stelly_mix::api::EntriesRequest>,
) -> Reply<Vec<stelly_mix::api::Entry>> {
    let mix = state.hub.mix().mix().await?;
    let entries = body
        .ids
        .iter()
        .filter_map(|&id| Some(stelly_mix::api::Entry { id, track: mix.track(id)?.clone() }))
        .collect();
    Ok(Json(entries))
}

async fn mix_track(State(state): State<AppState>, _: PlayAuth, Path(id): Path<i64>) -> Reply<stelly_mix::library::Track> {
    let mix = state.hub.mix().mix().await?;
    let track = mix.track(id).cloned().ok_or_else(|| Failure::not_found(format!("track {id} has no mix analysis")))?;
    Ok(Json(track))
}

/// Where a track stands for mixing, and what its analysis found once it has
/// one: the app's, which has no use for the beats themselves.
async fn mix_state(State(state): State<AppState>, _: PlayAuth, Path(id): Path<i64>) -> Reply<stelly_core::api::MixState> {
    Ok(Json(state.hub.mix().state(id).await?))
}

#[derive(Deserialize)]
struct SinceQuery {
    /// Unix seconds; everything when absent.
    #[serde(default)]
    since: i64,
}

/// The family's liked tracks, and their liked albums' tracks, newest first:
/// what stelly-mix opens on.
async fn mix_liked(State(state): State<AppState>, _: PlayAuth, Query(query): Query<SinceQuery>) -> Reply<Vec<crate::mix::LikedTrack>> {
    Ok(Json(state.hub.mix().liked(query.since).await?))
}

/// The mix queue, for the pipeline controls.
async fn mix_queue(State(state): State<AppState>, _: PlayAuth) -> Reply<stelly_core::api::MixQueue> {
    Ok(Json(state.hub.mix().queue().await?))
}

/// A selection to analyse for mixing, written as text, as the app's box and
/// `server-analyse` take it, rather than as stelly-mix's type.
async fn mix_select(
    State(state): State<AppState>,
    _: AdminAuth,
    Json(body): Json<stelly_core::api::MixSelect>,
) -> Reply<stelly_mix::api::JobStatus> {
    let selection: stelly_mix::api::Selection = body.selection.trim().parse().map_err(Failure::unprocessable)?;
    state.hub.mix().start(selection).map_err(|err| Failure::conflict(format!("{err:#}")))?;
    Ok(Json(state.hub.mix().status()))
}

/// The tempo and key of each of a list of tracks that has them.
async fn mix_briefs(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(body): Json<stelly_core::api::TracksRequest>,
) -> Reply<Vec<stelly_core::api::MixBrief>> {
    Ok(Json(state.hub.mix().briefs(&body.ids).await?))
}

/// A track's mix analysis, made now if it has none: ahead of everything
/// queued, and waited on. `play`, like reading: it is one track.
async fn mix_track_analyse(State(state): State<AppState>, _: PlayAuth, Path(id): Path<i64>) -> Reply<stelly_mix::library::Track> {
    use crate::mix::Asked;
    match state.hub.mix().analyse_now(id).await? {
        Asked::Analysed(track) => Ok(Json(track)),
        Asked::Failed(why) => Err(Failure::unprocessable(why)),
        Asked::Unknown => Err(Failure::not_found(format!("track {id} is not in the catalogue"))),
        Asked::Waiting => Err(Failure::unavailable(format!("track {id} is still queued for its mix analysis"))),
    }
}

#[derive(Deserialize)]
struct NextQuery {
    #[serde(default = "default_stretch")]
    stretch: f64,
    #[serde(default = "default_ways")]
    limit: usize,
}

fn default_stretch() -> f64 {
    6.0
}

fn default_ways() -> usize {
    30
}

async fn mix_next(
    State(state): State<AppState>,
    _: PlayAuth,
    Path(id): Path<i64>,
    Query(query): Query<NextQuery>,
) -> Reply<Vec<stelly_mix::api::Way>> {
    let mix = state.hub.mix().mix().await?;
    if mix.track(id).is_none() {
        return Err(Failure::not_found(format!("track {id} has no mix analysis")));
    }
    let ways = tokio::task::spawn_blocking(move || mix.next(id, query.stretch, query.limit))
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(ways))
}

/// A set, its description, if any, embedded by the server's own text tower.
async fn mix_set(
    State(state): State<AppState>,
    _: PlayAuth,
    Json(request): Json<stelly_mix::api::SetRequest>,
) -> Reply<stelly_mix::api::SetPlan> {
    let mix = state.hub.mix().mix().await?;
    let embedded = match request.like.as_deref().filter(|d| !d.trim().is_empty()) {
        Some(description) => {
            let query = state.hub.embed(description).await?;
            Some((query, state.hub.embed(stelly_mix::text::BASELINE).await?))
        }
        None => None,
    };
    let plan = tokio::task::spawn_blocking(move || {
        // Scored against every phrase, so on this side too.
        let like = embedded.map(|(query, baseline)| mix.likeness(&query, &baseline));
        mix.set(&request, like)
    })
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(plan))
}

#[derive(Deserialize)]
struct FindQuery {
    q: String,
    #[serde(default = "default_ways")]
    limit: usize,
}

async fn mix_find(
    State(state): State<AppState>,
    _: PlayAuth,
    Query(query): Query<FindQuery>,
) -> Reply<Vec<stelly_mix::api::Match>> {
    let mix = state.hub.mix().mix().await?;
    let description = state.hub.embed(&query.q).await?;
    let baseline = state.hub.embed(stelly_mix::text::BASELINE).await?;
    // A pass over every phrase of every track mix-analysed.
    let found = tokio::task::spawn_blocking(move || mix.find(&description, &baseline, query.limit))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(found))
}

async fn mix_job(State(state): State<AppState>, _: AdminAuth) -> Reply<stelly_mix::api::JobStatus> {
    Ok(Json(state.hub.mix().status()))
}

async fn mix_analyse(
    State(state): State<AppState>,
    _: AdminAuth,
    Json(selection): Json<stelly_mix::api::Selection>,
) -> Reply<stelly_mix::api::JobStatus> {
    state.hub.mix().start(selection)?;
    Ok(Json(state.hub.mix().status()))
}

async fn mix_stop(State(state): State<AppState>, _: AdminAuth) -> Reply<stelly_mix::api::JobStatus> {
    state.hub.mix().stop();
    Ok(Json(state.hub.mix().status()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, value.parse().unwrap());
        headers
    }

    #[test]
    fn zstd_only_when_accepted() {
        assert!(accepts_zstd(&with(header::ACCEPT_ENCODING, "gzip, zstd")));
        assert!(accepts_zstd(&with(header::ACCEPT_ENCODING, "ZSTD;q=0.5")));
        assert!(!accepts_zstd(&with(header::ACCEPT_ENCODING, "zstd;q=0, gzip")));
        assert!(!accepts_zstd(&with(header::ACCEPT_ENCODING, "gzip, br")));
        assert!(!accepts_zstd(&HeaderMap::new()));
    }

    #[test]
    fn held_by_tag() {
        let tag = stelly_core::api::space_tag("2026-10-20/00ff", "points");
        assert!(holds(&with(header::IF_NONE_MATCH, &tag), &tag));
        assert!(holds(&with(header::IF_NONE_MATCH, &format!("\"other\", W/{tag}")), &tag));
        assert!(holds(&with(header::IF_NONE_MATCH, "*"), &tag));
        let meta = stelly_core::api::space_tag("2026-10-20/00ff", "meta");
        assert!(!holds(&with(header::IF_NONE_MATCH, &meta), &tag));
        assert!(!holds(&HeaderMap::new(), &tag));
    }
}
