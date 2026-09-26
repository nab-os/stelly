//! The analyse stage: excerpt -> descriptors + CLAP embedding -> `features`.
//!
//! The work splits in two. Fetching is network-bound and spends the account's
//! one request budget, so it runs as futures on the stage's own thread,
//! sharing one rate-limited client. Extraction is CPU-bound, a CLAP pass
//! over nine 10s windows plus the descriptors, and runs on worker threads,
//! each with its own ONNX session. Only the stage thread writes to SQLite.

use super::audio::{self, AudioCache, Pcm};
use super::clap::{self, AudioEncoder};
use super::descriptors::{self, Descriptors};
use super::{models, Job, Paths};
use crate::db::{self, not_blocked, utc_now};
use crate::qobuz::{Credentials, QobuzClient, RateLimit, FORMAT_MP3_320};
use crate::schema::{failures, features, tracks};
use anyhow::{Context, Result};
use diesel::prelude::*;
use diesel::upsert::excluded;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a stored row was made with. Rows with anything else are redone: bump
/// it whenever the CLAP input or a descriptor's definition changes.
pub const VERSION: &str = "clap-htsat-unfused+descriptors-1";

/// One ONNX thread per worker. A 90s excerpt is ~1.3 core-seconds on one
/// thread and ~0.9s wall on four, so extra threads buy little; more workers
/// scale better. And a handful already outrun Qobuz's 2 requests/s.
const THREADS_PER_WORKER: usize = 1;
/// One worker per this many hardware threads, so a server that is analysing
/// still has most of its cores for everything else.
const CORES_PER_WORKER: usize = 4;
/// And no more than this many by default: each holds an ONNX session of about
/// 600MB, and four already outrun Qobuz's rate limit. `--workers` goes past it,
/// for re-analysing from the cache, where there is no limit to wait on.
const DEFAULT_MAX_WORKERS: usize = 4;

#[derive(Debug, Clone)]
pub struct Options {
    /// 0 means every pending track.
    pub limit: usize,
    pub retry_failed: bool,
    pub cache_gb: f64,
    /// Extraction workers; 0 picks one per four hardware threads, up to four.
    pub workers: usize,
    /// Excerpts fetched at once; 0 follows `workers`.
    pub fetchers: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limit: 0,
            retry_failed: false,
            cache_gb: 20.0,
            workers: 0,
            fetchers: 0,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub done: usize,
    pub failed: usize,
    pub total: usize,
}

#[derive(Queryable)]
struct Pending {
    id: i64,
    title: String,
    duration: Option<i64>,
}

/// Tracks with no stored features, or features from another extractor.
#[diesel::dsl::auto_type]
fn unanalysed() -> _ {
    let version: &'static str = VERSION;
    features::track_id
        .nullable()
        .is_null()
        .or(features::extractor_version.nullable().ne(version))
}

/// Tracks with no features, stale features, and optionally past failures.
/// The user's own library first.
fn pending_tracks(conn: &mut SqliteConnection, limit: usize, retry_failed: bool) -> Result<Vec<Pending>> {
    let mut query = tracks::table
        .left_join(features::table)
        .left_join(failures::table)
        .filter(unanalysed())
        .filter(not_blocked())
        .order((tracks::seed_distance.asc(), tracks::id.asc()))
        .select((tracks::id, tracks::title, tracks::duration))
        .into_boxed();
    if !retry_failed {
        query = query.filter(failures::track_id.nullable().is_null());
    }
    if limit > 0 {
        query = query.limit(limit as i64);
    }
    Ok(query.load(conn)?)
}

/// How many tracks `run` would queue, for the pipeline view's counts.
pub fn pending_count(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(tracks::table
        .left_join(features::table)
        .left_join(failures::table)
        .filter(failures::track_id.nullable().is_null())
        .filter(unanalysed())
        .filter(not_blocked())
        .count()
        .get_result(conn)?)
}

