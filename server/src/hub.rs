//! Everything the server does on its own disk and account: the music
//! provider, the block list, the crawl loop and the pipeline driver. The routes are a thin skin
//! over this.
//!
//! The crawl and the stages publish a status that clients poll, because a
//! server cannot write a client's signals.

use crate::cache::{Cache, Revalidating};
use crate::pipeline::{progress_of, Job, Paths, ProgressSlot};
use crate::provider::{Capabilities, Provider, Quality, Timings, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
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
use std::time::{Duration, Instant};
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
/// How long the corpus counts are served from memory. They take seconds on a
/// large database and every app polls them; a stage ending clears them.
const CORPUS_TTL: Duration = Duration::from_secs(15);

/// Phrases whose embedding is kept, 2 KB each.
const EMBEDDINGS_HELD: u64 = 1_000;

pub struct Hub {
    paths: Paths,
    env_dir: PathBuf,
    db_path: PathBuf,
    /// What requests read and write the database through.
    db: Arc<db::Db>,
    model_dir: PathBuf,
    /// Built on first use. The provider holds the account; without it,
    /// browsing the space still works, and only reaching for it fails.
    client: OnceLock<Provider>,
    /// Short, since the same words find new releases.
    searches: Cache,
    /// Playlists, which the account can change from anywhere else.
    /// Cleared on our own writes, and fetched again after two minutes for
    /// everyone else's, behind the one held.
    account: Revalidating,
    /// Each liked artist's newest releases. Outlives the background check's
    /// interval, which overwrites it, so a client never waits on the provider for an
    /// artist that check has seen.
    releases: Cache,
    /// Loaded at start when it is on disk, else on first use, and kept.
    /// `None` inside means the tower has not been exported, which is not an
    /// error, the UI hides steering.
    text: tokio::sync::OnceCell<Option<Arc<Mutex<TextEncoder>>>>,
    /// Whether steering is on offer, answered without waiting for the load:
    /// set when the export is on disk, cleared if it then fails to load.
    steerable: AtomicBool,
    /// Phrases already embedded. The same few come back on every drift and
    /// mood regenerated, and mixing's baseline on every set.
    embeddings: moka::future::Cache<String, Arc<Vec<f32>>>,
    /// The describing words, embedded once the tower has loaded and kept.
    vocabulary: tokio::sync::OnceCell<Arc<Vocabulary>>,
    crawl: Arc<CrawlJob>,
    pipeline: Arc<PipelineJob>,
    corpus: Arc<CorpusCounts>,
    log: Arc<LogBuffer>,
    /// Whole tracks analysed for mixing, and the job that adds to them.
    mix: Arc<crate::mix::MixService>,
    /// The loaded space, which every client's navigation runs against.
    space: Arc<SpaceService>,
}

/// The space rebuilt for the mix queue's tracks at most this often. A
/// rebuild of 160,000 tracks is about 75s on 32 cores, then every client
/// fetches the space again.
const MIX_REBUILD_EVERY: Duration = Duration::from_secs(30 * 60);

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

/// The last corpus counts and when they were taken. The lock is held while
/// they are taken, so callers arriving meanwhile wait for that one answer
/// rather than each running the counts again.
#[derive(Default)]
struct CorpusCounts {
    held: tokio::sync::Mutex<Option<(Instant, u64, Corpus)>>,
    /// Moved by whatever changes the counts. Counts taken before it moved
    /// are not served after, even when they finish later.
    epoch: AtomicU64,
}

impl CorpusCounts {
    fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
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

    /// Make dedupe, build-space then layout the next stages, so what the
    /// targets add reaches the map before the backlog carries on.
    fn rebuild_next(&mut self) {
        match self.stages.front() {
            Some(Stage::Dedupe) => {}
            Some(Stage::BuildSpace) => self.stages.push_front(Stage::Dedupe),
            Some(Stage::Layout) => {
                self.stages.push_front(Stage::BuildSpace);
                self.stages.push_front(Stage::Dedupe);
            }
            _ => {
                self.stages.push_front(Stage::Layout);
                self.stages.push_front(Stage::BuildSpace);
                self.stages.push_front(Stage::Dedupe);
            }
        }
    }
}

impl Hub {
    pub fn new(paths: Paths) -> Self {
        let log = Arc::new(LogBuffer::default());
        let steerable = TextEncoder::exported(&paths.model_dir);
        let space = Arc::new(SpaceService::new(paths.data_dir.clone(), paths.db_path.clone()));
        // Albums, artists, discographies: they change on the scale of weeks,
        // so one past its six hours is served while it is fetched again.
        let catalogue = Revalidating::new(Duration::from_secs(6 * 60 * 60), Duration::from_secs(7 * 24 * 60 * 60), 5_000);
        crate::provider::share_catalogue(&paths.db_path, catalogue);
        Self {
            mix: Arc::new(crate::mix::MixService::new(paths.clone(), space.clone(), log.clone())),
            space,
            env_dir: paths.env_dir.clone(),
            db_path: paths.db_path.clone(),
            db: db::Db::shared(&paths.db_path),
            model_dir: paths.model_dir.clone(),
            paths,
            client: OnceLock::new(),
            searches: Cache::new(Duration::from_secs(5 * 60), 500),
            account: Revalidating::new(Duration::from_secs(2 * 60), Duration::from_secs(60 * 60), 200),
            releases: Cache::new(Duration::from_secs(48 * 60 * 60), 10_000),
            text: tokio::sync::OnceCell::new(),
            steerable: AtomicBool::new(steerable),
            embeddings: moka::future::Cache::new(EMBEDDINGS_HELD),
            vocabulary: tokio::sync::OnceCell::new(),
            crawl: Arc::new(CrawlJob::default()),
            pipeline: Arc::new(PipelineJob::default()),
            corpus: Arc::new(CorpusCounts::default()),
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

    /// What the providers are and can do, from what each said once. Not
    /// cached here: one that was down is counted once it has said.
    pub async fn capabilities(&self) -> Result<Capabilities> {
        self.provider().capabilities().await
    }

    /// `only` names the providers to ask; none is all of them, and
    /// `session` the search box it came from. Where the time went goes in
    /// `timings`; a search the cache answered, or another request already
    /// asking, has only that wait.
    pub async fn search(&self, query: &str, limit: usize, only: &[String], session: &str, timings: &Timings) -> Result<SearchResults> {
        let key = format!("search:{limit}:{}:{query}", only.join(","));
        let started = Instant::now();
        let asked = AtomicBool::new(false);
        let ask = || async {
            asked.store(true, Ordering::Relaxed);
            self.session().await?.search(query, limit, only, session, timings).await
        };
        let mut found = self.searches.get_or(key.clone(), ask()).await;
        // Another device's search of the same query, which failed: perhaps
        // only because a newer one from that device replaced it, which says
        // nothing of this one. Asked once more, in this one's own name.
        if found.is_err() && !asked.load(Ordering::Relaxed) {
            timings.add("cache", started.elapsed());
            found = self.searches.get_or(key, ask()).await;
        }
        if timings.is_empty() {
            timings.add("cache", started.elapsed());
        }
        found
    }

    /// Each provider, most preferred first: what it is called and can do.
    pub async fn providers(&self) -> Result<Vec<Capabilities>> {
        self.provider().each_capabilities().await
    }

    /// One person's likes, from the database alone. Named favourites on the
    /// wire, which is what they were when they were the account's.
    pub async fn favourite_tracks(&self, user_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        Ok(self
            .db
            .read(move |conn| likes::list(conn, user_id, "track", cap))
            .await?
            .into_iter()
            .map(|(mut track, liked_at): (RemoteTrack, i64)| {
                track.liked_at = Some(liked_at);
                track
            })
            .collect())
    }

    pub async fn favourite_albums(&self, user_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        Ok(self
            .db
            .read(move |conn| likes::list(conn, user_id, "album", cap))
            .await?
            .into_iter()
            .map(|(mut album, liked_at): (RemoteAlbum, i64)| {
                album.liked_at = Some(liked_at);
                album
            })
            .collect())
    }

    pub async fn favourite_artists(&self, user_id: i64, cap: usize) -> Result<Vec<RemoteArtist>> {
        Ok(self
            .db
            .read(move |conn| likes::list(conn, user_id, "artist", cap))
            .await?
            .into_iter()
            .map(|(mut artist, liked_at): (RemoteArtist, i64)| {
                artist.liked_at = Some(liked_at);
                artist
            })
            .collect())
    }

    pub async fn playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        self.account
            .get_or(format!("playlists:{cap}"), {
                let provider = self.provider();
                async move { provider.user_playlists(cap).await }
            })
            .await
    }

    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.account
            .get_or(format!("playlist_tracks:{playlist_id}:{cap}"), {
                let provider = self.provider();
                async move { provider.playlist_tracks(playlist_id, cap).await }
            })
            .await
    }

    // Albums, artists, discographies and similar artists are cached by the
    // provider, in the catalogue cache it shares with the crawl and stages.
    // Where each one's time went goes in `timings`.

    pub async fn album_tracks(&self, album_id: &str, timings: &Timings) -> Result<Vec<RemoteTrack>> {
        Ok(self.session().await?.album_tracks(album_id, timings).await?.1)
    }

    pub async fn artist(&self, artist_id: i64, timings: &Timings) -> Result<RemoteArtist> {
        self.session().await?.artist(artist_id, timings).await
    }

    pub async fn artist_albums(&self, artist_id: i64, cap: usize, timings: &Timings) -> Result<Vec<RemoteAlbum>> {
        self.session().await?.artist_albums(artist_id, cap, timings).await
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
        let albums = self.artists_recent(self.unblocked(artists).await?, false).await;
        Ok(released_within(albums, days))
    }

    /// Every liked artist's newest releases, fetched again whatever is
    /// cached, so the next client to ask is answered from memory. Then,
    /// unless `add_days` is 0, whatever came out in the last `add_days` and
    /// is not in the space yet is added to it.
    pub async fn check_releases(&self, add_days: u32) -> Result<()> {
        let liked = self.db.read(|conn| likes::family(conn)).await?;
        let artists = liked
            .into_iter()
            .filter_map(|liked| match liked {
                likes::Liked::Artist(artist) => Some(artist.id),
                _ => None,
            })
            .collect();
        let artists = self.unblocked(artists).await?;
        let checked = artists.len();
        let albums = self.artists_recent(artists, true).await;
        self.log.push(format!("checked {checked} liked artists for new releases"));
        if add_days == 0 {
            return Ok(());
        }

        // One trip off the async threads for all of them, not a blocking
        // query an album on one.
        let fresh = self
            .db
            .read(move |conn| {
                let mut fresh = Vec::new();
                for album in released_within(albums, add_days) {
                    if !db::album_known(conn, &album.id)? {
                        fresh.push(album);
                    }
                }
                Ok(fresh)
            })
            .await?;
        for album in fresh {
            self.log.push(format!("adding new release {} by {} to the space", album.title, album.artist));
            self.pipeline_add(Target::Album(album.id)).await?;
        }
        Ok(())
    }

    /// Look up the space's tracks' lyrics on LRCLIB, for the language they
    /// are sung in, for the life of the server. See `lyrics`.
    pub async fn watch_lyrics(self: Arc<Self>) {
        crate::lyrics::watch(self.db_path.clone(), self.log.clone()).await
    }

    /// What is known of a track's lyrics.
    pub async fn lyrics(&self, id: i64) -> Result<stelly_core::api::Lyrics> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || crate::lyrics::of(&db_path, id)).await?
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

    async fn unblocked(&self, artists: Vec<i64>) -> Result<Vec<i64>> {
        let blocked: std::collections::HashSet<i64> =
            self.blocked_artists().await?.into_iter().map(|a| a.artist_id).collect();
        Ok(artists.into_iter().filter(|id| !blocked.contains(id)).collect())
    }

    /// These artists' newest releases, from memory unless `fresh` or never
    /// fetched. An artist that fails is left out rather than failing them all.
    ///
    /// `fresh` is the watcher, which no one waits on, so its requests queue
    /// as bulk and an album opened meanwhile goes ahead of them. A list
    /// someone asked for and is not cached yet stays interactive.
    async fn artists_recent(&self, artists: Vec<i64>, fresh: bool) -> Vec<RemoteAlbum> {
        use futures_util::StreamExt;

        let fetch = |id: i64| async move {
            let client = if fresh { self.provider().bulk() } else { self.session().await? };
            client.artist_recent_releases(id, RECENT_PER_ARTIST).await
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

    pub async fn similar_artists(&self, artist_id: i64, limit: usize, timings: &Timings) -> Result<Vec<RemoteArtist>> {
        self.session().await?.similar_artists(artist_id, limit, timings).await
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
        self.account.clear().await;
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
            "album" => likes::Liked::Album(self.session().await?.album(id, &Timings::default()).await?.album),
            "artist" => {
                let artist_id = id.parse().with_context(|| format!("“{id}” is not an artist id"))?;
                likes::Liked::Artist(self.session().await?.artist(artist_id, &Timings::default()).await?)
            }
            other => anyhow::bail!("unknown kind of like {other:?}"),
        };
        let id = id.to_string();
        self.db.write(move |conn| likes::record(conn, user_id, &id, &item, likes::now())).await?;
        self.mix.queue_likes();
        Ok(())
    }

    pub async fn favorite_remove(&self, kind: &str, id: &str, user_id: i64) -> Result<()> {
        let (kind, id) = (kind.to_string(), id.to_string());
        self.db.write(move |conn| likes::forget(conn, user_id, &kind, &id)).await
    }

    /// A person's "tune the space" weights, see `tuning`.
    pub async fn tuning(&self, user_id: i64) -> Result<std::collections::HashMap<String, f32>> {
        self.db.read(move |conn| crate::tuning::load(conn, user_id)).await
    }

    pub async fn set_tuning(&self, user_id: i64, weights: std::collections::HashMap<String, f32>) -> Result<()> {
        self.db.write(move |conn| crate::tuning::save(conn, user_id, &weights)).await
    }

    // --------------------------------------------------------- text steering

    /// Load the tower in the background, so the first request that steers
    /// does not wait for it. Only when it is on disk: fetching
    /// it is `models`' job, not the server's start.
    pub fn load_text(self: &Arc<Self>) {
        if !self.steerable.load(Ordering::SeqCst) {
            return;
        }
        let hub = self.clone();
        tokio::spawn(async move {
            // Mixing asks for its baseline with every set, and describing
            // for its words, so both are ready before the first.
            let _ = hub.embed(stelly_mix::text::BASELINE).await;
            if let Err(err) = hub.vocabulary().await {
                hub.log.push(format!("embedding the describing words failed: {err:#}"));
            }
        });
    }

    /// The tower, loaded once off the async threads; whoever asks while it
    /// loads waits for that load. A missing export is not an error, it is the
    /// ordinary state of a fresh checkout.
    async fn encoder(&self) -> Option<&Arc<Mutex<TextEncoder>>> {
        self.text
            .get_or_init(|| async {
                let model_dir = self.model_dir.clone();
                let encoder = match tokio::task::spawn_blocking(move || TextEncoder::load(&model_dir)).await {
                    Ok(Ok(encoder)) => encoder,
                    Ok(Err(err)) => {
                        self.log.push(format!("loading the text tower failed: {err:#}"));
                        None
                    }
                    Err(err) => {
                        self.log.push(format!("loading the text tower panicked: {err}"));
                        None
                    }
                };
                self.steerable.store(encoder.is_some(), Ordering::SeqCst);
                encoder.map(|encoder| Arc::new(Mutex::new(encoder)))
            })
            .await
            .as_ref()
    }

    /// A phrase's embedding, from memory when it has been asked for before.
    /// Callers asking for the same phrase at once share one run of the tower.
    pub async fn embed(&self, phrase: &str) -> Result<Arc<Vec<f32>>> {
        self.embeddings
            .try_get_with(phrase.to_string(), async { self.encode(phrase).await.map(Arc::new) })
            .await
            .map_err(|err| anyhow::anyhow!("{err:#}"))
    }

    /// Run the tower, off the async threads: milliseconds of ONNX on several
    /// cores, which would otherwise hold a worker other requests need.
    async fn encode(&self, phrase: &str) -> Result<Vec<f32>> {
        let encoder = self.encoder().await.ok_or_else(|| {
            anyhow::anyhow!(
                "text steering needs {}/{}; fetch it with: stelly-server models",
                self.model_dir.display(),
                crate::pipeline::models::CLAP_TEXT
            )
        })?;
        let (encoder, phrase) = (encoder.clone(), phrase.to_string());
        tokio::task::spawn_blocking(move || encoder.lock().unwrap_or_else(|e| e.into_inner()).embed(&phrase))
            .await
            .context("the worker thread panicked")?
    }

    /// Every word a track can be described with, embedded. Done in the
    /// background once the tower loads, a second or so; whoever asks before
    /// it is done waits for it.
    pub async fn vocabulary(&self) -> Result<Arc<Vocabulary>> {
        self.vocabulary
            .get_or_try_init(|| async {
                let mut vocabulary = Vocabulary::default();
                // Past the cache, which these would fill with phrases no one types.
                for (word, phrase) in crate::pipeline::labels::vocabulary() {
                    vocabulary.vectors.push(self.encode(&phrase).await?);
                    vocabulary.words.push(word);
                }
                let vocabulary = Arc::new(vocabulary);
                self.space.set_vocabulary(vocabulary.clone());
                Ok(vocabulary)
            })
            .await
            .cloned()
    }

    /// True while the tower is still loading: a request that steers waits
    /// for it rather than being told there is none.
    pub fn can_steer(&self) -> bool {
        self.steerable.load(Ordering::SeqCst)
    }

    // ---------------------------------------------------------------- hiding

    pub async fn blocked_artists(&self) -> Result<Vec<BlockedArtist>> {
        self.db.read(|conn| db::blocked_artists(conn)).await
    }

    pub async fn block_artist(&self, artist_id: i64, name: &str) -> Result<()> {
        let name = name.to_string();
        self.db.write(move |conn| db::block_artist(conn, artist_id, &name, None)).await?;
        self.refresh_blocked().await
    }

    pub async fn unblock_artist(&self, artist_id: i64) -> Result<()> {
        self.db.write(move |conn| db::unblock_artist(conn, artist_id)).await?;
        self.refresh_blocked().await
    }

    /// Tell the loaded space, which filters on every read rather than being
    /// rebuilt.
    async fn refresh_blocked(&self) -> Result<()> {
        self.corpus.clear();
        let blocked = self.blocked_artists().await?.into_iter().map(|a| a.artist_id).collect();
        let space = self.space.clone();
        tokio::task::spawn_blocking(move || space.set_blocked(blocked))
            .await
            .context("the worker thread panicked")
    }

    // ------------------------------------------- extending the catalogue

    /// The counts as of at most `CORPUS_TTL` ago. They scan whole tables.
    pub async fn corpus(&self) -> Result<Corpus> {
        let mut held = self.corpus.held.lock().await;
        let epoch = self.corpus.epoch.load(Ordering::SeqCst);
        if let Some((at, taken, corpus)) = &*held {
            if *taken == epoch && at.elapsed() < CORPUS_TTL {
                return Ok(corpus.clone());
            }
        }
        let corpus = self.db.read(|conn| stages::corpus(conn)).await?;
        *held = Some((Instant::now(), epoch, corpus.clone()));
        Ok(corpus)
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
        self.corpus.clear();
        self.drain().await
    }

    /// Dedupe, build-space and layout next, unless a build-space is
    /// already to come: what was analysed since reaches the map.
    pub async fn pipeline_rebuild(&self) -> Result<()> {
        {
            let mut queue = self.pipeline.queue.lock().unwrap();
            if !queue.stages.contains(&Stage::BuildSpace) {
                queue.rebuild_next();
            }
        }
        self.corpus.clear();
        self.drain().await
    }

    /// Rebuild the space for what the mix queue analysed, after each burst
    /// that gave it tracks, but no more often than `MIX_REBUILD_EVERY`: a
    /// rebuild has every client fetch the space again.
    pub fn show_mix_finds(self: &Arc<Self>) {
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                let spaced = hub.mix.spaced().await;
                hub.log.push(format!("mix: {spaced} tracks new to the space, rebuilding it"));
                if let Err(err) = hub.pipeline_rebuild().await {
                    hub.log.push(format!("mix: rebuilding the space failed: {err:#}"));
                }
                tokio::time::sleep(MIX_REBUILD_EVERY).await;
            }
        });
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
        let corpus = self.corpus.clone();
        let mix = self.mix.clone();

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
                corpus.clear();
                if stage == Stage::Dedupe {
                    mix.reread().await;
                }

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

    /// Returns once the pipeline log holds a line past `since`.
    pub async fn pipeline_log_past(&self, since: u64) {
        self.log.past(since).await
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

        // Counted before the lock is taken: a client reading the status
        // should not wait on SQL.
        let tracks = tracks::table.count().get_result(&mut conn).unwrap_or(0);
        let pending = frontier::table
            .filter(frontier::state.eq("pending"))
            .count()
            .get_result(&mut conn)
            .unwrap_or(0);
        {
            let mut status = job.status.lock().unwrap();
            status.last = Some(line);
            status.artists_expanded = stats.artists_expanded;
            status.albums_expanded = stats.albums_expanded;
            status.tracks_added = stats.tracks_added;
            status.errors = stats.errors;
            status.blocked_skipped = stats.blocked_skipped;
            status.tracks = tracks;
            status.pending = pending;
        }

        if !keep_going {
            break;
        }
    }

    Ok(())
}
