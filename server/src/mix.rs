//! Mix analysis: what stelly-mix makes of a whole track, its beats, bars,
//! phrases and each phrase's CLAP embedding, kept beside the space's coarse
//! analysis of the same track, so one server answers both "what sounds like
//! this" and "how do I mix into it".
//!
//! Unlike `analyse`, which reads a 90s excerpt, this needs the whole track:
//! MP3 320 through the gateway, analysed in memory and dropped. It runs on
//! what is asked, an album, an artist, a track's neighbours in the space or a
//! slice of it, never on the whole catalogue by default: a full track is ten
//! times the download and the CPU of an excerpt.
//!
//! A whole track holds the excerpt too: a track the space has no current
//! analysis of gets one from the same samples, rather than a second download
//! when `analyse` comes to it.

use crate::pipeline::descriptors::Descriptors;
use crate::pipeline::{analyse, audio, models, Job, Paths};
use crate::provider::{Provider, Quality};
use crate::space::SpaceService;
use anyhow::{bail, Context, Result};
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer, Nullable, Text};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;
use stelly_core::logbuffer::LogBuffer;
use stelly_core::api::{GenerateRequest, Recipe, Target};
use stelly_mix::api::{JobStatus, Selection, Stats};
use stelly_mix::library::Track;
use stelly_mix::mix::Mix;
use stelly_mix::source::{Item, Origin};

/// What a stored row was made with; rows with another are redone.
pub const VERSION: u32 = stelly_mix::library::VERSION;
/// Neighbours of each seed, unless asked otherwise.
pub const AROUND: usize = stelly_mix::api::AROUND;
/// The most tracks a playlist brings in.
const PLAYLIST_CAP: usize = 2000;
/// A full track at 320kbps is ~10MB; a slow CDN response gets this long.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// One worker per this many hardware threads, at most `MAX_WORKERS`: each
/// holds an ONNX session and a whole decoded track.
const CORES_PER_WORKER: usize = 2;
const MAX_WORKERS: usize = 6;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// 0 picks from the cores.
    pub workers: usize,
    pub retry_failed: bool,
}

// --------------------------------------------------------------- selecting

/// The catalogued tracks `selection` comes to. `space` is the server's,
/// already loaded; the command line has none and loads its own when asked.
pub async fn select(paths: &Paths, selection: &Selection, job: &Job, space: Option<&Arc<SpaceService>>) -> Result<Vec<i64>> {
    let target = match selection {
        Selection::Track { id } => Some(Target::Track(*id)),
        Selection::Album { id } => Some(Target::Album(id.clone())),
        Selection::Artist { id } => Some(Target::Artist(*id)),
        _ => None,
    };
    match selection {
        Selection::Track { .. } | Selection::Album { .. } | Selection::Artist { .. } => {
            let conn = &mut crate::db::open_for_write(&paths.db_path)?;
            let mut client = Provider::from_env(&paths.env_dir, &paths.db_path).bulk();
            crate::crawl::catalogue_target(conn, &mut client, &target.expect("a target"), job).await
        }
        Selection::Playlist { id } => {
            let mut client = Provider::from_env(&paths.env_dir, &paths.db_path).bulk();
            let tracks = client.playlist_tracks(*id, PLAYLIST_CAP).await?;
            job.log(format!("{} tracks in playlist {id}", tracks.len()));
            catalogue_each(paths, &mut client, tracks.iter().map(|t| Target::Track(t.id)).collect(), job).await
        }
        Selection::Favourites => {
            let likes = {
                let conn = &mut crate::db::open_for_write(&paths.db_path)?;
                crate::likes::family(conn)?
            };
            // Tracks and albums, whole; an artist liked is a whole discography,
            // which is `artist/<id>` to ask for on its own.
            let targets: Vec<Target> = likes
                .iter()
                .filter_map(|liked| match liked {
                    crate::likes::Liked::Track(track) => Some(Target::Track(track.id)),
                    crate::likes::Liked::Album(album) => Some(Target::Album(album.id.clone())),
                    crate::likes::Liked::Artist(_) => None,
                })
                .collect();
            job.log(format!("{} liked tracks and albums", targets.len()));
            let mut client = Provider::from_env(&paths.env_dir, &paths.db_path).bulk();
            catalogue_each(paths, &mut client, targets, job).await
        }
        Selection::Mood { phrase, count } => {
            let encoder = crate::text::TextEncoder::load(&paths.model_dir)?;
            let mut encoder = encoder.context("a mood wants the text tower: stelly-server models")?;
            let embedding = encoder.embed(phrase)?;
            let space = space_for(paths, space)?;
            let request = GenerateRequest {
                recipe: Recipe::Mood { phrase: phrase.clone() },
                count: *count,
                penalty: 0.0,
                weights: Default::default(),
            };
            Ok(space.generate(&request, Some(&embedding)).unwrap_or_default().into_iter().map(|s| s.track.track_id).collect())
        }
        Selection::Around { seed, count } => {
            let space = space_for(paths, space)?;
            Ok(around(&space, &[*seed], *count))
        }
        Selection::ArtistName { name, count } => {
            let space = space_for(paths, space)?;
            let wanted = squash(name);
            let seeds: Vec<i64> = every_track(&space)
                .into_iter()
                .filter(|t| squash(&t.artist) == wanted)
                .map(|t| t.track_id)
                .collect();
            if seeds.is_empty() {
                bail!("no track in the space is credited to {name}");
            }
            job.log(format!("{} of {name}'s tracks in the space", seeds.len()));
            Ok(around(&space, &seeds, *count))
        }
        Selection::Space { genres, limit } => {
            let space = space_for(paths, space)?;
            let genres: Vec<String> = genres.iter().map(|g| g.to_lowercase()).collect();
            Ok(every_track(&space)
                .into_iter()
                .filter(|t| genres.is_empty() || genres.iter().any(|g| t.genre.to_lowercase().contains(g)))
                .map(|t| t.track_id)
                .take(if *limit == 0 { usize::MAX } else { *limit })
                .collect())
        }
    }
}

/// Catalogue each of `targets`, those already known costing nothing, and
/// the tracks they come to, each once; one that fails is told and passed.
async fn catalogue_each(paths: &Paths, client: &mut Provider, targets: Vec<Target>, job: &Job) -> Result<Vec<i64>> {
    let conn = &mut crate::db::open_for_write(&paths.db_path)?;
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for target in targets {
        if job.cancelled() {
            break;
        }
        match crate::crawl::catalogue_target(conn, client, &target, job).await {
            Ok(found) => ids.extend(found.into_iter().filter(|id| seen.insert(*id))),
            Err(err) => job.log(format!("{target}: {err:#}")),
        }
    }
    Ok(ids)
}

