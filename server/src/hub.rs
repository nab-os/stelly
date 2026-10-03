//! Everything the server does on its own disk and account: Qobuz, the block
//! list, the crawl loop and the pipeline driver. The routes are a thin skin
//! over this.
//!
//! The crawl and the stages publish a status that clients poll, because a
//! server cannot write a client's signals.

use crate::cache::Cache;
use crate::pipeline::{progress_of, Job, Paths, ProgressSlot};
use crate::qobuz::{QobuzClient, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use crate::schema::{frontier, tracks};
use crate::stages;
use crate::text::TextEncoder;
use crate::space::SpaceService;
use crate::{crawl, db};
use anyhow::{Context, Result};
use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use stelly_core::api::{BlockedArtist, Corpus, CrawlStatus, LogSlice, PipelineStatus, Stage, Target, FULL_RUN};
use stelly_core::logbuffer::LogBuffer;

pub struct Hub {
    paths: Paths,
    env_dir: PathBuf,
    db_path: PathBuf,
    model_dir: PathBuf,
    /// Built on first use: browsing the space needs no credentials, so a
    /// missing .env should only bite when you reach for Qobuz.
    client: OnceLock<Arc<tokio::sync::Mutex<QobuzClient>>>,
    /// Albums, artists, discographies: they change on the scale of weeks.
    catalogue: Cache,
    /// Short, since the same words find new releases.
    searches: Cache,
    /// Favourites and playlists, which the account can change from any Qobuz
    /// client. Cleared on our own writes, and short for everyone else's.
    account: Cache,
    /// Loaded on first use and kept. `None` inside means the tower has not
    /// been exported, which is not an error, the UI hides steering.
    text: tokio::sync::Mutex<Option<TextEncoder>>,
    text_tried: AtomicBool,
    crawl: Arc<CrawlJob>,
    pipeline: Arc<PipelineJob>,
    log: Arc<LogBuffer>,
    /// The loaded space, which every client's navigation runs against.
    space: Arc<SpaceService>,
}

// ------------------------------------------------------------------- jobs

#[derive(Default)]
struct CrawlJob {
    status: Mutex<CrawlStatus>,
    /// Set to ask the loop to stop after the current step.
    stop: AtomicBool,
}

#[derive(Default)]
struct PipelineJob {
    queue: Mutex<Queue>,
    /// Shared with the running stage's `Job`, which is how it hears a stop.
    cancel: Arc<AtomicBool>,
    /// Set along with `cancel` when a target interrupts the backlog's analyse,
    /// so the runner carries on rather than treating it as a stop.
    preempted: AtomicBool,
    generation: AtomicU64,
    /// The running stage's, cleared between stages.
    progress: ProgressSlot,
}

/// What the pipeline is doing and will do, under one lock so a target
/// arriving between two stages cannot be lost.
#[derive(Default)]
struct Queue {
    running: Option<Stage>,
    /// What `running` is scoped to, when it is a targeted analyse.
    target: Option<Target>,
    /// Targets still to analyse. They go before `stages`, whatever is in it.
    targets: VecDeque<Target>,
    stages: VecDeque<Stage>,
    /// Every target not on the map yet, for the pages that asked.
    adding: Vec<Target>,
}

impl Queue {
    fn next(&mut self) -> Option<(Stage, Option<Target>)> {
        if let Some(target) = self.targets.pop_front() {
            return Some((Stage::Analyse, Some(target)));
        }
        self.stages.pop_front().map(|stage| (stage, None))
    }

    /// Make build-space then layout the next stages, so what the targets add
    /// reaches the map before the backlog carries on.
    fn rebuild_next(&mut self) {
        match self.stages.front() {
            Some(Stage::BuildSpace) => {}
            Some(Stage::Layout) => self.stages.push_front(Stage::BuildSpace),
            _ => {
                self.stages.push_front(Stage::Layout);
                self.stages.push_front(Stage::BuildSpace);
            }
        }
    }
}

impl Hub {
    pub fn new(paths: Paths) -> Self {
        Self {
            space: Arc::new(SpaceService::new(paths.data_dir.clone(), paths.db_path.clone())),
            env_dir: paths.env_dir.clone(),
            db_path: paths.db_path.clone(),
            model_dir: paths.model_dir.clone(),
            paths,
            client: OnceLock::new(),
            catalogue: Cache::new(Duration::from_secs(6 * 60 * 60), 5_000),
            searches: Cache::new(Duration::from_secs(5 * 60), 500),
            account: Cache::new(Duration::from_secs(2 * 60), 200),
            text: tokio::sync::Mutex::new(None),
            text_tried: AtomicBool::new(false),
            crawl: Arc::new(CrawlJob::default()),
            pipeline: Arc::new(PipelineJob::default()),
            log: Arc::new(LogBuffer::default()),
        }
    }

    pub fn space(&self) -> &Arc<SpaceService> {
        &self.space
    }

    /// The shared Qobuz client, built on first reach.
    fn qobuz(&self) -> Result<Arc<tokio::sync::Mutex<QobuzClient>>> {
        if let Some(existing) = self.client.get() {
            return Ok(existing.clone());
        }
        let built = QobuzClient::from_repo(&self.env_dir)?;
        let _ = self.client.set(Arc::new(tokio::sync::Mutex::new(built)));
        Ok(self.client.get().expect("just set").clone())
    }

    /// A logged-in copy of the shared client, for reads. The lock is held only
    /// while copying, so one device's request never waits out another's turn
    /// at the rate limiter; they still queue on the same budget.
    async fn session(&self) -> Result<QobuzClient> {
        let client = self.qobuz()?;
        let mut shared = client.lock().await;
        shared.login().await?;
        Ok(shared.clone())
    }

    // -------------------------------------------------------------- browsing

    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults> {
        let key = format!("search:{limit}:{query}");
        self.searches
            .get_or(key, async { self.session().await?.search(query, limit).await })
            .await
    }

    pub async fn favourite_tracks(&self, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.account
            .get_or(format!("favourite_tracks:{cap}"), async {
                self.session().await?.favorite_tracks(cap).await
            })
            .await
    }

    pub async fn favourite_albums(&self, cap: usize) -> Result<Vec<RemoteAlbum>> {
        self.account
            .get_or(format!("favourite_albums:{cap}"), async {
                self.session().await?.favorite_albums(cap).await
            })
            .await
    }

    pub async fn favourite_artists(&self, cap: usize) -> Result<Vec<RemoteArtist>> {
        self.account
            .get_or(format!("favourite_artists:{cap}"), async {
                self.session().await?.favorite_artists(cap).await
            })
            .await
    }

    pub async fn playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        self.account
            .get_or(format!("playlists:{cap}"), async {
                self.session().await?.user_playlists(cap).await
            })
            .await
    }

    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.account
            .get_or(format!("playlist_tracks:{playlist_id}:{cap}"), async {
                self.session().await?.playlist_tracks(playlist_id, cap).await
            })
            .await
    }

    pub async fn album_tracks(&self, album_id: &str) -> Result<Vec<RemoteTrack>> {
        self.catalogue
            .get_or(format!("album_tracks:{album_id}"), async {
                Ok(self.session().await?.album_tracks(album_id).await?.1)
            })
            .await
    }

    pub async fn artist(&self, artist_id: i64) -> Result<RemoteArtist> {
        self.catalogue
            .get_or(format!("artist:{artist_id}"), async {
                self.session().await?.artist(artist_id).await
            })
            .await
    }

    pub async fn artist_albums(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        self.catalogue
            .get_or(format!("artist_albums:{artist_id}:{cap}"), async {
                Ok(self.session().await?.artist_albums(artist_id, cap).await?.1)
            })
            .await
    }

    pub async fn similar_artists(&self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        self.catalogue
            .get_or(format!("similar_artists:{artist_id}:{limit}"), async {
                self.session().await?.similar_artists(artist_id, limit).await
            })
            .await
    }

    // -------------------------------------------------------------- playback

    /// Never cached, the URL is signed and expires. Under the lock rather than
    /// on a session, because it is where the client learns which secret works.
    pub async fn file_url(&self, track_id: i64, format_id: u32) -> Result<String> {
        self.qobuz()?.lock().await.file_url(track_id, format_id).await
    }

    pub async fn export_playlist(&self, name: &str, track_ids: &[i64]) -> Result<i64> {
        let id = self.session().await?.export_playlist(name, track_ids).await?;
        self.account.clear();
        Ok(id)
    }

    // ------------------------------------------------------- favourites

    pub async fn favorite_add(&self, kind: &str, id: &str) -> Result<()> {
        self.session().await?.favorite_add(kind, id).await?;
        self.account.clear();
        Ok(())
    }

    pub async fn favorite_remove(&self, kind: &str, id: &str) -> Result<()> {
        self.session().await?.favorite_remove(kind, id).await?;
        self.account.clear();
        Ok(())
    }

    // --------------------------------------------------------- text steering

    /// Load the tower once, remembering that we tried. A missing export is not
    /// an error, it is the ordinary state of a fresh checkout.
    async fn encoder(&self) -> tokio::sync::MutexGuard<'_, Option<TextEncoder>> {
        let mut guard = self.text.lock().await;
        if !self.text_tried.swap(true, Ordering::SeqCst) {
            *guard = TextEncoder::load(&self.model_dir).unwrap_or(None);
        }
        guard
    }

    pub async fn embed(&self, phrase: &str) -> Result<Vec<f32>> {
        let mut guard = self.encoder().await;
        let encoder = guard.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "text steering needs {}/{}; fetch it with: stelly-server models",
                self.model_dir.display(),
                crate::pipeline::models::CLAP_TEXT
            )
        })?;
        encoder.embed(phrase)
    }

    pub async fn can_steer(&self) -> Result<bool> {
        Ok(self.encoder().await.is_some())
    }

    // ---------------------------------------------------------------- hiding

    pub async fn blocked_artists(&self) -> Result<Vec<BlockedArtist>> {
        db::blocked_artists(&self.db_path)
    }

    pub async fn block_artist(&self, artist_id: i64, name: &str) -> Result<()> {
        db::block_artist(&self.db_path, artist_id, name, None)?;
        self.refresh_blocked().await
    }

    pub async fn unblock_artist(&self, artist_id: i64) -> Result<()> {
        db::unblock_artist(&self.db_path, artist_id)?;
        self.refresh_blocked().await
    }

    /// Tell the loaded space, which filters on every read rather than being
    /// rebuilt.
    async fn refresh_blocked(&self) -> Result<()> {
        let blocked = db::blocked_artists(&self.db_path)?.into_iter().map(|a| a.artist_id).collect();
        let space = self.space.clone();
        tokio::task::spawn_blocking(move || space.set_blocked(blocked))
            .await
            .context("the worker thread panicked")
    }

    // ------------------------------------------- extending the catalogue

    pub async fn corpus(&self) -> Result<Corpus> {
        stages::corpus(&self.db_path)
    }

    // -------------------------------------------------------------- crawling

    /// Start a crawl on a thread of its own.
    ///
    /// Not `tokio::spawn`: `crawl::step` holds its SQLite connection across
    /// awaits and blocks on it; see `off_thread`. Its own client, but the same
    /// `RateLimit`, the account is what 2/s protects, not the socket.
    pub async fn crawl_start(&self, max_distance: i64) -> Result<()> {
        if self.crawl.status.lock().unwrap().running {
            return Ok(());
        }

        // Built here, on purpose: a missing .env should fail the button press
        // rather than a thread nobody is watching.
        let limit = self.qobuz()?.lock().await.rate_limit();
        let env_dir = self.env_dir.clone();
        let db_path = self.db_path.clone();
        let job = self.crawl.clone();

        job.stop.store(false, Ordering::SeqCst);
        *job.status.lock().unwrap() = CrawlStatus {
            running: true,
            last: Some("starting…".into()),
            ..Default::default()
        };

        std::thread::Builder::new()
            .name("stelly-crawl".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let mut status = job.status.lock().unwrap();
                        status.last = Some(format!("could not start the crawl: {err}"));
                        status.running = false;
                        return;
                    }
                };

                runtime.block_on(async {
                    if let Err(err) = crawl_loop(&job, &env_dir, limit, &db_path, max_distance).await
                    {
                        job.status.lock().unwrap().last = Some(format!("{err:#}"));
                    }
                });

                job.status.lock().unwrap().running = false;
            })
            .context("spawning the crawl thread")?;

        Ok(())
    }

    pub async fn crawl_stop(&self) -> Result<()> {
        self.crawl.stop.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn crawl_status(&self) -> Result<CrawlStatus> {
        Ok(self.crawl.status.lock().unwrap().clone())
    }

    // -------------------------------------------------------------- pipeline

    pub async fn pipeline_start(&self, stage: Stage) -> Result<()> {
        if stage == Stage::Crawl {
            return self.crawl_start(stelly_core::api::DEFAULT_MAX_DISTANCE).await;
        }
        {
            let mut queue = self.pipeline.queue.lock().unwrap();
            if queue.running.is_some() {
                return Ok(());
            }
            queue.stages.push_back(stage);
        }
        self.drain().await
    }

    pub async fn pipeline_start_full(&self) -> Result<()> {
        {
            let mut queue = self.pipeline.queue.lock().unwrap();
            if queue.running.is_some() {
                return Ok(());
            }
            queue.stages.extend(FULL_RUN);
        }
        self.drain().await
    }

    /// Put one track, album or artist on the map, ahead of everything else.
    /// A backlog analyse in progress is interrupted and resumes once the
    /// target is laid out; anything else finishes first.
    pub async fn pipeline_add(&self, target: Target) -> Result<()> {
        {
            let mut queue = self.pipeline.queue.lock().unwrap();
            if queue.adding.contains(&target) {
                return Ok(());
            }
            if queue.running == Some(Stage::Analyse) && queue.target.is_none() {
                self.log.push(format!("pausing the backlog for {target}"));
                queue.stages.push_front(Stage::Analyse);
                self.pipeline.preempted.store(true, Ordering::SeqCst);
                self.pipeline.cancel.store(true, Ordering::SeqCst);
            }
            queue.targets.push_back(target.clone());
            queue.adding.push(target);
            queue.rebuild_next();
        }
        self.drain().await
    }

    /// Run the queue until it empties, unless it already is being run.
    async fn drain(&self) -> Result<()> {
        let first = {
            let mut queue = self.pipeline.queue.lock().unwrap();
            if queue.running.is_some() {
                return Ok(());
            }
            let Some((stage, target)) = queue.next() else {
                return Ok(());
            };
            queue.running = Some(stage);
            queue.target = target.clone();
            (stage, target)
        };

        // The account's budget, for analyse to share with browsing. Only
        // analyse talks to Qobuz: without credentials build-space and layout
        // still run, and analyse says what is missing in the log.
        let limit = match self.qobuz() {
            Ok(client) => client.lock().await.rate_limit(),
            Err(_) => crate::qobuz::RateLimit::new(),
        };
        let job = self.pipeline.clone();
        let log = self.log.clone();
        let paths = self.paths.clone();
        let space = self.space.clone();

        job.cancel.store(false, Ordering::SeqCst);

        tokio::spawn(async move {
            // A loop rather than recursion: the next step is just the next
            // stage, and boxing a recursive future to say so would be worse.
            let mut current = Some(first);

            while let Some((stage, target)) = current {
                match &target {
                    Some(target) => log.push(format!("$ stelly-server {} {}", stage.command(), target.flag())),
                    None => log.push(format!("$ stelly-server {}", stage.command())),
                }

                let sink = log.clone();
                *job.progress.lock().unwrap() = None;
                let reporter = Job::new(move |line| sink.push(line), job.cancel.clone())
                    .reporting(job.progress.clone());
                let (stage_paths, limit, scope) = (paths.clone(), limit.clone(), target.clone());
                let outcome = off_thread(move || async move {
                    stages::run(stage, scope.as_ref(), &stage_paths, limit, &reporter).await
                })
                .await;

                // Build-space is nearly always followed by layout, and telling
                // clients after both would have them fetch the space twice for
                // one rebuild, the first time without its new positions. So
                // only the last of a run of rebuilds moves the generation.
                let layout_next = stage == Stage::BuildSpace
                    && (job.preempted.load(Ordering::SeqCst) || !job.cancel.load(Ordering::SeqCst))
                    && job.queue.lock().unwrap().stages.contains(&Stage::Layout);
                let settled = stage.rebuilds_space() && !layout_next;

                // Before the generation moves, not after: a client refetches
                // what it caches as soon as it sees the new one.
                if settled {
                    let space = space.clone();
                    let loaded = tokio::task::spawn_blocking(move || space.reload())
                        .await
                        .context("the worker thread panicked")
                        .and_then(|loaded| loaded);
                    if let Err(err) = loaded {
                        log.push(format!("could not load the rebuilt space: {err:#}"));
                    }
                }

                let mut queue = job.queue.lock().unwrap();
                let preempted = job.preempted.swap(false, Ordering::SeqCst);
                let cancelled = !preempted && job.cancel.load(Ordering::SeqCst);

                match &outcome {
                    _ if preempted => log.push(format!("{} paused", stage.label())),
                    _ if cancelled => log.push("cancelled"),
                    Ok(()) => log.push(format!("{} finished", stage.label())),
                    Err(err) => log.push(format!("{} failed: {err:#}", stage.label())),
                }

                if settled {
                    job.generation.fetch_add(1, Ordering::SeqCst);
                }

                // A target that failed will not reach the map; one that
                // analysed has, once a layout after it is done.
                if let (Some(target), Err(_)) = (&target, &outcome) {
                    queue.adding.retain(|t| t != target);
                }
                if stage == Stage::Layout {
                    let waiting = queue.targets.clone();
                    queue.adding.retain(|t| waiting.contains(t));
                }

                if preempted {
                    job.cancel.store(false, Ordering::SeqCst);
                }
                *job.progress.lock().unwrap() = None;
                current = if cancelled { None } else { queue.next() };
                queue.running = current.as_ref().map(|(stage, _)| *stage);
                queue.target = current.as_ref().and_then(|(_, target)| target.clone());
            }
        });

        Ok(())
    }

    pub async fn pipeline_stop(&self) -> Result<()> {
        {
            let mut queue = self.pipeline.queue.lock().unwrap();
            queue.stages.clear();
            queue.targets.clear();
            queue.adding.clear();
        }
        self.pipeline.preempted.store(false, Ordering::SeqCst);
        self.pipeline.cancel.store(true, Ordering::SeqCst);
        self.crawl.stop.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub async fn pipeline_status(&self) -> Result<PipelineStatus> {
        let queue = self.pipeline.queue.lock().unwrap();
        Ok(PipelineStatus {
            running: queue.running,
            queued: queue.stages.iter().copied().collect(),
            generation: self.pipeline.generation.load(Ordering::SeqCst),
            target: queue.target.clone(),
            adding: queue.adding.clone(),
            waiting: queue.targets.iter().cloned().collect(),
            progress: progress_of(&self.pipeline.progress),
        })
    }

    pub async fn pipeline_log(&self) -> Result<Vec<String>> {
        Ok(self.log.all())
    }

    /// Incremental read, for the server's SSE handler. Views use
    /// `pipeline_log`; this exists so a stream can push only what is new.
    pub async fn pipeline_log_since(&self, since: u64) -> Result<LogSlice> {
        Ok(self.log.since(since))
    }

    pub async fn pipeline_clear_log(&self) -> Result<()> {
        self.log.clear();
        Ok(())
    }
}

// --------------------------------------------------------------- off-thread
//
// Crawl and analyse futures hold a SQLite connection across awaits and make
// blocking calls on it, which a worker of the shared runtime must not do.
// Rather than restructure them, the work moves to a thread with its own
// runtime. The client is separate but the budget is shared, see
// `qobuz::RateLimit`.

/// A connection and a client, both belonging to the calling thread.
fn worker_parts(
    env_dir: &Path,
    db_path: &Path,
    limit: crate::qobuz::RateLimit,
) -> Result<(diesel::SqliteConnection, QobuzClient)> {
    let conn = db::open_for_write(db_path).context("opening the database")?;
    let credentials = crate::qobuz::Credentials::from_env(env_dir)?;
    Ok((conn, QobuzClient::sharing(credentials, limit)))
}

/// Drive a `!Send` future to completion somewhere it is allowed to live. `job`
/// is `Send`; the future it produces need not be.
async fn off_thread<T, F, Fut>(job: F) -> Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>>,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building a runtime for the worker thread")?;
        runtime.block_on(job())
    })
    .await
    .context("the worker thread panicked")?
}