pub fn store(conn: &mut SqliteConnection, track_id: i64, descriptors: &Descriptors, clap: &[f32]) -> Result<()> {
    diesel::insert_into(features::table)
        .values((
            features::track_id.eq(track_id),
            features::extractor_version.eq(VERSION),
            features::descriptors_json.eq(serde_json::to_string(descriptors)?),
            features::clap_f32.eq(bytemuck::cast_slice::<f32, u8>(clap)),
            features::analysed_at.eq(utc_now()),
        ))
        .on_conflict(features::track_id)
        .do_update()
        .set((
            features::extractor_version.eq(excluded(features::extractor_version)),
            features::descriptors_json.eq(excluded(features::descriptors_json)),
            features::clap_f32.eq(excluded(features::clap_f32)),
            features::analysed_at.eq(excluded(features::analysed_at)),
        ))
        .execute(conn)?;
    // A track that now analyses cleanly should not stay on the failure list.
    diesel::delete(failures::table.find(track_id)).execute(conn)?;
    Ok(())
}

fn record_failure(conn: &mut SqliteConnection, track_id: i64, reason: &str) -> Result<()> {
    let reason: String = reason.chars().take(500).collect();
    diesel::insert_into(failures::table)
        .values((
            failures::track_id.eq(track_id),
            failures::stage.eq("analyse"),
            failures::reason.eq(reason),
            failures::failed_at.eq(utc_now()),
        ))
        .on_conflict(failures::track_id)
        .do_update()
        .set((
            failures::stage.eq(excluded(failures::stage)),
            failures::reason.eq(excluded(failures::reason)),
            failures::failed_at.eq(excluded(failures::failed_at)),
        ))
        .execute(conn)?;
    Ok(())
}

// --------------------------------------------------------------- extraction

/// All the CPU work for one excerpt: resample once, then both analyses.
pub fn analyse_pcm(encoder: &mut AudioEncoder, pcm: &Pcm) -> Result<(Descriptors, Vec<f32>)> {
    let audio = audio::resample(&pcm.samples, pcm.rate, clap::SAMPLE_RATE);
    let descriptors = descriptors::extract(&audio, clap::SAMPLE_RATE)?;
    let embedding = encoder.embed(&audio)?;
    Ok((descriptors, embedding))
}

fn analyse_mp3(encoder: &mut AudioEncoder, bytes: Vec<u8>) -> Result<(Descriptors, Vec<f32>)> {
    analyse_pcm(encoder, &audio::excerpt_pcm(bytes)?)
}

type Outcome = std::result::Result<(Descriptors, Vec<f32>), String>;