/// The server's space as it is, rather than a second copy of the whole
/// engine for one selection; loaded first if it has not been yet.
fn space_for(paths: &Paths, shared: Option<&Arc<SpaceService>>) -> Result<Arc<SpaceService>> {
    let space = match shared {
        Some(space) if space.derived().is_some() => return Ok(space.clone()),
        Some(space) => space.clone(),
        None => Arc::new(SpaceService::new(paths.data_dir.clone(), paths.db_path.clone())),
    };
    space.reload().context("loading the space")?;
    Ok(space)
}

fn every_track(space: &SpaceService) -> Vec<stelly_core::api::TrackMeta> {
    let ids = space.derived().map(|d| d.index.ids.clone()).unwrap_or_default();
    space.tracks(&ids).unwrap_or_default()
}

/// The seeds, then each one's nearest `count` in the space, each once.
fn around(space: &SpaceService, seeds: &[i64], count: usize) -> Vec<i64> {
    let mut seen = HashSet::new();
    let mut ids: Vec<i64> = seeds.iter().copied().filter(|s| seen.insert(*s)).collect();
    for &seed in seeds {
        let request = GenerateRequest {
            recipe: Recipe::Neighbours { seed },
            count: count + 1,
            penalty: 0.0,
            weights: Default::default(),
        };
        for step in space.generate(&request, None).unwrap_or_default() {
            if seen.insert(step.track.track_id) {
                ids.push(step.track.track_id);
            }
        }
    }
    ids
}

/// Lower case letters and digits only, for comparing names.
fn squash(text: &str) -> String {
    text.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

// --------------------------------------------------------------- storage

#[derive(QueryableByName)]
struct Pending {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Nullable<Text>)]
    artist: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    duration: Option<i64>,
    /// No current `features` row: the space wants the excerpt too.
    #[diesel(sql_type = diesel::sql_types::Bool)]
    needs_features: bool,
}

/// Of `ids`, those with no current mix analysis, nor a failure unless
/// `retry_failed`, in the order asked.
fn pending(conn: &mut SqliteConnection, ids: &[i64], retry_failed: bool) -> Result<Vec<Pending>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(500) {
        let list = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let failed = if retry_failed { "" } else { "AND NOT EXISTS (SELECT 1 FROM mix_failures f WHERE f.track_id = t.id)" };
        let query = format!(
            "SELECT {columns}
             FROM tracks t LEFT JOIN artists a ON a.id = t.artist_id
             WHERE t.id IN ({list})
               AND NOT EXISTS (SELECT 1 FROM mix m WHERE m.track_id = t.id AND m.version = {VERSION})
               {failed}",
            columns = pending_columns(),
        );
        out.extend(diesel::sql_query(query).load::<Pending>(conn)?);
    }
    let order: std::collections::HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    out.sort_by_key(|p| order.get(&p.id).copied().unwrap_or(usize::MAX));
    Ok(out)
}

/// What a `Pending` is read with, from `tracks t` and `artists a`.
fn pending_columns() -> String {
    format!(
        "t.id AS id, t.title AS title, a.name AS artist, t.duration AS duration,
         NOT EXISTS (SELECT 1 FROM features f WHERE f.track_id = t.id
                     AND f.extractor_version = '{}') AS needs_features",
        analyse::VERSION
    )
}

/// `id`'s current mix analysis, if it has one.
fn stored(conn: &mut SqliteConnection, id: i64) -> Result<Option<Track>> {
    let row = diesel::sql_query(format!("SELECT track_id, entry_json FROM mix WHERE track_id = ? AND version = {VERSION}"))
        .bind::<BigInt, _>(id)
        .get_result::<Row>(conn)
        .optional()?;
    Ok(row.and_then(|row| serde_json::from_str(&row.entry_json).ok()))
}

fn store(conn: &mut SqliteConnection, id: i64, track: &Track) -> Result<()> {
    diesel::sql_query(
        "INSERT INTO mix (track_id, version, entry_json, analysed_at) VALUES (?, ?, ?, ?)
         ON CONFLICT(track_id) DO UPDATE SET version = excluded.version,
           entry_json = excluded.entry_json, analysed_at = excluded.analysed_at",
    )
    .bind::<BigInt, _>(id)
    .bind::<Integer, _>(VERSION as i32)
    .bind::<Text, _>(serde_json::to_string(track)?)
    .bind::<Text, _>(crate::db::utc_now())
    .execute(conn)?;
    diesel::sql_query("DELETE FROM mix_failures WHERE track_id = ?").bind::<BigInt, _>(id).execute(conn)?;
    Ok(())
}

fn record_failure(conn: &mut SqliteConnection, id: i64, reason: &str) -> Result<()> {
    let reason: String = reason.chars().take(500).collect();
    diesel::sql_query(
        "INSERT INTO mix_failures (track_id, reason, failed_at) VALUES (?, ?, ?)
         ON CONFLICT(track_id) DO UPDATE SET reason = excluded.reason, failed_at = excluded.failed_at",
    )
    .bind::<BigInt, _>(id)
    .bind::<Text, _>(reason)
    .bind::<Text, _>(crate::db::utc_now())
    .execute(conn)?;
    Ok(())
}

#[derive(QueryableByName)]
struct Row {
    #[diesel(sql_type = BigInt)]
    track_id: i64,
    #[diesel(sql_type = Text)]
    entry_json: String,
}

/// Every current mix analysis, in memory, ready to be asked of.
pub fn load(db_path: &std::path::Path) -> Result<Mix> {
    // Through the writer's connection, which brings the schema up to date: the
    // first read of a database from before mixing makes its tables.
    let conn = &mut crate::db::open_for_write(db_path)?;
    let rows: Vec<Row> = diesel::sql_query(format!("SELECT track_id, entry_json FROM mix WHERE version = {VERSION}")).load(conn)?;
    let entries = rows
        .into_iter()
        .filter_map(|row| Some((row.track_id, serde_json::from_str::<Track>(&row.entry_json).ok()?)))
        .collect();
    Ok(Mix::new(entries))
}

// ------------------------------------------------------------------- queue

/// Why a track is in the queue, most urgent first, which is the order it is
/// taken in. A track queued again keeps the more urgent of its reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// Someone is waiting on it.
    Asked = 0,
    Liked = 1,
    /// `server-analyse`, or a selection from the pipeline controls.
    Selection = 2,
    /// Listened through: the library growing by itself, behind the rest.
    Played = 3,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Asked => "asked",
            Reason::Liked => "liked",
            Reason::Selection => "selection",
            Reason::Played => "played",
        }
    }

    fn of(priority: i32) -> Self {
        match priority {
            0 => Reason::Asked,
            1 => Reason::Liked,
            2 => Reason::Selection,
            _ => Reason::Played,
        }
    }
}

