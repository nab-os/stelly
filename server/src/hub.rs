//! Everything the server does on its own disk and account: the music
//! provider, the block list, the crawl loop and the pipeline driver. The routes are a thin skin
//! over this.
//!
//! The crawl and the stages publish a status that clients poll, because a
//! server cannot write a client's signals.

use crate::cache::Cache;
use crate::pipeline::{progress_of, Job, Paths, ProgressSlot};
use crate::provider::{Capabilities, Provider, Quality, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use crate::schema::{frontier, tracks};
use crate::stages;
use crate::text::TextEncoder;
use crate::space::{SpaceService, Vocabulary};
use crate::likes;
use crate::{crawl, db};
use anyhow::{Context, Result};
use diesel::{ExpressionMethods, QueryDsl, RunQueryDsl};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use stelly_core::api::{
    BlockedArtist, Corpus, CrawlStatus, LogSlice, PipelineStatus, Stage, Target, FULL_RUN,
};
use stelly_core::logbuffer::LogBuffer;

/// The releases out in the last `days`, up to today, newest first.
fn released_within(albums: Vec<RemoteAlbum>, days: u32) -> Vec<RemoteAlbum> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let since = db::utc_at(now.saturating_sub(u64::from(days) * 86_400));
    let until = db::utc_at(now);
    stelly_core::catalogue::recent_releases(albums, &since[..10], &until[..10])
}

/// Every liked artist counts towards recent releases, however many.
const LIKED_ARTISTS_CAP: usize = 10_000;
/// Each artist's newest few: enough to reach back months for all but the
/// busiest, in one request.
const RECENT_PER_ARTIST: usize = 20;