// ------------------------------------------------------------- the crawl loop

/// Run the frontier until it empties, the budget is hit, or stop is asked. The
/// Qobuz client is released around each step, so search and playback never wait
/// longer than one item.
async fn crawl_loop(
    job: &CrawlJob,
    env_dir: &Path,
    limit: crate::qobuz::RateLimit,
    db_path: &Path,
    max_distance: i64,
) -> Result<()> {
    // This thread's own connection and client, but not its own budget, both
    // halves talk to one account. See `qobuz::RateLimit`.
    let (mut conn, mut client) = worker_parts(env_dir, db_path, limit)?;

    // No budget: the frontier and the stop button are the limits.
    let max_tracks = i64::MAX;
    let mut stats = crawl::Stats::default();

    loop {
        if job.stop.load(Ordering::SeqCst) {
            job.status.lock().unwrap().last = Some("stopped".into());
            break;
        }

        let result = crawl::step(&mut conn, &mut client, max_tracks, max_distance).await?;

        let line = match &result {
            crawl::StepResult::Expanded { kind, ref_id } => format!("{kind} {ref_id}"),
            crawl::StepResult::Skipped { kind, ref_id } => format!("skipped {kind} {ref_id}"),
            crawl::StepResult::Failed { kind, ref_id, error } => {
                format!("{kind} {ref_id}: {error}")
            }
            crawl::StepResult::Exhausted => "frontier exhausted".into(),
            crawl::StepResult::BudgetReached => "budget reached".into(),
        };

        let keep_going = stats.absorb(&result);

        {
            let mut status = job.status.lock().unwrap();
            status.last = Some(line);
            status.artists_expanded = stats.artists_expanded;
            status.albums_expanded = stats.albums_expanded;
            status.tracks_added = stats.tracks_added;
            status.errors = stats.errors;
            status.blocked_skipped = stats.blocked_skipped;
            status.tracks = tracks::table.count().get_result(&mut conn).unwrap_or(0);
            status.pending = frontier::table
                .filter(frontier::state.eq("pending"))
                .count()
                .get_result(&mut conn)
                .unwrap_or(0);
        }

        if !keep_going {
            break;
        }
    }

    Ok(())
}