/// Queue those of `ids` with no current mix analysis, nor a failed one, for
/// `reason`; answers those, which are in the queue now. Asked for, one that
/// failed is tried again: someone wants it now.
pub fn enqueue(conn: &mut SqliteConnection, ids: &[i64], reason: Reason) -> Result<Vec<i64>> {
    let todo: Vec<i64> = pending(conn, ids, reason == Reason::Asked)?.into_iter().map(|p| p.id).collect();
    let now = crate::db::utc_now();
    conn.transaction(|conn| {
        for &id in &todo {
            diesel::sql_query(
                "INSERT INTO mix_queue (track_id, priority, reason, queued_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT(track_id) DO UPDATE SET priority = excluded.priority, reason = excluded.reason
                 WHERE excluded.priority < mix_queue.priority",
            )
            .bind::<BigInt, _>(id)
            .bind::<Integer, _>(reason as i32)
            .bind::<Text, _>(reason.as_str())
            .bind::<Text, _>(&now)
            .execute(conn)?;
        }
        Ok::<_, diesel::result::Error>(())
    })?;
    Ok(todo)
}

#[derive(QueryableByName)]
struct Queued {
    #[diesel(embed)]
    track: Pending,
    #[diesel(sql_type = Integer)]
    priority: i32,
}

/// The next `n` queued tracks but those `taken`, most urgent first.
fn claim(conn: &mut SqliteConnection, n: usize, taken: &HashSet<i64>) -> Result<Vec<Queued>> {
    // Analysed some other way since, or gone from the catalogue.
    diesel::sql_query(format!(
        "DELETE FROM mix_queue
         WHERE track_id IN (SELECT track_id FROM mix WHERE version = {VERSION})
            OR NOT EXISTS (SELECT 1 FROM tracks t WHERE t.id = mix_queue.track_id)"
    ))
    .execute(conn)?;
    let taken = taken.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT {columns}, q.priority AS priority
         FROM mix_queue q JOIN tracks t ON t.id = q.track_id LEFT JOIN artists a ON a.id = t.artist_id
         WHERE q.track_id NOT IN ({taken})
         ORDER BY q.priority, q.rowid
         LIMIT {n}",
        columns = pending_columns(),
    );
    Ok(diesel::sql_query(query).load(conn)?)
}