pub struct Hub {
    paths: Paths,
    env_dir: PathBuf,
    db_path: PathBuf,
    model_dir: PathBuf,
    /// Built on first use. The provider holds the account; without it,
    /// browsing the space still works, and only reaching for it fails.
    client: OnceLock<Provider>,
    /// Albums, artists, discographies: they change on the scale of weeks.
    catalogue: Cache,
    /// Short, since the same words find new releases.
    searches: Cache,
    /// Playlists, which the account can change from anywhere else.
    /// Cleared on our own writes, and short for everyone else's.
    account: Cache,
    /// Each liked artist's newest releases. Outlives the background check's
    /// interval, which overwrites it, so a client never waits on the provider for an
    /// artist that check has seen.
    releases: Cache,
    /// Loaded on first use and kept. `None` inside means the tower has not
    /// been exported, which is not an error, the UI hides steering.
    text: tokio::sync::Mutex<Option<TextEncoder>>,
    text_tried: AtomicBool,
    /// The describing words, embedded on first use and kept.
    vocabulary: tokio::sync::Mutex<Option<Arc<Vocabulary>>>,
    crawl: Arc<CrawlJob>,
    pipeline: Arc<PipelineJob>,
    log: Arc<LogBuffer>,
    /// Whole tracks analysed for mixing, and the job that adds to them.
    mix: Arc<crate::mix::MixService>,
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
        let log = Arc::new(LogBuffer::default());
        Self {
            mix: Arc::new(crate::mix::MixService::new(paths.clone(), log.clone())),
            space: Arc::new(SpaceService::new(paths.data_dir.clone(), paths.db_path.clone())),
            env_dir: paths.env_dir.clone(),
            db_path: paths.db_path.clone(),
            model_dir: paths.model_dir.clone(),
            paths,
            client: OnceLock::new(),
            catalogue: Cache::new(Duration::from_secs(6 * 60 * 60), 5_000),
            searches: Cache::new(Duration::from_secs(5 * 60), 500),
            account: Cache::new(Duration::from_secs(2 * 60), 200),
            releases: Cache::new(Duration::from_secs(48 * 60 * 60), 10_000),
            text: tokio::sync::Mutex::new(None),
            text_tried: AtomicBool::new(false),
            vocabulary: tokio::sync::Mutex::new(None),
            crawl: Arc::new(CrawlJob::default()),
            pipeline: Arc::new(PipelineJob::default()),
            log,
        }
    }

    pub fn mix(&self) -> &Arc<crate::mix::MixService> {
        &self.mix
    }

    pub fn space(&self) -> &Arc<SpaceService> {
        &self.space
    }

    /// The provider's client, built on first reach. Clones share it; the
    /// provider queues every device's requests on the account's one budget.
    fn provider(&self) -> Provider {
        self.client.get_or_init(|| Provider::from_env(&self.env_dir, &self.db_path)).clone()
    }

    /// The client for a device's reads, which queue as interactive.
    async fn session(&self) -> Result<Provider> {
        Ok(self.provider())
    }

    // -------------------------------------------------------------- browsing

    /// Convert what an earlier Stelly stored in the provider's own shapes,
    /// in the background: a few minutes on a large catalogue, once. Until it
    /// is done, likes from before it are missing and the space has fewer
    /// covers. On a thread of its own, since it holds the database across
    /// requests to the provider.
    pub fn import_legacy(self: &Arc<Self>) {
        let hub = self.clone();
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            let report = |line: String| hub.log.push(line);
            match runtime.block_on(crate::legacy::run(&hub.db_path, &hub.provider().bulk(), &report)) {
                Ok(done) if done.converted > 0 => {
                    hub.log.push(format!("imported {} stored items, {} dropped", done.converted, done.dropped));
                    if let Err(err) = hub.space.reload() {
                        hub.log.push(format!("reloading the space after the import failed: {err:#}"));
                    }
                }
                Ok(_) => {}
                Err(err) => hub.log.push(format!("importing stored items failed, retried at the next start: {err:#}")),
            }
        });
    }

    /// What the provider is and can do; it does not change while it runs.
    pub async fn capabilities(&self) -> Result<Capabilities> {
        self.catalogue
            .get_or("capabilities".to_string(), async { self.provider().capabilities().await })
            .await
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults> {
        let key = format!("search:{limit}:{query}");
        self.searches
            .get_or(key, async { self.session().await?.search(query, limit).await })
            .await
    }

    /// One person's likes, from the database alone. Named favourites on the
    /// wire, which is what they were when they were the account's.
    pub async fn favourite_tracks(&self, user_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        Ok(likes::list(&self.db_path, user_id, "track", cap)?
            .into_iter()
            .map(|(mut track, liked_at): (RemoteTrack, i64)| {
                track.liked_at = Some(liked_at);
                track
            })
            .collect())
    }

    pub async fn favourite_albums(&self, user_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        Ok(likes::list(&self.db_path, user_id, "album", cap)?
            .into_iter()
            .map(|(mut album, liked_at): (RemoteAlbum, i64)| {
                album.liked_at = Some(liked_at);
                album
            })
            .collect())
    }

    pub async fn favourite_artists(&self, user_id: i64, cap: usize) -> Result<Vec<RemoteArtist>> {
        Ok(likes::list(&self.db_path, user_id, "artist", cap)?
            .into_iter()
            .map(|(mut artist, liked_at): (RemoteArtist, i64)| {
                artist.liked_at = Some(liked_at);
                artist
            })
            .collect())
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
                self.session().await?.artist_albums(artist_id, cap).await
            })
            .await
    }

    /// What the artists this user likes have put out in the last `days`,
    /// newest first.
    ///
    /// One request per liked artist, cached per artist rather than per user,
    /// so a family sharing artists shares the spend, and liking one more
    /// costs one request rather than all of them again. An artist that fails
    /// is left out rather than failing the list.
    pub async fn recent_releases(&self, user_id: i64, days: u32) -> Result<Vec<RemoteAlbum>> {
        let artists = self
            .favourite_artists(user_id, LIKED_ARTISTS_CAP)
            .await?
            .into_iter()
            .map(|artist| artist.id)
            .collect();
        let albums = self.artists_recent(self.unblocked(artists)?, false).await;
        Ok(released_within(albums, days))
    }

    /// Every liked artist's newest releases, fetched again whatever is
    /// cached, so the next client to ask is answered from memory. Then,
    /// unless `add_days` is 0, whatever came out in the last `add_days` and
    /// is not in the space yet is added to it.
    pub async fn check_releases(&self, add_days: u32) -> Result<()> {
        let liked = likes::family(&mut db::connect(&self.db_path, Duration::from_secs(5))?)?;
        let artists = liked
            .into_iter()
            .filter_map(|liked| match liked {
                likes::Liked::Artist(artist) => Some(artist.id),
                _ => None,
            })
            .collect();
        let artists = self.unblocked(artists)?;
        let checked = artists.len();
        let albums = self.artists_recent(artists, true).await;
        self.log.push(format!("checked {checked} liked artists for new releases"));
        if add_days == 0 {
            return Ok(());
        }

        let mut conn = db::connect(&self.db_path, Duration::from_secs(5))?;
        for album in released_within(albums, add_days) {
            if db::album_known(&mut conn, &album.id)? {
                continue;
            }
            self.log.push(format!("adding new release {} by {} to the space", album.title, album.artist));
            self.pipeline_add(Target::Album(album.id)).await?;
        }
        Ok(())
    }

    /// `check_releases` every `every`, for the life of the server. The first
    /// check waits a minute, for the space to load and the first requests to
    /// go out ahead of it.
    pub async fn watch_releases(self: Arc<Self>, every: Duration, add_days: u32) {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if let Err(err) = self.check_releases(add_days).await {
                self.log.push(format!("new releases check failed: {err:#}"));
            }
            tokio::time::sleep(every).await;
        }
    }

    fn unblocked(&self, artists: Vec<i64>) -> Result<Vec<i64>> {
        let blocked: std::collections::HashSet<i64> =
            db::blocked_artists(&self.db_path)?.into_iter().map(|a| a.artist_id).collect();
        Ok(artists.into_iter().filter(|id| !blocked.contains(id)).collect())
    }

    /// These artists' newest releases, from memory unless `fresh` or never
    /// fetched. An artist that fails is left out rather than failing them all.
    async fn artists_recent(&self, artists: Vec<i64>, fresh: bool) -> Vec<RemoteAlbum> {
        use futures_util::StreamExt;

        let fetch = |id: i64| async move {
            self.session().await?.artist_recent_releases(id, RECENT_PER_ARTIST).await
        };
        // The bucket paces the requests; this only keeps it from idling
        // between them.
        futures_util::stream::iter(artists)
            .map(|id| async move {
                let key = format!("artist_recent:{id}");
                let fetched = if fresh {
                    match fetch(id).await {
                        Ok(albums) => {
                            self.releases.put(key, albums.clone()).await;
                            Ok(albums)
                        }
                        Err(err) => Err(err),
                    }
                } else {
                    self.releases.get_or(key, fetch(id)).await
                };
                fetched.unwrap_or_else(|err| {
                    eprintln!("recent releases of artist {id} failed: {err:#}");
                    Vec::new()
                })
            })
            .buffer_unordered(4)
            .flat_map(futures_util::stream::iter)
            .collect()
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

    /// Never cached, the URL is signed and expires.
    pub async fn file_url(&self, track_id: i64, quality: Quality) -> Result<String> {
        self.provider().file_url(track_id, quality).await
    }

    /// Under the name of whoever made it, so the account's playlist list
    /// stays readable with the whole family writing to it.
    pub async fn export_playlist(&self, user: &str, name: &str, track_ids: &[i64]) -> Result<i64> {
        let name = format!("{user} · {name}");
        let id = self.session().await?.export_playlist(&name, track_ids).await?;
        self.account.clear();
        Ok(id)
    }

    // ------------------------------------------------------------ likes

    /// Fetch the item once, so the like can be listed and crawled without
    /// asking the provider again. An album's tracklist is left out: the
    /// crawl fetches it when it expands the album.
    pub async fn favorite_add(&self, kind: &str, id: &str, user_id: i64) -> Result<()> {
        let item = match kind {
            "track" => {
                let track_id = id.parse().with_context(|| format!("“{id}” is not a track id"))?;
                likes::Liked::Track(self.session().await?.track(track_id).await?)
            }
            "album" => likes::Liked::Album(self.session().await?.album(id).await?.album),
            "artist" => {
                let artist_id = id.parse().with_context(|| format!("“{id}” is not an artist id"))?;
                likes::Liked::Artist(self.session().await?.artist(artist_id).await?)
            }
            other => anyhow::bail!("unknown kind of like {other:?}"),
        };
        likes::record(&self.db_path, user_id, id, &item, likes::now()).map(|_| ())
    }

    pub async fn favorite_remove(&self, kind: &str, id: &str, user_id: i64) -> Result<()> {
        likes::forget(&self.db_path, user_id, kind, id)
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

    /// Every word a track can be described with, embedded. The first call
    /// embeds them all, a few seconds; after that it is free.
    pub async fn vocabulary(&self) -> Result<Arc<Vocabulary>> {
        let mut held = self.vocabulary.lock().await;
        if let Some(vocabulary) = held.as_ref() {
            return Ok(vocabulary.clone());
        }
        let mut vocabulary = Vocabulary::default();
        for (word, phrase) in crate::pipeline::labels::vocabulary() {
            vocabulary.vectors.push(self.embed(&phrase).await?);
            vocabulary.words.push(word);
        }
        let vocabulary = Arc::new(vocabulary);
        *held = Some(vocabulary.clone());
        Ok(vocabulary)
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
    /// awaits and blocks on it; see `off_thread`. Its requests queue as bulk
    /// at the gateway, behind browsing.
    pub async fn crawl_start(&self, max_distance: i64) -> Result<()> {
        if self.crawl.status.lock().unwrap().running {
            return Ok(());
        }

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
                    if let Err(err) = crawl_loop(&job, &env_dir, &db_path, max_distance).await
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
                let (stage_paths, scope) = (paths.clone(), target.clone());
                let outcome = off_thread(move || async move {
                    stages::run(stage, scope.as_ref(), &stage_paths, &reporter).await
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
// runtime.

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

/// Run the frontier until it empties, the budget is hit, or stop is asked.
/// Its requests queue as bulk, so search and playback go ahead of it at the
/// gateway.
async fn crawl_loop(job: &CrawlJob, env_dir: &Path, db_path: &Path, max_distance: i64) -> Result<()> {
    // This thread's own connection and client: a client's connections belong
    // to the runtime that made them.
    let mut conn = db::open_for_write(db_path).context("opening the database")?;
    let mut client = Provider::from_env(env_dir, db_path).bulk();

    // From the database alone, so likes made since the last crawl are in
    // the frontier before it carries on.
    let seeded = crawl::seed(&mut conn)?;
    job.status.lock().unwrap().last = Some(format!(
        "seeded {} tracks, {} albums, {} artists from the family's likes",
        seeded.tracks_added, seeded.albums_expanded, seeded.artists_expanded
    ));

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