/// A pool of extraction threads fed over a channel. Failures come back as
/// text, including a panic, so one bad file cannot take a worker down.
pub struct Workers {
    work: Option<mpsc::Sender<(i64, Vec<u8>)>>,
    results: tokio::sync::mpsc::UnboundedReceiver<(i64, Outcome)>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Workers {
    pub fn start(model_dir: &std::path::Path, workers: usize, threads: usize) -> Result<Self> {
        let (work, queue) = mpsc::channel::<(i64, Vec<u8>)>();
        let queue = Arc::new(Mutex::new(queue));
        let (report, results) = tokio::sync::mpsc::unbounded_channel();

        let mut handles = Vec::with_capacity(workers);
        for n in 0..workers {
            // Loaded here rather than on the thread, so a missing model is an
            // error from `start` instead of a pool that silently never works.
            let mut encoder = AudioEncoder::load(model_dir, threads)?;
            let queue = queue.clone();
            let report = report.clone();
            let handle = std::thread::Builder::new()
                .name(format!("two-khz-analyse-{n}"))
                .spawn(move || loop {
                    let next = queue.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok((track_id, bytes)) = next else { break };
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        analyse_mp3(&mut encoder, bytes)
                    }));
                    let outcome = match outcome {
                        Ok(Ok(features)) => Ok(features),
                        Ok(Err(err)) => Err(format!("{err:#}")),
                        Err(_) => Err("extraction panicked".to_string()),
                    };
                    if report.send((track_id, outcome)).is_err() {
                        break;
                    }
                })
                .context("spawning an analysis worker")?;
            handles.push(handle);
        }

        Ok(Self {
            work: Some(work),
            results,
            handles,
        })
    }

    fn submit(&self, track_id: i64, bytes: Vec<u8>) {
        if let Some(work) = &self.work {
            let _ = work.send((track_id, bytes));
        }
    }

    /// Let the workers finish what they hold, then wait for them.
    fn finish(mut self) {
        self.work = None;
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

pub fn default_workers() -> usize {
    (cpus() / CORES_PER_WORKER).clamp(1, DEFAULT_MAX_WORKERS)
}

fn cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

// ------------------------------------------------------------------- driver

/// Fetch one excerpt, from the cache when it is there.
async fn fetch(
    client: &tokio::sync::Mutex<QobuzClient>,
    http: &reqwest::Client,
    cache: &AudioCache,
    track: &Pending,
) -> Result<Vec<u8>> {
    if let Some(bytes) = cache.get(track.id) {
        return Ok(bytes);
    }
    if let Some(seconds) = track.duration.filter(|&d| d < audio::MIN_TRACK_SECONDS) {
        anyhow::bail!("track too short to analyse ({seconds}s)");
    }
    // Only the signed URL costs a request against the account; the excerpt
    // itself comes from the CDN, so the lock is held for just that.
    let url = client.lock().await.file_url(track.id, FORMAT_MP3_320).await?;
    let bytes = audio::fetch_excerpt(http, &url, track.duration).await?;
    cache.put(track.id, &bytes)?;
    Ok(bytes)
}

/// Analyse every pending track. Safe to interrupt and resume: each result is
/// committed as it lands.
///
/// The future holds a blocking SQLite connection across awaits: run it on
/// a thread of its own, as `stages` does.
pub async fn run(paths: &Paths, limiter: RateLimit, options: &Options, job: &Job) -> Result<Stats> {
    let mut conn = db::open_for_write(&paths.db_path)?;
    let todo = pending_tracks(&mut conn, options.limit, options.retry_failed)?;
    if todo.is_empty() {
        job.log("nothing to analyse");
        return Ok(Stats::default());
    }

    models::ensure(&paths.model_dir, &[models::CLAP_AUDIO], job).await?;

    let workers = if options.workers > 0 { options.workers } else { default_workers() };
    let workers = workers.clamp(1, todo.len());
    let threads = THREADS_PER_WORKER;
    // Fetching overlaps extraction, so a little depth here keeps the workers
    // busy. The rate limit is the real governor.
    let fetchers = if options.fetchers > 0 { options.fetchers } else { workers.max(4) };

    job.log(format!(
        "analysing {} tracks (extractor {VERSION})\n  {workers} workers x {threads} thread, {fetchers} fetchers",
        todo.len()
    ));

    let credentials = Credentials::from_env(&paths.env_dir)?;
    let mut client = QobuzClient::sharing(credentials, limiter);
    // Once, here: the fetches share this client and would otherwise race to
    // log in on their first call.
    client.login().await?;
    let client = tokio::sync::Mutex::new(client);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let cache = AudioCache::new(paths.cache_dir.clone(), (options.cache_gb * 1e9) as u64);

    let mut pool = Workers::start(&paths.model_dir, workers, threads)?;

    let mut stats = Stats {
        total: todo.len(),
        ..Default::default()
    };
    let titles: std::collections::HashMap<i64, String> =
        todo.iter().map(|t| (t.id, t.title.clone())).collect();
    let mut pending: VecDeque<Pending> = todo.into();
    let mut fetches = FuturesUnordered::new();
    let mut extracting = 0usize;
    // Excerpts sit in memory between fetch and extraction; cap how far ahead
    // fetching runs.
    let in_flight_cap = workers * 2 + fetchers;
    let started = Instant::now();
    let mut finished = 0usize;

    let fail = |conn: &mut SqliteConnection, stats: &mut Stats, track_id: i64, reason: &str| -> Result<()> {
        record_failure(conn, track_id, reason)?;
        stats.failed += 1;
        let title: String = titles
            .get(&track_id)
            .map(|t| t.chars().take(40).collect())
            .unwrap_or_default();
        job.log(format!("  ! {track_id} {title}: {reason}"));
        Ok(())
    };

    loop {
        if job.cancelled() {
            job.log(format!(
                "interrupted after {} tracks, rerun to resume",
                stats.done
            ));
            break;
        }

        while fetches.len() < fetchers && fetches.len() + extracting < in_flight_cap {
            let Some(track) = pending.pop_front() else { break };
            let (client, http, cache) = (&client, &http, &cache);
            fetches.push(async move {
                let result = fetch(client, http, cache, &track).await;
                (track.id, result)
            });
        }

        if fetches.is_empty() && extracting == 0 {
            break;
        }

        tokio::select! {
            Some((track_id, result)) = fetches.next(), if !fetches.is_empty() => {
                match result {
                    Ok(bytes) => {
                        pool.submit(track_id, bytes);
                        extracting += 1;
                    }
                    Err(err) => {
                        fail(&mut conn, &mut stats, track_id, &format!("{err:#}"))?;
                        finished += 1;
                    }
                }
            }
            Some((track_id, outcome)) = pool.results.recv(), if extracting > 0 => {
                extracting -= 1;
                match outcome {
                    Ok((descriptors, embedding)) => {
                        store(&mut conn, track_id, &descriptors, &embedding)?;
                        stats.done += 1;
                    }
                    Err(reason) => fail(&mut conn, &mut stats, track_id, &reason)?,
                }
                finished += 1;

                if finished.is_multiple_of(10) {
                    let rate = finished as f64 / started.elapsed().as_secs_f64().max(1e-6);
                    let remaining = (stats.total - finished) as f64 / rate.max(1e-6);
                    job.log(format!(
                        "  {finished}/{}  {rate:.2} tracks/s  ({:.0}/hour)  eta {:.1} min",
                        stats.total,
                        rate * 3600.0,
                        remaining / 60.0
                    ));
                }
            }
            // Wakes the loop so a stop request lands even while everything is
            // mid-download.
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }

    pool.finish();
    job.log(format!(
        "analysed {}/{} tracks, {} failed",
        stats.done, stats.total, stats.failed
    ));
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What one 90s excerpt costs, at a few thread counts. Needs the audio
    /// tower: `cargo test --release -- --ignored cost --nocapture`.
    #[test]
    #[ignore]
    fn cost_of_one_excerpt() {
        let model_dir = Paths::from_env().model_dir;
        let rate = 44_100usize;
        let samples: Vec<f32> = (0..rate * 90)
            .map(|i| {
                let t = i as f32 / rate as f32;
                0.3 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
                    + 0.05 * (((i * 7919) % 1000) as f32 / 500.0 - 1.0)
            })
            .collect();
        let pcm = Pcm {
            samples,
            rate: rate as u32,
        };

        for threads in [1, 4, cpus()] {
            let mut encoder = AudioEncoder::load(&model_dir, threads).unwrap();
            analyse_pcm(&mut encoder, &pcm).unwrap(); // warm up
            let started = Instant::now();
            let resampled = audio::resample(&pcm.samples, pcm.rate, clap::SAMPLE_RATE);
            let resampling = started.elapsed();
            descriptors::extract(&resampled, clap::SAMPLE_RATE).unwrap();
            let describing = started.elapsed() - resampling;
            encoder.embed(&resampled).unwrap();
            let total = started.elapsed();
            eprintln!(
                "{threads:>2} threads: {:.2}s wall (resample {:.2}s, descriptors {:.2}s, \
                 CLAP {:.2}s) = {:.1} core-seconds",
                total.as_secs_f64(),
                resampling.as_secs_f64(),
                describing.as_secs_f64(),
                (total - resampling - describing).as_secs_f64(),
                total.as_secs_f64() * threads as f64
            );
        }
    }
}