fn dequeue(conn: &mut SqliteConnection, id: i64) -> Result<()> {
    diesel::sql_query("DELETE FROM mix_queue WHERE track_id = ?").bind::<BigInt, _>(id).execute(conn)?;
    Ok(())
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn queued(conn: &mut SqliteConnection) -> Result<usize> {
    Ok(diesel::sql_query("SELECT COUNT(*) AS n FROM mix_queue").get_result::<Count>(conn)?.n as usize)
}

/// How many are queued for a more urgent reason than `reason`.
fn queued_before(conn: &mut SqliteConnection, reason: Reason) -> Result<usize> {
    let count = diesel::sql_query("SELECT COUNT(*) AS n FROM mix_queue WHERE priority < ?").bind::<Integer, _>(reason as i32);
    Ok(count.get_result::<Count>(conn)?.n as usize)
}

/// Rung when something is queued, so the worker need not wait to look.
#[derive(Default)]
struct Bell {
    rung: Mutex<bool>,
    ringing: Condvar,
}

impl Bell {
    fn ring(&self) {
        *self.rung.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ringing.notify_all();
    }

    /// Until it rings, or `at_most`.
    fn wait(&self, at_most: Duration) {
        let mut rung = self.rung.lock().unwrap_or_else(|e| e.into_inner());
        if !*rung {
            rung = self.ringing.wait_timeout(rung, at_most).unwrap_or_else(|e| e.into_inner()).0;
        }
        *rung = false;
    }
}

// ----------------------------------------------------------------- serving

/// The selection last asked for, as `server-analyse` follows it: running
/// until each track it queued has been analysed or has failed.
#[derive(Default)]
struct Batch {
    status: JobStatus,
    /// Still choosing its tracks.
    selecting: bool,
    left: HashSet<i64>,
    stats: Stats,
}

impl Batch {
    fn finish(&mut self, last: String) {
        self.status.running = false;
        self.status.stats = Some(self.stats.clone());
        self.status.last = Some(last);
        self.left.clear();
    }
}

/// The mix library as the server holds it, loaded on first use and again
/// as tracks are analysed, and the queue's worker, for as long as the
/// server runs.
pub struct MixService {
    paths: Paths,
    /// The server's space, which a selection made from it walks.
    space: Arc<SpaceService>,
    loaded: Arc<tokio::sync::RwLock<Option<Arc<Mix>>>>,
    batch: Arc<Mutex<Batch>>,
    /// Calls off the selection being chosen.
    cancel: Arc<AtomicBool>,
    bell: Arc<Bell>,
    /// Set by `stop`: the worker drops what selections queued.
    drop_selections: AtomicBool,
    /// Likes are being queued, and whether to look again once that is done.
    liking: Arc<AtomicBool>,
    liked_since: Arc<AtomicBool>,
    /// Tracks the queue gave the space their excerpt's analysis since the
    /// hub last rebuilt it for them, and the signal that a burst added some.
    spaced: AtomicUsize,
    settled: tokio::sync::Notify,
    /// Each track as it is recorded, and why not when it failed, for whoever
    /// is waiting on one.
    finished: tokio::sync::broadcast::Sender<(i64, std::result::Result<(), String>)>,
    log: Arc<LogBuffer>,
}

/// What a track asked for came to.
pub enum Asked {
    Analysed(Track),
    Failed(String),
    /// Not in the catalogue, so nothing to fetch it by.
    Unknown,
    /// Still queued after `ASKED_WAIT`: the providers not answering, or a
    /// long queue ahead of the tracks a worker already has.
    Waiting,
}

impl MixService {
    pub fn new(paths: Paths, space: Arc<SpaceService>, log: Arc<LogBuffer>) -> Self {
        Self {
            paths,
            space,
            loaded: Default::default(),
            batch: Default::default(),
            cancel: Default::default(),
            bell: Default::default(),
            drop_selections: AtomicBool::new(false),
            liking: Default::default(),
            liked_since: Default::default(),
            spaced: AtomicUsize::new(0),
            settled: tokio::sync::Notify::new(),
            finished: tokio::sync::broadcast::channel(256).0,
            log,
        }
    }

    /// The library, read from the database the first time.
    pub async fn mix(&self) -> Result<Arc<Mix>> {
        if let Some(mix) = self.loaded.read().await.clone() {
            return Ok(mix);
        }
        let mut slot = self.loaded.write().await;
        if let Some(mix) = slot.clone() {
            return Ok(mix);
        }
        let db_path = self.paths.db_path.clone();
        let mix = Arc::new(tokio::task::spawn_blocking(move || load(&db_path)).await??);
        *slot = Some(mix.clone());
        Ok(mix)
    }

    /// What was added is read in on the next ask.
    fn forget_loaded(&self) {
        *self.loaded.blocking_write() = None;
    }

    /// The same, from async code: after a dedupe, which moves analyses
    /// from the tracks it merges to the ones it keeps.
    pub async fn reread(&self) {
        *self.loaded.write().await = None;
    }

    pub fn status(&self) -> JobStatus {
        self.batch.lock().unwrap_or_else(|e| e.into_inner()).status.clone()
    }

    /// Call off the selection: its choosing, and what it queued that is not
    /// already being analysed. What was queued for another reason stays.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.drop_selections.store(true, Ordering::SeqCst);
        self.bell.ring();
        let mut batch = self.batch.lock().unwrap_or_else(|e| e.into_inner());
        if batch.status.running && !batch.selecting {
            let stats = batch.stats.clone();
            batch.finish(format!("stopped, {} analysed, {} failed", stats.done, stats.failed));
        }
    }

    /// Choose `selection`'s tracks on a thread of its own and queue them;
    /// refused while the last selection is still running.
    pub fn start(&self, selection: Selection) -> Result<()> {
        {
            let mut batch = self.batch.lock().unwrap_or_else(|e| e.into_inner());
            if batch.status.running {
                bail!("a mix analysis is already running: {}", batch.status.selection.clone().unwrap_or_default());
            }
            *batch = Batch {
                status: JobStatus { running: true, selection: Some(selection.to_string()), ..Default::default() },
                selecting: true,
                ..Default::default()
            };
        }
        self.cancel.store(false, Ordering::SeqCst);
        let (paths, batch, log, bell) = (self.paths.clone(), self.batch.clone(), self.log.clone(), self.bell.clone());
        let space = self.space.clone();
        let sink = {
            let (batch, log) = (batch.clone(), log.clone());
            move |line: &str| {
                log.push(format!("mix: {line}"));
                batch.lock().unwrap_or_else(|e| e.into_inner()).status.last = Some(line.to_string());
            }
        };
        let job = Job::new(sink, self.cancel.clone());
        std::thread::Builder::new().name("stelly-mix-select".into()).spawn(move || {
            let chosen = (|| -> Result<Vec<i64>> {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(select(&paths, &selection, &job, Some(&space)))
            })();
            let mut batch = batch.lock().unwrap_or_else(|e| e.into_inner());
            batch.selecting = false;
            // Queued holding the batch, so the worker cannot finish one of its
            // tracks before the batch knows it is one.
            let queued = chosen.and_then(|ids| {
                if job.cancelled() {
                    return Ok(Vec::new());
                }
                enqueue(&mut crate::db::open_for_write(&paths.db_path)?, &ids, Reason::Selection)
            });
            match queued {
                Ok(ids) if ids.is_empty() => batch.finish("nothing to analyse".into()),
                Ok(ids) => {
                    let line = format!("{} tracks queued", ids.len());
                    log.push(format!("mix: {line}"));
                    batch.status.last = Some(line);
                    batch.stats.total = ids.len();
                    batch.left = ids.into_iter().collect();
                    bell.ring();
                }
                Err(err) => {
                    log.push(format!("mix: {err:#}"));
                    batch.finish(format!("{err:#}"));
                }
            }
        })?;
        Ok(())
    }

    /// Queue `ids` for `reason` and wake the worker; answers those queued.
    pub fn enqueue(&self, ids: &[i64], reason: Reason) -> Result<Vec<i64>> {
        let queued = enqueue(&mut crate::db::open_for_write(&self.paths.db_path)?, ids, reason)?;
        if !queued.is_empty() {
            self.bell.ring();
        }
        Ok(queued)
    }

    /// Analyse `id` ahead of everything and wait for it: what stelly-mix
    /// asks for a track it is about to mix and has no analysis of.
    pub async fn analyse_now(&self, id: i64) -> Result<Asked> {
        // Before queueing, so the end cannot come between the two.
        let mut finished = self.finished.subscribe();
        let db_path = self.paths.db_path.clone();
        let read = move || -> Result<Option<Track>> { stored(&mut crate::db::open_for_write(&db_path)?, id) };
        if let Some(track) = tokio::task::spawn_blocking(read.clone()).await?? {
            return Ok(Asked::Analysed(track));
        }
        let db_path = self.paths.db_path.clone();
        let queued = tokio::task::spawn_blocking(move || enqueue(&mut crate::db::open_for_write(&db_path)?, &[id], Reason::Asked)).await??;
        if queued.is_empty() {
            return Ok(Asked::Unknown);
        }
        self.bell.ring();
        let outcome = tokio::time::timeout(ASKED_WAIT, async {
            loop {
                match finished.recv().await {
                    Ok((done, outcome)) if done == id => return outcome,
                    Ok(_) => {}
                    // Missed some: it may be among them.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if matches!(tokio::task::spawn_blocking(read.clone()).await, Ok(Ok(Some(_)))) {
                            return Ok(());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("the mix queue is gone".to_string()),
                }
            }
        })
        .await;
        Ok(match outcome {
            Err(_) => Asked::Waiting,
            Ok(Err(why)) => Asked::Failed(why),
            Ok(Ok(())) => {
                // So `next` and `set` know it at once.
                self.reread().await;
                match tokio::task::spawn_blocking(read).await?? {
                    Some(track) => Asked::Analysed(track),
                    None => Asked::Failed("analysed, then gone: merged into another track?".into()),
                }
            }
        })
    }

    /// Queue the tracks of every like not queued yet, newest first, on a
    /// thread of its own: a track or album not catalogued is, first. Called
    /// at start, for those from before this or added from the command
    /// line, and after each like.
    pub fn queue_likes(self: &Arc<Self>) {
        self.liked_since.store(true, Ordering::SeqCst);
        if self.liking.swap(true, Ordering::SeqCst) {
            return;
        }
        let service = self.clone();
        let spawned = std::thread::Builder::new().name("stelly-mix-likes".into()).spawn(move || loop {
            while service.liked_since.swap(false, Ordering::SeqCst) {
                if let Err(err) = service.queue_likes_now() {
                    service.log.push(format!("mix: queueing likes: {err:#}"));
                }
            }
            service.liking.store(false, Ordering::SeqCst);
            // A like that came between the last look and the flag going down.
            if !service.liked_since.load(Ordering::SeqCst) || service.liking.swap(true, Ordering::SeqCst) {
                break;
            }
        });
        if let Err(err) = spawned {
            self.liking.store(false, Ordering::SeqCst);
            self.log.push(format!("mix: queueing likes: {err:#}"));
        }
    }

    fn queue_likes_now(&self) -> Result<()> {
        let log = self.log.clone();
        let job = Job::new(move |line: &str| log.push(format!("mix: {line}")), Arc::new(AtomicBool::new(false)));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let conn = &mut crate::db::open_for_write(&self.paths.db_path)?;
        let likes = unqueued_likes(conn)?;
        if likes.is_empty() {
            return Ok(());
        }
        let mut client = Provider::from_env(&self.paths.env_dir, &self.paths.db_path).bulk();
        let now = crate::likes::now();
        let (mut liked, mut later) = (HashSet::new(), HashSet::new());
        for like in likes {
            let ids = match like.kind.as_str() {
                "track" | "album" => {
                    let target = match like.kind.as_str() {
                        "track" => like.ref_id.parse().map(Target::Track).ok(),
                        _ => Some(Target::Album(like.ref_id.clone())),
                    };
                    match target {
                        Some(target) => runtime.block_on(crate::crawl::catalogue_target(conn, &mut client, &target, &job)),
                        None => Ok(Vec::new()),
                    }
                }
                "artist" => like.ref_id.parse().map_or(Ok(Vec::new()), |artist| artist_tracks(conn, artist)),
                _ => Ok(Vec::new()),
            };
            // One that cannot be catalogued is told once, not tried at
            // every start.
            let ids = ids.unwrap_or_else(|err| {
                job.log(format!("liked {} {}: {err:#}", like.kind, like.ref_id));
                Vec::new()
            });
            // A like just made is what stelly-mix opens on; older ones, and
            // every artist's, fill in behind everything else.
            let reason = match like.kind.as_str() {
                "artist" => Reason::Played,
                _ if now - like.liked_at <= RECENT_LIKE => Reason::Liked,
                _ => Reason::Played,
            };
            let queued = enqueue(conn, &ids, reason)?;
            let any = !queued.is_empty();
            match reason {
                Reason::Liked => liked.extend(queued),
                _ => later.extend(queued),
            }
            diesel::sql_query("INSERT OR IGNORE INTO mix_liked (kind, ref_id, queued_at) VALUES (?, ?, ?)")
                .bind::<Text, _>(&like.kind)
                .bind::<Text, _>(&like.ref_id)
                .bind::<Text, _>(crate::db::utc_now())
                .execute(conn)?;
            if any {
                self.bell.ring();
            }
        }
        if !liked.is_empty() || !later.is_empty() {
            let later = later.difference(&liked).count();
            job.log(format!("{} liked tracks queued, and {later} behind everything else", liked.len()));
        }
        Ok(())
    }

    /// Until a burst has given the space tracks it does not show yet;
    /// answers how many, since the last time this answered.
    pub async fn spaced(&self) -> usize {
        loop {
            let spaced = self.spaced.swap(0, Ordering::SeqCst);
            if spaced > 0 {
                return spaced;
            }
            self.settled.notified().await;
        }
    }

    /// Start the worker that drains the queue, for as long as the server
    /// runs: it looks when something is queued, and now and then anyway.
    pub fn serve(self: &Arc<Self>) -> Result<()> {
        let service = self.clone();
        std::thread::Builder::new().name("stelly-mix-queue".into()).spawn(move || loop {
            if let Err(err) = service.drain() {
                service.log.push(format!("mix: {err:#}"));
                std::thread::sleep(RETRY_AFTER);
            }
            service.bell.wait(LOOK_EVERY);
        })?;
        Ok(())
    }

    /// Analyse what is queued until nothing is. The workers start with the
    /// first track and go with the last, so an idle server holds no ONNX
    /// session for them.
    ///
    /// Played tracks are the library growing by itself, which is no hurry:
    /// one at a time, on one worker, when nothing else is queued. A burst of
    /// them that something more urgent comes into ends once its track is
    /// done, as does a burst of urgent ones once only played tracks are
    /// left, and the next starts with as many workers as it wants.
    fn drain(&self) -> Result<()> {
        let mut conn = crate::db::open_for_write(&self.paths.db_path)?;
        self.drop_if_stopped(&mut conn, None)?;
        let waiting = queued(&mut conn)?;
        if waiting == 0 {
            self.finish_if_idle();
            return Ok(());
        }
        let log = self.log.clone();
        let job = Job::new(move |line: &str| log.push(format!("mix: {line}")), Arc::new(AtomicBool::new(false)));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(models::ensure(&self.paths.model_dir, &[models::CLAP_AUDIO], &job))?;

        let urgent = queued_before(&mut conn, Reason::Played)?;
        let workers = if urgent == 0 { 1 } else { default_workers().min(urgent) };
        job.log(format!("{waiting} tracks queued, {} played through; {workers} workers", waiting - urgent));
        let feed = Feed::new(Vec::new());
        let (report, results) = mpsc::channel();
        let handles = start_workers(&self.paths, workers, &feed, report, &job)?;
        let mut taken = HashSet::new();
        let mut unread = 0;
        let mut spaced = 0;
        // Whether to start again with other workers, once what they have is done.
        let mut again = false;
        // Why the audio cannot be had, when it cannot: nothing more is handed
        // out, and the queue waits to try again.
        let mut stalled: Option<String> = None;
        let drained = (|| -> Result<bool> {
            loop {
                self.drop_if_stopped(&mut conn, Some((&feed, &mut taken)))?;
                if feed.len() < workers && !again && stalled.is_none() {
                    for queued in claim(&mut conn, workers - feed.len(), &taken)? {
                        let reason = Reason::of(queued.priority);
                        match reason {
                            Reason::Played if workers > 1 => again = true,
                            Reason::Played if !taken.is_empty() => {}
                            Reason::Played => {
                                taken.insert(queued.track.id);
                                feed.push(Work::of(queued.track, reason));
                                continue;
                            }
                            _ if urgent == 0 => again = true,
                            _ => {
                                taken.insert(queued.track.id);
                                feed.push(Work::of(queued.track, reason));
                                continue;
                            }
                        }
                        break;
                    }
                }
                if taken.is_empty() {
                    if let Some(why) = stalled.take() {
                        bail!("waiting for the music providers, {}s: {why}", RETRY_AFTER.as_secs());
                    }
                    return Ok(!again);
                }
                let (id, name, outcome) = match results.recv_timeout(Duration::from_secs(2)) {
                    Ok(result) => result,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the mix workers stopped"),
                };
                taken.remove(&id);
                if outcome.unreachable {
                    // Back to the queue, and so is what was handed out.
                    for id in feed.drop_where(|_| true) {
                        taken.remove(&id);
                    }
                    stalled = outcome.mix.err();
                    continue;
                }
                let recorded = record(&mut conn, id, &name, outcome, &job)?;
                spaced += recorded.spaced as usize;
                dequeue(&mut conn, id)?;
                self.tell(id, &recorded);
                unread += 1;
                if unread >= RELOAD_EVERY {
                    self.forget_loaded();
                    unread = 0;
                }
            }
        })();
        feed.close();
        for handle in handles {
            let _ = handle.join();
        }
        self.forget_loaded();
        if spaced > 0 {
            self.spaced.fetch_add(spaced, Ordering::SeqCst);
            self.settled.notify_one();
        }
        match drained? {
            true => self.finish_if_idle(),
            false => self.bell.ring(),
        }
        Ok(())
    }

    /// After `stop`: the selections' tracks out of the queue, and out of
    /// the feed when there is one, but those a worker already has.
    fn drop_if_stopped(&self, conn: &mut SqliteConnection, feed: Option<(&Feed, &mut HashSet<i64>)>) -> Result<()> {
        if !self.drop_selections.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let dropped = diesel::sql_query("DELETE FROM mix_queue WHERE priority = ?")
            .bind::<Integer, _>(Reason::Selection as i32)
            .execute(conn)?;
        if let Some((feed, taken)) = feed {
            for id in feed.drop_where(|work| work.reason == Reason::Selection) {
                taken.remove(&id);
            }
        }
        if dropped > 0 {
            self.log.push(format!("mix: {dropped} queued tracks dropped"));
        }
        Ok(())
    }

    /// One track done: told in the log, and counted in the selection if it
    /// is one of its.
    fn tell(&self, id: i64, recorded: &Recorded) {
        self.log.push(format!("mix: {}", recorded.line));
        let _ = self.finished.send((id, if recorded.ok { Ok(()) } else { Err(recorded.line.clone()) }));
        let mut batch = self.batch.lock().unwrap_or_else(|e| e.into_inner());
        if !batch.left.remove(&id) {
            return;
        }
        match recorded.ok {
            true => batch.stats.done += 1,
            false => batch.stats.failed += 1,
        }
        let Stats { total, done, failed } = batch.stats.clone();
        batch.status.last = Some(format!("[{}/{total}] {}", done + failed, recorded.line));
        if batch.left.is_empty() {
            batch.finish(format!("mix-analysed {done} tracks, {failed} failed"));
        }
    }

    /// The queue is empty: a selection still running had its tracks done
    /// some other way, or before it knew they were its.
    fn finish_if_idle(&self) {
        let mut batch = self.batch.lock().unwrap_or_else(|e| e.into_inner());
        if batch.status.running && !batch.selecting {
            let Stats { done, failed, .. } = batch.stats.clone();
            batch.finish(format!("mix-analysed {done} tracks, {failed} failed"));
        }
    }
}

