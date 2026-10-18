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

use crate::pipeline::{models, Job, Paths};
use crate::provider::{Provider, Quality};
use crate::space::SpaceService;
use anyhow::{bail, Context, Result};
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer, Nullable, Text};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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

/// The catalogued tracks `selection` comes to.
pub async fn select(paths: &Paths, selection: &Selection, job: &Job) -> Result<Vec<i64>> {
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
            let space = load_space(paths)?;
            let request = GenerateRequest {
                recipe: Recipe::Mood { phrase: phrase.clone() },
                count: *count,
                penalty: 0.0,
                weights: Default::default(),
            };
            Ok(space.generate(&request, Some(&embedding)).unwrap_or_default().into_iter().map(|s| s.track.track_id).collect())
        }
        Selection::Around { seed, count } => {
            let space = load_space(paths)?;
            Ok(around(&space, &[*seed], *count))
        }
        Selection::ArtistName { name, count } => {
            let space = load_space(paths)?;
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
            let space = load_space(paths)?;
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

fn load_space(paths: &Paths) -> Result<SpaceService> {
    let space = SpaceService::new(paths.data_dir.clone(), paths.db_path.clone());
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
}

/// Of `ids`, those with no current mix analysis, nor a failure unless
/// `retry_failed`, in the order asked.
fn pending(conn: &mut SqliteConnection, ids: &[i64], retry_failed: bool) -> Result<Vec<Pending>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(500) {
        let list = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let failed = if retry_failed { "" } else { "AND NOT EXISTS (SELECT 1 FROM mix_failures f WHERE f.track_id = t.id)" };
        let query = format!(
            "SELECT t.id AS id, t.title AS title, a.name AS artist, t.duration AS duration
             FROM tracks t LEFT JOIN artists a ON a.id = t.artist_id
             WHERE t.id IN ({list})
               AND NOT EXISTS (SELECT 1 FROM mix m WHERE m.track_id = t.id AND m.version = {VERSION})
               {failed}"
        );
        out.extend(diesel::sql_query(query).load::<Pending>(conn)?);
    }
    let order: std::collections::HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    out.sort_by_key(|p| order.get(&p.id).copied().unwrap_or(usize::MAX));
    Ok(out)
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

// ----------------------------------------------------------------- serving

/// The mix library as the server holds it, loaded on first use and again
/// after each analysis, and the analysis an admin runs in the background.
pub struct MixService {
    paths: Paths,
    loaded: Arc<tokio::sync::RwLock<Option<Arc<Mix>>>>,
    job: Arc<Mutex<JobStatus>>,
    cancel: Arc<AtomicBool>,
    log: Arc<LogBuffer>,
}

impl MixService {
    pub fn new(paths: Paths, log: Arc<LogBuffer>) -> Self {
        Self {
            paths,
            loaded: Default::default(),
            job: Default::default(),
            cancel: Default::default(),
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

    pub fn status(&self) -> JobStatus {
        self.job.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Analyse `selection` on a thread of its own; nothing if one already runs.
    pub fn start(&self, selection: Selection) -> Result<()> {
        {
            let mut job = self.job.lock().unwrap_or_else(|e| e.into_inner());
            if job.running {
                bail!("a mix analysis is already running: {}", job.selection.clone().unwrap_or_default());
            }
            *job = JobStatus { running: true, selection: Some(selection.to_string()), ..Default::default() };
        }
        self.cancel.store(false, Ordering::SeqCst);
        let (paths, status, log, loaded) = (self.paths.clone(), self.job.clone(), self.log.clone(), self.loaded.clone());
        let sink = {
            let (status, log) = (status.clone(), log.clone());
            move |line: &str| {
                log.push(format!("mix: {line}"));
                status.lock().unwrap_or_else(|e| e.into_inner()).last = Some(line.to_string());
            }
        };
        let job = Job::new(sink, self.cancel.clone());
        std::thread::Builder::new().name("stelly-mix-job".into()).spawn(move || {
            let outcome = (|| -> Result<Stats> {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                let ids = runtime.block_on(select(&paths, &selection, &job))?;
                run(&paths, &ids, &Options::default(), &job)
            })();
            let mut status = status.lock().unwrap_or_else(|e| e.into_inner());
            status.running = false;
            match outcome {
                Ok(stats) => status.stats = Some(stats),
                Err(err) => {
                    log.push(format!("mix: {err:#}"));
                    status.last = Some(format!("{err:#}"));
                }
            }
            // What was added is read in on the next ask.
            *loaded.blocking_write() = None;
        })?;
        Ok(())
    }
}

// --------------------------------------------------------------- analysing

/// One track to do, and what it is called.
struct Work {
    id: i64,
    item: Item,
}

type Outcome = std::result::Result<Track, String>;

/// Mix-analyse `ids`, those not done already; blocking, on worker threads of
/// its own, each with its own ONNX session and gateway client. Only the
/// calling thread writes to the database.
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
    job.log(format!("mix-analysing {} tracks, {workers} workers", todo.len()));
    let queue: VecDeque<Work> = todo
        .into_iter()
        .map(|p| Work {
            id: p.id,
            item: Item {
                id: p.id.to_string(),
                artist: p.artist.unwrap_or_default(),
                title: p.title,
                seconds: p.duration.map(|d| d as f64),
            },
        })
        .collect();
    let queue = Arc::new(Mutex::new(queue));
    // Where each entry says it came from: Stelly's own ids, `stelly:<id>` to
    // stelly-mix, whichever provider has the audio.
    let source = stelly_mix::stelly::SOURCE.to_string();
    let (report, results) = mpsc::channel::<(i64, String, Outcome)>();

    let mut handles = Vec::new();
    for n in 0..workers {
        let (queue, report, job) = (queue.clone(), report.clone(), job.clone());
        let (env_dir, model_dir, db_path) = (paths.env_dir.clone(), paths.model_dir.clone(), paths.db_path.clone());
        let source = source.clone();
        // Loaded here, so a missing model fails the stage rather than a thread.
        let mut encoder = stelly_audio::clap::AudioEncoder::load(&model_dir, 1)?;
        let handle = std::thread::Builder::new().name(format!("stelly-mix-{n}")).spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            let client = Provider::from_env(&env_dir, &db_path).bulk();
            let Ok(http) = reqwest::Client::builder().timeout(DOWNLOAD_TIMEOUT).build() else { return };
            loop {
                if job.cancelled() {
                    break;
                }
                let Some(work) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() else { break };
                let name = format!("{} - {}", work.item.artist, work.item.title);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Track> {
                    let (bytes, mime) = runtime.block_on(download(&client, &http, work.id))?;
                    let pcm = crate::pipeline::audio::decode_any(bytes, mime.as_deref())?;
                    let origin = Origin { source: source.clone(), item: work.item.clone() };
                    stelly_mix::library::index_origin_pcm(&pcm, origin, &stelly_mix::Options::default(), &mut encoder)
                }));
                let outcome = match outcome {
                    Ok(Ok(track)) => Ok(track),
                    Ok(Err(err)) => Err(format!("{err:#}")),
                    Err(_) => Err("the analysis panicked".to_string()),
                };
                if report.send((work.id, name, outcome)).is_err() {
                    break;
                }
            }
        })?;
        handles.push(handle);
    }
    drop(report);

    for (id, name, outcome) in results {
        match outcome {
            Ok(track) => {
                store(&mut conn, id, &track)?;
                stats.done += 1;
                job.log(format!("[{}/{}] {name}: {} phrases", stats.done + stats.failed, stats.total, track.segments.len()));
            }
            Err(error) => {
                record_failure(&mut conn, id, &error)?;
                stats.failed += 1;
                job.log(format!("[{}/{}] {name}: {error}", stats.done + stats.failed, stats.total));
            }
        }
        job.advance("mix", (stats.done + stats.failed) as u64, stats.total as u64);
    }
    for handle in handles {
        let _ = handle.join();
    }
    job.log(format!("mix-analysed {} tracks, {} failed", stats.done, stats.failed));
    Ok(stats)
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

    #[test]
    fn names_compare_without_case_or_punctuation() {
        assert_eq!(squash("Lun"), squash("LUN."));
        assert_ne!(squash("Lun"), squash("Lún"));
    }
}