/// Liked within this, in seconds, a like goes ahead of selections: one just
/// made, or made while the server was down. Older, from before the queue or
/// imported, it waits with the played tracks: an account's favourites are
/// thousands of tracks, days of analysis.
const RECENT_LIKE: i64 = 24 * 60 * 60;
/// The tracks an artist's like queues, of theirs already catalogued.
const ARTIST_TRACKS: i64 = 20;

#[derive(QueryableByName)]
struct Like {
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = Text)]
    ref_id: String,
    #[diesel(sql_type = BigInt)]
    liked_at: i64,
}

/// The family's likes whose tracks were not queued yet, newest first.
fn unqueued_likes(conn: &mut SqliteConnection) -> Result<Vec<Like>> {
    Ok(diesel::sql_query(
        "SELECT l.kind AS kind, l.ref_id AS ref_id, MAX(l.liked_at) AS liked_at FROM likes l
         WHERE NOT EXISTS (SELECT 1 FROM mix_liked q WHERE q.kind = l.kind AND q.ref_id = l.ref_id)
         GROUP BY l.kind, l.ref_id
         ORDER BY liked_at DESC",
    )
    .load(conn)?)
}

/// An artist's catalogued tracks, those nearest the family's likes first.
fn artist_tracks(conn: &mut SqliteConnection, artist: i64) -> Result<Vec<i64>> {
    #[derive(QueryableByName)]
    struct Id {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    let ids: Vec<Id> = diesel::sql_query("SELECT id FROM tracks WHERE artist_id = ? ORDER BY seed_distance, id LIMIT ?")
        .bind::<BigInt, _>(artist)
        .bind::<BigInt, _>(ARTIST_TRACKS)
        .load(conn)?;
    Ok(ids.into_iter().map(|row| row.id).collect())
}

/// How long a track asked for is waited on before the asking is told it is
/// still queued.
const ASKED_WAIT: Duration = Duration::from_secs(10 * 60);

/// After a failure to drain, how long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How often the worker looks at the queue unasked, for what was queued by
/// another process, the command line's.
const LOOK_EVERY: Duration = Duration::from_secs(5 * 60);
/// The library is read in again after this many new tracks, and at the end.
const RELOAD_EVERY: usize = 25;

// --------------------------------------------------------------- analysing

/// One track to do, and what it is called.
struct Work {
    id: i64,
    item: Item,
    needs_features: bool,
    reason: Reason,
}

impl Work {
    fn of(p: Pending, reason: Reason) -> Self {
        Work {
            id: p.id,
            item: Item {
                id: p.id.to_string(),
                artist: p.artist.unwrap_or_default(),
                title: p.title,
                seconds: p.duration.map(|d| d as f64),
            },
            needs_features: p.needs_features,
            reason,
        }
    }
}

/// The mix analysis, and the space's of the excerpt when it was wanted.
struct Outcome {
    mix: std::result::Result<Track, String>,
    features: Option<std::result::Result<(Descriptors, Vec<f32>), String>>,
    /// The audio could not be had for now, which says nothing of the track:
    /// it stays queued rather than failed.
    unreachable: bool,
}

impl Outcome {
    fn failed(err: impl std::fmt::Display) -> Self {
        Outcome { mix: Err(err.to_string()), features: None, unreachable: false }
    }
}

/// Whether fetching failed for want of anyone to fetch from: no provider
/// answering, or the network not there or too slow, rather than anything
/// about the track.
async fn unreachable(client: &Provider, err: &anyhow::Error) -> bool {
    let network = err.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|e| e.is_connect() || e.is_timeout())
            // A provider's own deadline, `Provider::first`'s.
            || cause.to_string().contains("did not answer in")
    });
    network || !client.answering_any().await
}

/// Tracks handed to the workers, which wait for more until it is closed.
struct Feed {
    ready: Mutex<VecDeque<Work>>,
    more: Condvar,
    closed: AtomicBool,
}

impl Feed {
    fn new(work: Vec<Work>) -> Arc<Self> {
        Arc::new(Feed { ready: Mutex::new(work.into()), more: Condvar::new(), closed: AtomicBool::new(false) })
    }

    fn ready(&self) -> std::sync::MutexGuard<'_, VecDeque<Work>> {
        self.ready.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asked for goes first: someone is waiting on it.
    fn push(&self, work: Work) {
        match work.reason {
            Reason::Asked => self.ready().push_front(work),
            _ => self.ready().push_back(work),
        }
        self.more.notify_one();
    }

    fn len(&self) -> usize {
        self.ready().len()
    }

    /// Takes out the work `drop` says, answering whose it was.
    fn drop_where(&self, drop: impl Fn(&Work) -> bool) -> Vec<i64> {
        let mut ready = self.ready();
        let dropped = ready.iter().filter(|w| drop(w)).map(|w| w.id).collect();
        ready.retain(|w| !drop(w));
        dropped
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.more.notify_all();
    }

    /// The next track, waiting for one; none once closed and empty.
    fn take(&self) -> Option<Work> {
        let mut ready = self.ready();
        loop {
            if let Some(work) = ready.pop_front() {
                return Some(work);
            }
            if self.closed.load(Ordering::SeqCst) {
                return None;
            }
            ready = self.more.wait_timeout(ready, Duration::from_secs(1)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

type Report = (i64, String, Outcome);

/// `n` workers taking from `feed` until it closes, each with its own ONNX
/// session and gateway client, reporting each track on `report`.
fn start_workers(paths: &Paths, n: usize, feed: &Arc<Feed>, report: mpsc::Sender<Report>, job: &Job) -> Result<Vec<std::thread::JoinHandle<()>>> {
    // Where each entry says it came from: Stelly's own ids, `stelly:<id>` to
    // stelly-mix, whichever provider has the audio.
    let source = stelly_mix::stelly::SOURCE.to_string();
    let mut handles = Vec::new();
    for i in 0..n {
        let (feed, report, job) = (feed.clone(), report.clone(), job.clone());
        let (env_dir, db_path) = (paths.env_dir.clone(), paths.db_path.clone());
        let source = source.clone();
        // Loaded here, so a missing model fails the stage rather than a thread.
        let mut encoder = stelly_audio::clap::AudioEncoder::load(&paths.model_dir, 1)?;
        let handle = std::thread::Builder::new().name(format!("stelly-mix-{i}")).spawn(move || {
            crate::pipeline::step_back();
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            let client = Provider::from_env(&env_dir, &db_path).bulk();
            let Ok(http) = reqwest::Client::builder().timeout(DOWNLOAD_TIMEOUT).build() else { return };
            loop {
                if job.cancelled() {
                    break;
                }
                let Some(work) = feed.take() else { break };
                let name = format!("{} - {}", work.item.artist, work.item.title);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Outcome {
                    let (bytes, mime) = match runtime.block_on(download(&client, &http, work.id)) {
                        Ok(downloaded) => downloaded,
                        Err(err) => {
                            let unreachable = runtime.block_on(unreachable(&client, &err));
                            return Outcome { unreachable, ..Outcome::failed(format!("{err:#}")) };
                        }
                    };
                    let pcm = match audio::decode_any(bytes, mime.as_deref()) {
                        Ok(pcm) => pcm,
                        Err(err) => return Outcome::failed(format!("{err:#}")),
                    };
                    let origin = Origin { source: source.clone(), item: work.item.clone() };
                    let mix = stelly_mix::library::index_origin_pcm(&pcm, origin, &stelly_mix::Options::default(), &mut encoder)
                        .map_err(|err| format!("{err:#}"));
                    let features = work.needs_features.then(|| excerpt_features(&pcm, &mut encoder).map_err(|err| format!("{err:#}")));
                    Outcome { mix, features, unreachable: false }
                }));
                let outcome = outcome.unwrap_or_else(|_| Outcome::failed("the analysis panicked"));
                if report.send((work.id, name, outcome)).is_err() {
                    break;
                }
            }
        })?;
        handles.push(handle);
    }
    Ok(handles)
}

/// What one track came to, as stored.
struct Recorded {
    ok: bool,
    /// Its name and how it went.
    line: String,
    /// Its excerpt went into `features` too.
    spaced: bool,
}

fn record(conn: &mut SqliteConnection, id: i64, name: &str, outcome: Outcome, job: &Job) -> Result<Recorded> {
    // The excerpt's analysis stands on its own: a track whose phrases could
    // not be found can still join the space.
    let spaced = match &outcome.features {
        Some(Ok((descriptors, clap))) => {
            analyse::store(conn, id, descriptors, clap)?;
            true
        }
        Some(Err(error)) => {
            analyse::record_failure(conn, id, error)?;
            job.log(format!("{name}: excerpt for the space: {error}"));
            false
        }
        None => false,
    };
    Ok(match outcome.mix {
        Ok(track) => {
            store(conn, id, &track)?;
            let joined = if spaced { ", and its excerpt for the space" } else { "" };
            Recorded { ok: true, line: format!("{name}: {} phrases{joined}", track.segments.len()), spaced }
        }
        Err(error) => {
            record_failure(conn, id, &error)?;
            Recorded { ok: false, line: format!("{name}: {error}"), spaced }
        }
    })
}

/// Mix-analyse `ids`, those not done already, here and now rather than
/// through the queue: the command line's, with no server to drain it.
/// Blocking, on worker threads of its own; only the calling thread writes
/// to the database.
pub fn run(paths: &Paths, ids: &[i64], options: &Options, job: &Job) -> Result<Stats> {
    let mut conn = crate::db::open_for_write(&paths.db_path)?;
    let todo = pending(&mut conn, ids, options.retry_failed)?;
    let mut stats = Stats { total: todo.len(), ..Default::default() };
    if todo.is_empty() {
        job.log(format!("all {} already have a mix analysis", ids.len()));
        return Ok(stats);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(models::ensure(&paths.model_dir, &[models::CLAP_AUDIO], job))?;

    let workers = if options.workers > 0 { options.workers } else { default_workers() }.min(todo.len());
    let unanalysed = todo.iter().filter(|p| p.needs_features).count();
    job.log(format!("mix-analysing {} tracks, {unanalysed} new to the space, {workers} workers", todo.len()));
    let feed = Feed::new(todo.into_iter().map(|p| Work::of(p, Reason::Selection)).collect());
    feed.close();
    let (report, results) = mpsc::channel();
    let handles = start_workers(paths, workers, &feed, report, job)?;

    let mut spaced = 0;
    for (id, name, outcome) in results {
        let recorded = record(&mut conn, id, &name, outcome, job)?;
        match recorded.ok {
            true => stats.done += 1,
            false => stats.failed += 1,
        }
        spaced += recorded.spaced as usize;
        job.log(format!("[{}/{}] {}", stats.done + stats.failed, stats.total, recorded.line));
        job.advance("mix", (stats.done + stats.failed) as u64, stats.total as u64);
    }
    for handle in handles {
        let _ = handle.join();
    }
    job.log(format!("mix-analysed {} tracks, {} failed; {spaced} new to the space", stats.done, stats.failed));
    Ok(stats)
}

/// What `analyse` makes of the excerpt, cut from the samples already here.
fn excerpt_features(pcm: &audio::Pcm, encoder: &mut stelly_audio::clap::AudioEncoder) -> Result<(Descriptors, Vec<f32>)> {
    let excerpt = audio::excerpt_of(pcm);
    let seconds = excerpt.seconds();
    if seconds < audio::MIN_EXCERPT_SECONDS {
        bail!("excerpt of only {seconds:.1}s of audio");
    }
    analyse::analyse_pcm(encoder, &excerpt)
}

/// The whole track, lossy, and what format it is in.
async fn download(client: &Provider, http: &reqwest::Client, id: i64) -> Result<(Vec<u8>, Option<String>)> {
    let file = client.file(id, Quality::Lossy).await?;
    Ok((crate::pipeline::audio::fetch_whole(http, &file.url).await?, file.mime_type))
}

fn default_workers() -> usize {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cpus / CORES_PER_WORKER).clamp(1, MAX_WORKERS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(name: &str) -> SqliteConnection {
        let dir = std::env::temp_dir().join(format!("stelly-mix-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut conn = crate::db::open_for_write(&dir.join("stelly.db")).unwrap();
        diesel::connection::SimpleConnection::batch_execute(
            &mut conn,
            "INSERT INTO tracks (id, title) VALUES (1, 'One'), (2, 'Two'), (3, 'Three'), (4, 'Four'), (5, 'Five');",
        )
        .unwrap();
        conn
    }

    fn order(conn: &mut SqliteConnection, taken: &[i64]) -> Vec<(i64, Reason)> {
        let taken = taken.iter().copied().collect();
        claim(conn, 10, &taken).unwrap().into_iter().map(|q| (q.track.id, Reason::of(q.priority))).collect()
    }

    #[test]
    fn the_queue_goes_most_urgent_first_then_in_order() {
        let conn = &mut database("order");
        enqueue(conn, &[3, 1], Reason::Played).unwrap();
        enqueue(conn, &[2], Reason::Selection).unwrap();
        enqueue(conn, &[5], Reason::Liked).unwrap();
        assert_eq!(order(conn, &[]), [(5, Reason::Liked), (2, Reason::Selection), (3, Reason::Played), (1, Reason::Played)]);
        // Taken already: not handed out twice.
        assert_eq!(order(conn, &[5, 3]), [(2, Reason::Selection), (1, Reason::Played)]);
    }

    #[test]
    fn queued_again_a_track_keeps_its_more_urgent_reason() {
        let conn = &mut database("again");
        enqueue(conn, &[1, 2], Reason::Played).unwrap();
        enqueue(conn, &[2], Reason::Asked).unwrap();
        enqueue(conn, &[2], Reason::Played).unwrap();
        assert_eq!(order(conn, &[]), [(2, Reason::Asked), (1, Reason::Played)]);
    }

    #[test]
    fn analysed_failed_or_gone_tracks_leave_the_queue() {
        let conn = &mut database("leave");
        diesel::connection::SimpleConnection::batch_execute(
            conn,
            &format!(
                "INSERT INTO mix (track_id, version, entry_json, analysed_at) VALUES (1, {VERSION}, '{{}}', ''), (2, 0, '{{}}', '');
                 INSERT INTO mix_failures (track_id, reason) VALUES (3, 'no audio');"
            ),
        )
        .unwrap();
        // 1 is done, 3 failed; 2's analysis is from another version.
        assert_eq!(enqueue(conn, &[1, 2, 3, 4, 5], Reason::Selection).unwrap(), [2, 4, 5]);
        diesel::sql_query("DELETE FROM tracks WHERE id = 5").execute(conn).unwrap();
        // And 4 analysed some other way since it was queued.
        diesel::sql_query(format!("INSERT INTO mix (track_id, version, entry_json, analysed_at) VALUES (4, {VERSION}, '{{}}', '')"))
            .execute(conn)
            .unwrap();
        assert_eq!(order(conn, &[]), [(2, Reason::Selection)]);
        assert_eq!(queued(conn).unwrap(), 1);
    }

    #[test]
    fn asked_for_a_failed_track_is_tried_again() {
        let conn = &mut database("asked");
        diesel::connection::SimpleConnection::batch_execute(conn, "INSERT INTO mix_failures (track_id, reason) VALUES (1, 'no audio');")
            .unwrap();
        assert!(enqueue(conn, &[1], Reason::Liked).unwrap().is_empty());
        assert_eq!(enqueue(conn, &[1, 6], Reason::Asked).unwrap(), [1]);
        assert_eq!(order(conn, &[]), [(1, Reason::Asked)]);
    }

    #[test]
    fn each_like_is_queued_once_newest_first() {
        let conn = &mut database("likes");
        diesel::connection::SimpleConnection::batch_execute(
            conn,
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL);
             INSERT INTO users VALUES (1, 'one', ''), (2, 'two', '');",
        )
        .unwrap();
        crate::likes::ensure_schema(conn).unwrap();
        diesel::connection::SimpleConnection::batch_execute(
            conn,
            "INSERT INTO likes (user_id, kind, ref_id, liked_at) VALUES
               (1, 'track', '1', 10), (2, 'track', '1', 30), (1, 'album', 'a', 20), (1, 'artist', '9', 5);
             INSERT INTO mix_liked (kind, ref_id, queued_at) VALUES ('album', 'a', '');",
        )
        .unwrap();
        let likes: Vec<(String, String, i64)> = unqueued_likes(conn).unwrap().into_iter().map(|l| (l.kind, l.ref_id, l.liked_at)).collect();
        // Liked by two, once, as of the later.
        assert_eq!(likes, [("track".into(), "1".into(), 30), ("artist".into(), "9".into(), 5)]);
    }

    #[test]
    fn an_artists_like_is_their_tracks_nearest_the_likes() {
        let conn = &mut database("artist");
        let mut values: Vec<String> = (10..40).map(|id| format!("({id}, 't', 9, {})", 40 - id)).collect();
        values.push("(99, 'other', 8, 0)".into());
        diesel::connection::SimpleConnection::batch_execute(
            conn,
            &format!("INSERT INTO tracks (id, title, artist_id, seed_distance) VALUES {};", values.join(", ")),
        )
        .unwrap();
        let ids = artist_tracks(conn, 9).unwrap();
        assert_eq!(ids.len(), ARTIST_TRACKS as usize);
        assert_eq!(ids[..3], [39, 38, 37]);
    }

    #[test]
    fn names_compare_without_case_or_punctuation() {
        assert_eq!(squash("Lun"), squash("LUN."));
        assert_ne!(squash("Lun"), squash("Lún"));
    }
}
