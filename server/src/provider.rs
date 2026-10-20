//! The music providers: browsing, stream URLs, playlists, and the bulk reads
//! the crawl and the analyser make, through the provider protocol, from as
//! many providers as are configured.
//!
//! Each provider is a service of its own, holding whatever account its
//! catalogue needs. `STELLY_PROVIDERS` lists them, most preferred first:
//!
//! ```text
//! STELLY_PROVIDERS=http://127.0.0.1:7711,http://secret@127.0.0.1:7710
//! ```
//!
//! a token, where one is wanted, as the URL's user; or `STELLY_PROVIDER_URL`
//! and `STELLY_PROVIDER_TOKEN` for just one. Everything here speaks in
//! Stelly's own ids, which `refs` keeps against each provider's: a track
//! from one provider that is the same recording as another's is one track,
//! played and analysed from the first provider in that order that has it,
//! the next if it fails. Browsing queues as interactive, the crawl and
//! analyse as bulk, so a search never waits behind them.

use crate::cache::Revalidating;
use crate::refs::{self, Item, Kind as RefKind};
use anyhow::{bail, Context, Result};
use diesel::SqliteConnection;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use stelly_provider::wire::SEARCH_SESSION_HEADER;
use stelly_provider::{api, AlbumTracks, Client, Config, FileUrl, Kind, Priority, ProviderError};
use tokio::sync::watch;

pub use stelly_core::catalogue::{Capabilities, Quality, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
pub use stelly_provider::Account;

/// How long a search waits for any one provider: one slow or down is left
/// out rather than holding every search up.
const SEARCH_WAIT: Duration = Duration::from_secs(6);

/// How long someone browsing waits on any one provider before the next one
/// that has the item is asked, or the request fails. The provider's client
/// sets no deadline of its own, so a gateway's queue or a 429's backoff
/// could hold a page for minutes. A healthy answer, a listing paged through
/// a gateway's budget included, comes well inside this, and the app gives
/// up on a request after a minute.
const BROWSE_WAIT: Duration = Duration::from_secs(15);

/// How long a play waits on any one provider for a stream's URL: shorter,
/// since the listener hears silence meanwhile. A cold yt-dlp takes a few
/// seconds; past this, the next provider is the better bet, and asking
/// again joins the run still going.
const FILE_WAIT: Duration = Duration::from_secs(10);

/// How long a provider has to say what it is, and how long one that did
/// not, or missed a deadline, is left alone or asked last: one down holds
/// nothing up.
const ASK_WAIT: Duration = Duration::from_secs(5);
const DOWN_FOR: Duration = Duration::from_secs(30);

/// How long new ids wait for a stage's write lock before the answer fails:
/// someone is waiting on a browse, while the crawl and analyse can wait.
const INTERACTIVE_WAIT: Duration = Duration::from_secs(2);
const BULK_WAIT: Duration = Duration::from_secs(30);

/// One configured provider, and what it said it is.
struct Source {
    client: Client,
    /// Set once it has said, by the background asking.
    capabilities: OnceLock<Capabilities>,
    /// When it last failed to say and why, if it has not said since.
    down: Mutex<Option<(Instant, String)>>,
    /// When it last missed a deadline, if it has not answered in time since.
    late_since: Mutex<Option<Instant>>,
}

impl Source {
    fn known(&self) -> Option<&Capabilities> {
        self.capabilities.get()
    }

    /// Not said yet, and not failed to lately: the next asking asks it.
    fn due(&self) -> bool {
        self.known().is_none() && self.down.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|(at, _)| at.elapsed() >= DOWN_FOR)
    }

    /// Why it has not said what it is.
    fn silent(&self) -> anyhow::Error {
        let url = self.client.url();
        match &*self.down.lock().unwrap_or_else(|e| e.into_inner()) {
            Some((at, why)) => anyhow::anyhow!("{why} ({}s ago)", at.elapsed().as_secs()),
            None => anyhow::anyhow!("the music provider at {url} has not said what it is yet"),
        }
    }

    fn late(&self) -> bool {
        self.late_since.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|at| at.elapsed() < DOWN_FOR)
    }
}

/// The asking of what the providers are, which runs in the background.
struct Probe {
    /// Whether an asking is out.
    running: AtomicBool,
    /// Moved as each provider answers and when an asking ends, for whoever
    /// waits on the first answer.
    answered: watch::Sender<u64>,
}

/// Ends an asking however its task ends, a runtime shut down under it
/// included: otherwise no provider would be asked again.
struct Asking<'p>(&'p Probe);

impl Drop for Asking<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
        self.0.answered.send_modify(|n| *n += 1);
    }
}

/// Cheap to clone; clones share connections, what each provider said it is,
/// and the database the ids are kept in.
#[derive(Clone)]
pub struct Provider {
    sources: Arc<Vec<Source>>,
    priority: Priority,
    /// Where Stelly's ids are kept.
    db: Arc<crate::db::Db>,
    probe: Arc<Probe>,
    /// The server's catalogue cache, when this runs inside it.
    catalogue: Option<Revalidating>,
}

/// The catalogue caches of the servers running on each database. The
/// crawl, the stages and mixing build their own clients, on runtimes of
/// their own, and read albums, artists, discographies and similar artists
/// through the same cache as browsing: neither fetches what the other just
/// did. A command line run has no server, and no cache.
fn catalogues() -> &'static Mutex<HashMap<PathBuf, Revalidating>> {
    static ALL: OnceLock<Mutex<HashMap<PathBuf, Revalidating>>> = OnceLock::new();
    ALL.get_or_init(Mutex::default)
}

/// Every client built on `db_path` from now on reads the catalogue through
/// `cache`.
pub fn share_catalogue(db_path: &Path, cache: Revalidating) {
    catalogues().lock().unwrap_or_else(|e| e.into_inner()).insert(db_path.to_path_buf(), cache);
}

/// A listing fetched for up to `held` items answers an ask for up to `cap`
/// when it was for as many or more, cut to `cap`, or came back short: then
/// it left nothing out.
fn fits<T>(cap: usize) -> impl Fn(&(usize, Vec<T>)) -> bool {
    move |(held, items)| *held >= cap || items.len() < *held
}

/// The search box `device`'s searches come from, for a provider that drops
/// a search a newer one from the same box replaced: one per device, so one
/// device's typing never replaces another's search. Unique to this server
/// too, should two share a provider.
pub fn search_session(device: i64) -> String {
    static SERVER: OnceLock<u64> = OnceLock::new();
    format!("{:016x}-{device}", SERVER.get_or_init(rand::random))
}

/// A provider gave up on this search because a newer one from the same
/// search box came: nobody wants its answer any more.
fn superseded(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ProviderError>().is_some_and(|e| e.status == 409)
}

/// Where one request's time went, step by step, for its `Server-Timing`.
/// Only what ran for this request: one served from the cache, or joined to
/// another's, has no steps of its own. Clones add to the same steps, so a
/// fetch the cache may finish in the background can carry one.
#[derive(Clone, Default)]
pub struct Timings(Arc<Mutex<Vec<(String, Duration)>>>);

impl Timings {
    /// `name` took `took`. Names are kept to a token's characters, as the
    /// header wants.
    pub fn add(&self, name: &str, took: Duration) {
        let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push((name, took));
    }

    /// `Server-Timing`'s value, `total` last: `qobuz-ask;dur=812.0, …`.
    pub fn header(&self, total: Duration) -> String {
        let steps = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
        let each = steps.iter().map(|(name, took)| format!("{name};dur={:.1}", ms(took)));
        each.chain([format!("total;dur={:.1}", ms(&total))]).collect::<Vec<_>>().join(", ")
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }
}

/// How an item is given Stelly's ids: by reading alone, or writing what is
/// new.
enum Ids<'c> {
    Read(&'c mut SqliteConnection),
    Write(&'c mut SqliteConnection),
}

impl Ids<'_> {
    /// `None` when reading alone cannot give it.
    fn of(&mut self, provider: &str, kind: RefKind, item: &Item) -> Result<Option<String>> {
        match self {
            Ids::Read(conn) => refs::known(conn, provider, kind, item),
            Ids::Write(conn) => refs::resolve(conn, provider, kind, item).map(Some),
        }
    }
}

/// `STELLY_PROVIDERS`, or the one `STELLY_PROVIDER_URL`, from the
/// environment over the .env in `env_dir`.
fn configured(env_dir: &Path) -> Vec<Config> {
    let file = stelly_provider::config::parse_env(&env_dir.join(".env"));
    let get = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned()).filter(|v| !v.trim().is_empty());
    let Some(list) = get("STELLY_PROVIDERS") else {
        return vec![Config::from_env(Some(&env_dir.join(".env")))];
    };
    let shared = get("STELLY_PROVIDER_TOKEN");
    list.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            // `scheme://token@host…`: the token, and the URL without it.
            let (scheme, rest) = entry.split_once("://").unwrap_or(("http", entry));
            let (token, host) = match rest.split_once('@') {
                Some((token, host)) if !token.contains('/') => (Some(token.to_string()), host),
                _ => (shared.clone(), rest),
            };
            Config { url: format!("{scheme}://{host}").trim_end_matches('/').to_string(), token }
        })
        .collect()
}

impl Provider {
    /// The providers `STELLY_PROVIDERS` names, their ids kept in `db_path`.
    pub fn from_env(env_dir: &Path, db_path: &Path) -> Self {
        let sources = configured(env_dir)
            .into_iter()
            .map(|config| Source {
                client: Client::new(config),
                capabilities: OnceLock::new(),
                down: Mutex::new(None),
                late_since: Mutex::new(None),
            })
            .collect();
        Self {
            sources: Arc::new(sources),
            priority: Priority::Interactive,
            db: crate::db::Db::shared(db_path),
            probe: Arc::new(Probe { running: AtomicBool::new(false), answered: watch::channel(0).0 }),
            catalogue: catalogues().lock().unwrap_or_else(|e| e.into_inner()).get(db_path).cloned(),
        }
    }

    /// The same providers, this client's requests queued behind browsing:
    /// for the crawl and analyse, which can wait.
    pub fn bulk(&self) -> Self {
        Self { priority: Priority::Bulk, ..self.clone() }
    }

    fn client(&self, source: usize) -> Client {
        self.sources[source].client.at(self.priority)
    }

    fn wait(&self) -> Duration {
        match self.priority {
            Priority::Interactive => INTERACTIVE_WAIT,
            Priority::Bulk => BULK_WAIT,
        }
    }

    /// `fetch`, through the server's catalogue cache when there is one.
    /// Browsing is served what is held, fetched again behind it once old;
    /// bulk reads take what is held however old, and keep what they fetch,
    /// in their own lane. An answer `fetch` had no part in, held or another
    /// request's, has only its wait in `timings`, as `cache`.
    async fn cached<T, F>(&self, key: String, fits: impl Fn(&T) -> bool, fetch: F, timings: &Timings) -> Result<T>
    where
        T: Clone + Send + Sync + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let started = Instant::now();
        let answer = match (&self.catalogue, self.priority) {
            (None, _) => fetch.await,
            (Some(cache), Priority::Interactive) => cache.get_or_fitting(key, fits, fetch).await,
            (Some(cache), Priority::Bulk) => cache.held_or(key, fits, fetch).await,
        };
        if timings.is_empty() {
            timings.add("cache", started.elapsed());
        }
        answer
    }

    /// Up to `cap` of a listing kept under `key`, `fetch` asking for up to
    /// so many. Fetched again for as many as are held when that is more: a
    /// short first page asked for after the whole listing would otherwise
    /// replace it once old, and the whole be fetched again behind it.
    async fn listing<T, F, Fut>(&self, key: String, cap: usize, timings: &Timings, fetch: F) -> Result<Vec<T>>
    where
        T: Clone + Send + Sync + 'static,
        F: FnOnce(usize) -> Fut,
        Fut: Future<Output = Result<Vec<T>>> + Send + 'static,
    {
        let held = match &self.catalogue {
            Some(cache) => cache.peek::<(usize, Vec<T>)>(&key).await.map_or(0, |(held, _)| held),
            None => 0,
        };
        let want = cap.max(held);
        let fetch = fetch(want);
        let (_, mut items) = self.cached(key, fits(cap), async move { Ok((want, fetch.await?)) }, timings).await?;
        items.truncate(cap);
        Ok(items)
    }

    /// `ask` of one provider, given up on after `wait` when someone is
    /// waiting on it. Bulk reads wait as long as the provider's queue takes:
    /// the crawl and analyse can, and giving up would only ask again. A
    /// provider that misses a deadline is asked last for a while.
    async fn within<T>(&self, source: usize, wait: Duration, ask: impl Future<Output = Result<T>>) -> Result<T> {
        if self.priority == Priority::Bulk {
            return ask.await;
        }
        let late = &self.sources[source].late_since;
        match tokio::time::timeout(wait, ask).await {
            Ok(answer) => {
                if answer.is_ok() {
                    *late.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                answer
            }
            Err(_) => {
                *late.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                bail!("the music provider at {} did not answer in {}s", self.sources[source].client.url(), wait.as_secs())
            }
        }
    }

    /// One answer's `items` in Stelly's ids, `own` giving one item its ids.
    ///
    /// Read first, on a reader and outside any transaction: an item seen
    /// before waits on no writer and no other answer. Only what that cannot
    /// give, a new item or a stub that learns its name, goes to the writer,
    /// in one transaction that takes the write lock before it reads: one
    /// that read first would fail at once had a stage written meanwhile, and
    /// two answers could take the same new id. Off the async workers.
    async fn own<T: Clone + Send + 'static>(
        &self,
        provider: &str,
        mut items: Vec<T>,
        own: fn(&mut Ids, &str, &mut T) -> Result<bool>,
    ) -> Result<Vec<T>> {
        let (db, provider, wait) = (self.db.clone(), provider.to_string(), self.wait());
        tokio::task::spawn_blocking(move || {
            let new = db.read_here(|conn| {
                let mut new = Vec::new();
                for (i, item) in items.iter_mut().enumerate() {
                    let mut read = item.clone();
                    if own(&mut Ids::Read(conn), &provider, &mut read)? {
                        *item = read;
                    } else {
                        new.push(i);
                    }
                }
                Ok(new)
            })?;
            if !new.is_empty() {
                db.write_here_within(wait, |conn| {
                    conn.immediate_transaction(|conn| {
                        for i in new {
                            own(&mut Ids::Write(conn), &provider, &mut items[i])?;
                        }
                        Ok(())
                    })
                })?;
            }
            Ok(items)
        })
        .await
        .context("the database thread panicked")?
    }

    /// The providers' items Stelly's `id` stands for.
    async fn refs_of(&self, kind: RefKind, id: &str) -> Result<Vec<(String, String)>> {
        let id = id.to_string();
        self.db.read(move |conn| refs::routes(conn, kind, &id)).await
    }

    /// The provider a catalogue from before several providers came from:
    /// the one that can convert what it stored. None when it is not among
    /// those answering: until it is, that catalogue is nobody's.
    async fn legacy(&self) -> Result<Option<usize>> {
        self.ready().await?;
        anyhow::ensure!(!self.sources.is_empty(), "no music provider is configured");
        Ok(self.sources.iter().position(|s| s.known().is_some_and(|c| c.import)))
    }

    /// Ask the providers that have not said what they are, and have not
    /// failed to lately, in the background and all at once. Whether an
    /// asking is out.
    fn probe(&self) -> bool {
        if !self.sources.iter().any(Source::due) {
            return self.probe.running.load(Ordering::SeqCst);
        }
        if !self.probe.running.swap(true, Ordering::SeqCst) {
            let provider = self.clone();
            tokio::spawn(async move { provider.asking().await });
        }
        true
    }

    /// Each provider's answer kept as it comes. The first to say it can
    /// convert a catalogue from before several providers has that
    /// catalogue's ids taken for its own first, before anything is routed
    /// to it.
    async fn asking(&self) {
        use futures_util::StreamExt;

        let _end = Asking(&self.probe);
        let asks = self.sources.iter().filter(|s| s.due()).map(|s| async move {
            let url = s.client.url();
            let answer = match tokio::time::timeout(ASK_WAIT, s.client.send(api::capabilities())).await {
                Ok(answer) => answer.with_context(|| format!("asking the music provider at {url} what it is")),
                Err(_) => Err(anyhow::anyhow!("the music provider at {url} did not answer in {}s", ASK_WAIT.as_secs())),
            };
            (s, answer)
        });
        let mut answers: futures_util::stream::FuturesUnordered<_> = asks.collect();
        while let Some((source, answer)) = answers.next().await {
            let answer = match answer {
                Ok(capabilities) if capabilities.import => self.bootstrap(&capabilities.name).await.map(|()| capabilities),
                answer => answer,
            };
            match answer {
                Ok(capabilities) => {
                    let _ = source.capabilities.set(capabilities);
                    *source.down.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    self.probe.answered.send_modify(|n| *n += 1);
                }
                Err(err) => {
                    eprintln!("{err:#}");
                    *source.down.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), format!("{err:#}")));
                }
            }
        }
    }

    /// A catalogue from before several providers has its ids taken for
    /// `name`'s, once.
    async fn bootstrap(&self, name: &str) -> Result<()> {
        let legacy = name.to_string();
        let taken = self.db.write_within(self.wait(), move |conn| refs::bootstrap(conn, &legacy)).await?;
        if taken > 0 {
            eprintln!("took {taken} catalogue ids for {name}'s");
        }
        Ok(())
    }

    /// The providers that have said what they are. Those that have not are
    /// asked in the background; a request waits for that only while no
    /// provider at all has answered, and then only for the first to.
    async fn ready(&self) -> Result<()> {
        let mut answered = self.probe.answered.subscribe();
        while self.probe() && self.sources.iter().all(|s| s.known().is_none()) {
            let _ = answered.changed().await;
        }
        let mut names = HashSet::new();
        for capabilities in self.sources.iter().filter_map(Source::known) {
            if !names.insert(capabilities.name.as_str()) {
                bail!("two music providers are both called {:?}", capabilities.name);
            }
        }
        Ok(())
    }

    /// The providers that have said what they are, by name.
    fn answering(&self) -> Vec<(usize, String)> {
        let known = self.sources.iter().enumerate();
        known.filter_map(|(source, s)| Some((source, s.known()?.name.clone()))).collect()
    }

    fn name(&self, source: usize) -> Result<String> {
        let s = &self.sources[source];
        Ok(s.known().ok_or_else(|| s.silent())?.name.clone())
    }

    /// The providers that have Stelly's `id`, most preferred first, with
    /// their own id for it.
    async fn routes(&self, kind: RefKind, id: &str) -> Result<Vec<(usize, String)>> {
        self.ready().await?;
        let known = self.refs_of(kind, id).await?;
        let mut out = Vec::new();
        for (source, name) in self.answering() {
            if let Some((_, external)) = known.iter().find(|(provider, _)| *provider == name) {
                out.push((source, external.clone()));
            }
        }
        if out.is_empty() {
            bail!("no music provider that is answering has {} {id}", kind.as_str());
        }
        // One that missed a deadline lately goes last, so it does not hold
        // every request up by as much again.
        out.sort_by_key(|(source, _)| self.sources[*source].late());
        Ok(out)
    }

    /// The first provider that has `id` and answers `call` within `wait`,
    /// and its answer. Each one's wait goes in `timings`.
    async fn first<T, F, Fut>(&self, kind: RefKind, id: &str, wait: Duration, call: F, timings: &Timings) -> Result<(usize, T)>
    where
        F: Fn(Client, String) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut last = None;
        for (source, external) in self.routes(kind, id).await? {
            let asking = Instant::now();
            let answer = self.within(source, wait, call(self.client(source), external)).await;
            timings.add(&format!("{}-ask", self.name(source)?), asking.elapsed());
            match answer {
                Ok(answer) => return Ok((source, answer)),
                Err(err) => last = Some(err),
            }
        }
        Err(last.expect("routes is never empty"))
    }

    /// Every answering provider whose capabilities `can` says yes to.
    async fn able(&self, can: impl Fn(&Capabilities) -> bool) -> Result<Vec<usize>> {
        self.ready().await?;
        Ok((0..self.sources.len()).filter(|&source| self.sources[source].known().is_some_and(&can)).collect())
    }

    // ------------------------------------------------------------ ids

    fn number(id: String) -> Result<i64> {
        id.parse().with_context(|| format!("{id:?} is not a number"))
    }

    // Each of these gives one item Stelly's ids, or says it could not: a
    // read that found the item new, or a stub that would learn its name.

    fn own_artist(ids: &mut Ids, provider: &str, id: &mut Option<i64>, name: &str) -> Result<bool> {
        let Some(theirs) = *id else { return Ok(true) };
        let key = Some(refs::key(&[name])).filter(|_| !name.is_empty());
        let item = Item { external: theirs.to_string(), key, ..Default::default() };
        let Some(own) = ids.of(provider, RefKind::Artist, &item)? else { return Ok(false) };
        *id = Some(Self::number(own)?);
        Ok(true)
    }

    fn own_track(ids: &mut Ids, provider: &str, track: &mut RemoteTrack) -> Result<bool> {
        let item = Item {
            external: track.id.to_string(),
            isrc: track.isrc.as_deref(),
            key: Some(refs::credit_key(&track.artist, &track.title)),
            seconds: track.duration,
        };
        let Some(id) = ids.of(provider, RefKind::Track, &item)? else { return Ok(false) };
        track.id = Self::number(id)?;
        if !Self::own_artist(ids, provider, &mut track.artist_id, &track.artist)? {
            return Ok(false);
        }
        if let Some(album) = track.album_id.take() {
            // The album's artist is not always the track's: no name to match
            // on until the album itself is seen.
            let item = Item { external: album, ..Default::default() };
            let Some(id) = ids.of(provider, RefKind::Album, &item)? else { return Ok(false) };
            track.album_id = Some(id);
        }
        Ok(true)
    }

    fn own_album(ids: &mut Ids, provider: &str, album: &mut RemoteAlbum) -> Result<bool> {
        let key = Some(refs::credit_key(&album.artist, &album.title));
        let item = Item { external: album.id.clone(), key, ..Default::default() };
        let Some(id) = ids.of(provider, RefKind::Album, &item)? else { return Ok(false) };
        album.id = id;
        Self::own_artist(ids, provider, &mut album.artist_id, &album.artist)
    }

    fn own_one_artist(ids: &mut Ids, provider: &str, artist: &mut RemoteArtist) -> Result<bool> {
        let mut id = Some(artist.id);
        if !Self::own_artist(ids, provider, &mut id, &artist.name)? {
            return Ok(false);
        }
        artist.id = id.expect("an id was given");
        Ok(true)
    }

    fn own_playlist(ids: &mut Ids, provider: &str, playlist: &mut RemotePlaylist) -> Result<bool> {
        let item = Item { external: playlist.id.to_string(), ..Default::default() };
        let Some(id) = ids.of(provider, RefKind::Playlist, &item)? else { return Ok(false) };
        playlist.id = Self::number(id)?;
        Ok(true)
    }

    async fn own_tracks(&self, provider: &str, tracks: Vec<RemoteTrack>) -> Result<Vec<RemoteTrack>> {
        self.own(provider, tracks, Self::own_track).await
    }

    async fn own_albums(&self, provider: &str, albums: Vec<RemoteAlbum>) -> Result<Vec<RemoteAlbum>> {
        self.own(provider, albums, Self::own_album).await
    }

    async fn own_artists(&self, provider: &str, artists: Vec<RemoteArtist>) -> Result<Vec<RemoteArtist>> {
        self.own(provider, artists, Self::own_one_artist).await
    }

    // ------------------------------------------------------------ what they are

    /// Every answering provider's capabilities, in order.
    pub async fn each_capabilities(&self) -> Result<Vec<Capabilities>> {
        self.ready().await?;
        let out: Vec<Capabilities> = self.sources.iter().filter_map(Source::known).cloned().collect();
        match self.sources.iter().find(|s| s.known().is_none()) {
            Some(silent) if out.is_empty() => Err(silent.silent()),
            _ => Ok(out),
        }
    }

    /// What they can do between them, under one name: what the app is told.
    pub async fn capabilities(&self) -> Result<Capabilities> {
        let each = self.each_capabilities().await?;
        let mut qualities: Vec<Quality> = each.iter().flat_map(|c| c.qualities.iter().copied()).collect();
        qualities.sort();
        qualities.dedup();
        Ok(Capabilities {
            name: "stelly".into(),
            title: each.iter().map(|c| c.title.as_str()).collect::<Vec<_>>().join(" + "),
            qualities,
            similar_artists: each.iter().any(|c| c.similar_artists),
            favourites: each.iter().any(|c| c.favourites),
            playlists: each.iter().any(|c| c.playlists),
            export: each.iter().any(|c| c.export),
            releases: each.iter().any(|c| c.releases),
            import: false,
        })
    }

    /// Who each provider is signed in as: what it is, where, and the account.
    pub async fn accounts(&self) -> Vec<(String, String, Result<Account>)> {
        if let Err(err) = self.ready().await {
            eprintln!("{err:#}");
        }
        let asks = self.sources.iter().enumerate().map(|(source, s)| async move {
            let what = match s.known() {
                Some(c) => format!("{} ({})", c.title, c.name),
                None => format!("unreachable: {:#}", s.silent()),
            };
            let account = self.within(source, BROWSE_WAIT, self.client(source).send(api::account())).await;
            (what, s.client.url().to_string(), account)
        });
        futures_util::future::join_all(asks).await
    }

    /// The provider called `name`.
    async fn named(&self, name: &str) -> Result<usize> {
        self.ready().await?;
        let answering = self.answering();
        if let Some((source, _)) = answering.iter().find(|(_, theirs)| theirs == name) {
            return Ok(*source);
        }
        let known: Vec<&str> = answering.iter().map(|(_, n)| n.as_str()).collect();
        bail!("no answering music provider is called {name:?}; there are {}", known.join(", "))
    }

    // ------------------------------------------------------------ browsing

    /// Every provider's results, the first one's first, each item once; or
    /// only those of the providers `only` names, when it names any. Each
    /// provider's wait for its answer, and the time its items took to get
    /// Stelly's ids, go in `timings`. `session` names the search box it
    /// came from; a provider that queues searches drops the one before it.
    pub async fn search(&self, query: &str, limit: usize, only: &[String], session: &str, timings: &Timings) -> Result<SearchResults> {
        let started = Instant::now();
        self.ready().await?;
        timings.add("ready", started.elapsed());
        let mut asked = Vec::new();
        for name in only {
            asked.push(self.named(name).await?);
        }
        if asked.is_empty() {
            asked = self.answering().into_iter().map(|(source, _)| source).collect();
        }
        let asks = asked.into_iter().map(|source| async move {
            let client = self.client(source);
            let asking = Instant::now();
            let answer = match tokio::time::timeout(SEARCH_WAIT, client.send(api::search(query, limit).header(SEARCH_SESSION_HEADER, session))).await {
                Ok(answer) => answer,
                Err(_) => Err(anyhow::anyhow!("no answer in {}s", SEARCH_WAIT.as_secs())),
            };
            let name = self.name(source).unwrap_or_else(|_| source.to_string());
            timings.add(&format!("{name}-ask"), asking.elapsed());
            (source, answer)
        });
        let mut answers = futures_util::future::join_all(asks).await;
        // Replaced at one provider: no answer at all, since what the others
        // found alone would be held as this query's results.
        if let Some(at) = answers.iter().position(|(_, answer)| answer.as_ref().is_err_and(superseded)) {
            return Err(answers.swap_remove(at).1.err().expect("found failed"));
        }
        let mut out = SearchResults::default();
        let (mut tracks, mut albums, mut artists) = (HashSet::new(), HashSet::new(), HashSet::new());
        let mut failed = None;
        for (source, answer) in answers {
            let found = match answer {
                Ok(found) => found,
                Err(err) => {
                    eprintln!("searching {} failed: {err:#}", self.sources[source].client.url());
                    failed = Some(err);
                    continue;
                }
            };
            let name = self.name(source)?;
            let owning = Instant::now();
            let (found_tracks, found_albums, found_artists) = (
                self.own_tracks(&name, found.tracks).await?,
                self.own_albums(&name, found.albums).await?,
                self.own_artists(&name, found.artists).await?,
            );
            timings.add(&format!("{name}-ids"), owning.elapsed());
            out.tracks.extend(found_tracks.into_iter().filter(|t| tracks.insert(t.id)));
            out.albums.extend(found_albums.into_iter().filter(|a| albums.insert(a.id.clone())));
            out.artists.extend(found_artists.into_iter().filter(|a| artists.insert(a.id)));
        }
        match failed {
            // Nobody answered: say why rather than that nothing was found.
            Some(err) if out.tracks.is_empty() && out.albums.is_empty() && out.artists.is_empty() => Err(err),
            _ => Ok(out),
        }
    }

    pub async fn track(&self, track_id: i64) -> Result<RemoteTrack> {
        let ask = |client: Client, id: String| async move { client.send(api::track(Self::number(id)?)).await };
        let (source, track) = self.first(RefKind::Track, &track_id.to_string(), BROWSE_WAIT, ask, &Timings::default()).await?;
        let name = self.name(source)?;
        Ok(self.own_tracks(&name, vec![track]).await?.remove(0))
    }

    /// An album and its tracklist, in one request.
    pub async fn album(&self, album_id: &str, timings: &Timings) -> Result<AlbumTracks> {
        let (this, id, steps) = (self.clone(), album_id.to_string(), timings.clone());
        self.cached(format!("album:{album_id}"), |_| true, async move { this.fetch_album(&id, &steps).await }, timings).await
    }

    async fn fetch_album(&self, album_id: &str, timings: &Timings) -> Result<AlbumTracks> {
        let ask = |client: Client, id: String| async move { client.send(api::album(&id)).await };
        let (source, AlbumTracks { album, tracks }) = self.first(RefKind::Album, album_id, BROWSE_WAIT, ask, timings).await?;
        let name = self.name(source)?;
        let owning = Instant::now();
        let album = self.own_albums(&name, vec![album]).await?.remove(0);
        // A tracklist names the album it is on: this one.
        let mut tracks = self.own_tracks(&name, tracks).await?;
        timings.add(&format!("{name}-ids"), owning.elapsed());
        for track in &mut tracks {
            track.album_id = Some(album.id.clone());
        }
        Ok(AlbumTracks { album, tracks })
    }

    pub async fn album_tracks(&self, album_id: &str, timings: &Timings) -> Result<(RemoteAlbum, Vec<RemoteTrack>)> {
        let AlbumTracks { album, tracks } = self.album(album_id, timings).await?;
        Ok((album, tracks))
    }

    /// One artist on their own: name, portrait and biography, no discography.
    pub async fn artist(&self, artist_id: i64, timings: &Timings) -> Result<RemoteArtist> {
        let (this, steps) = (self.clone(), timings.clone());
        self.cached(format!("artist:{artist_id}"), |_| true, async move { this.fetch_artist(artist_id, &steps).await }, timings).await
    }

    async fn fetch_artist(&self, artist_id: i64, timings: &Timings) -> Result<RemoteArtist> {
        let ask = |client: Client, id: String| async move { client.send(api::artist(Self::number(id)?)).await };
        let (source, artist) = self.first(RefKind::Artist, &artist_id.to_string(), BROWSE_WAIT, ask, timings).await?;
        let name = self.name(source)?;
        let owning = Instant::now();
        let artist = self.own_artists(&name, vec![artist]).await?.remove(0);
        timings.add(&format!("{name}-ids"), owning.elapsed());
        Ok(artist)
    }

    /// An artist's discography from every provider that has them, newest
    /// first, each album once. Each provider's wait, and the time its
    /// albums took to get Stelly's ids, go in `timings`.
    ///
    /// The first `n` of a shorter ask are not always the first `n` of a
    /// longer one: each provider's albums past the shorter cap may be newer
    /// than another's within it, or the same album, which the first
    /// provider to have it then gives.
    async fn releases(&self, artist_id: i64, own: bool, cap: usize, timings: &Timings) -> Result<Vec<RemoteAlbum>> {
        let mut out: Vec<RemoteAlbum> = Vec::new();
        let mut seen = HashSet::new();
        let mut last = None;
        // All at once: one slow provider costs its own wait, not the sum.
        let asks = self.routes(RefKind::Artist, &artist_id.to_string()).await?.into_iter().map(|(source, external)| async move {
            let asking = Instant::now();
            let ask = async { self.client(source).send(api::artist_releases(Self::number(external)?, own, cap)).await };
            let answer = self.within(source, BROWSE_WAIT, ask).await;
            timings.add(&format!("{}-ask", self.name(source).unwrap_or_else(|_| source.to_string())), asking.elapsed());
            (source, answer)
        });
        for (source, answer) in futures_util::future::join_all(asks).await {
            match answer {
                Ok(page) => {
                    let name = self.name(source)?;
                    let owning = Instant::now();
                    let albums = self.own_albums(&name, page.items).await?;
                    timings.add(&format!("{name}-ids"), owning.elapsed());
                    out.extend(albums.into_iter().filter(|a| seen.insert(a.id.clone())));
                }
                Err(err) => last = Some(err),
            }
        }
        if let (true, Some(err)) = (out.is_empty(), last) {
            return Err(err);
        }
        out.sort_by(|a, b| b.released.cmp(&a.released));
        Ok(out)
    }

    pub async fn artist_albums(&self, artist_id: i64, cap: usize, timings: &Timings) -> Result<Vec<RemoteAlbum>> {
        let (this, steps) = (self.clone(), timings.clone());
        let fetch = move |cap| async move { this.releases(artist_id, false, cap, &steps).await };
        self.listing(format!("artist_albums:{artist_id}"), cap, timings, fetch).await
    }

    /// An artist's own latest releases, newest first: albums, EPs, singles
    /// and live records, none of everyone else's covers of their songs.
    pub async fn artist_recent_releases(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        self.releases(artist_id, true, cap, &Timings::default()).await
    }

    /// Artists the providers consider close to this one, from every one that
    /// can say. The same call the crawler expands the frontier with.
    pub async fn similar_artists(&self, artist_id: i64, limit: usize, timings: &Timings) -> Result<Vec<RemoteArtist>> {
        let (this, steps) = (self.clone(), timings.clone());
        let fetch = move |limit| async move { this.fetch_similar(artist_id, limit, &steps).await };
        self.listing(format!("similar_artists:{artist_id}"), limit, timings, fetch).await
    }

    async fn fetch_similar(&self, artist_id: i64, limit: usize, timings: &Timings) -> Result<Vec<RemoteArtist>> {
        let able = self.able(|c| c.similar_artists).await?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let routes = self.routes(RefKind::Artist, &artist_id.to_string()).await?;
        let asks = routes.into_iter().filter(|(source, _)| able.contains(source)).map(|(source, external)| async move {
            let asking = Instant::now();
            let ask = async { self.client(source).send(api::similar_artists(Self::number(external)?, limit)).await };
            let answer = self.within(source, BROWSE_WAIT, ask).await;
            timings.add(&format!("{}-ask", self.name(source).unwrap_or_else(|_| source.to_string())), asking.elapsed());
            (source, answer)
        });
        for (source, answer) in futures_util::future::join_all(asks).await {
            match answer {
                Ok(similar) => {
                    let name = self.name(source)?;
                    let owning = Instant::now();
                    let similar = self.own_artists(&name, similar).await?;
                    timings.add(&format!("{name}-ids"), owning.elapsed());
                    out.extend(similar.into_iter().filter(|a| seen.insert(a.id)));
                }
                Err(err) if err.downcast_ref::<ProviderError>().is_some_and(|e| e.status == 404) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ accounts

    /// The provider called `name`, if it keeps an account's likes.
    async fn liking(&self, name: &str) -> Result<usize> {
        let source = self.named(name).await?;
        let capabilities = self.sources[source].known().ok_or_else(|| self.sources[source].silent())?;
        anyhow::ensure!(capabilities.favourites, "{} keeps no likes of its own", capabilities.title);
        Ok(source)
    }

    /// The likes of the account behind the provider called `name`.
    pub async fn favourite_tracks(&self, name: &str, cap: usize) -> Result<Vec<RemoteTrack>> {
        let source = self.liking(name).await?;
        let found = self.within(source, BROWSE_WAIT, self.client(source).send(api::favourite_tracks(cap))).await?.items;
        self.own_tracks(name, found).await
    }

    pub async fn favourite_albums(&self, name: &str, cap: usize) -> Result<Vec<RemoteAlbum>> {
        let source = self.liking(name).await?;
        let found = self.within(source, BROWSE_WAIT, self.client(source).send(api::favourite_albums(cap))).await?.items;
        self.own_albums(name, found).await
    }

    pub async fn favourite_artists(&self, name: &str, cap: usize) -> Result<Vec<RemoteArtist>> {
        let source = self.liking(name).await?;
        let found = self.within(source, BROWSE_WAIT, self.client(source).send(api::favourite_artists(cap))).await?.items;
        self.own_artists(name, found).await
    }

    /// The playlists of every provider that keeps any.
    pub async fn user_playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        let mut out = Vec::new();
        for source in self.able(|c| c.playlists).await? {
            let name = self.name(source)?;
            let found = self.within(source, BROWSE_WAIT, self.client(source).send(api::playlists(cap))).await?.items;
            out.extend(self.own(&name, found, Self::own_playlist).await?);
        }
        Ok(out)
    }

    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        let ask = |client: Client, id: String| async move { client.send(api::playlist_tracks(Self::number(id)?, cap)).await };
        let (source, page) = self.first(RefKind::Playlist, &playlist_id.to_string(), BROWSE_WAIT, ask, &Timings::default()).await?;
        let name = self.name(source)?;
        self.own_tracks(&name, page.items).await
    }

    // ------------------------------------------------------------ audio

    /// Where a track's audio is, in up to `quality`, from the first provider
    /// that has it and answers.
    pub async fn file(&self, track_id: i64, quality: Quality) -> Result<FileUrl> {
        let ask = |client: Client, id: String| async move { client.send(api::file_url(Self::number(id)?, quality)).await };
        Ok(self.first(RefKind::Track, &track_id.to_string(), FILE_WAIT, ask, &Timings::default()).await?.1)
    }

    pub async fn file_url(&self, track_id: i64, quality: Quality) -> Result<String> {
        Ok(self.file(track_id, quality).await?.url)
    }

    /// Write a path out as a playlist of the first provider that can, with
    /// those of its tracks that provider has. Returns its id.
    pub async fn export_playlist(&self, name: &str, track_ids: &[i64]) -> Result<i64> {
        let source = *self.able(|c| c.export).await?.first().context("no music provider can write playlists")?;
        let provider = self.name(source)?;
        let mut theirs = Vec::new();
        for id in track_ids {
            let routes = self.refs_of(RefKind::Track, &id.to_string()).await?;
            if let Some((_, external)) = routes.into_iter().find(|(p, _)| *p == provider) {
                theirs.push(Self::number(external)?);
            }
        }
        if theirs.is_empty() {
            bail!("{provider} has none of these tracks");
        }
        // No deadline: a playlist written after one would still be written,
        // and the retry it prompted would write it twice.
        let created = self.client(source).send(api::create_playlist(name, &theirs)).await?.id;
        let playlist = RemotePlaylist { id: created, ..Default::default() };
        Ok(self.own(&provider, vec![playlist], Self::own_playlist).await?.remove(0).id)
    }

    /// Items in the shapes of the provider an older catalogue came from, as
    /// a Stelly before providers stored them, in the protocol's. Its ids are
    /// Stelly's, so they are left as they are.
    pub async fn import<T: DeserializeOwned>(&self, kind: Kind, items: &[Value]) -> Result<Vec<Option<T>>> {
        let source = self.legacy().await?.context("no music provider can convert what a Stelly before providers stored")?;
        self.client(source).send(api::import(kind, items)).await
    }

    /// What that provider is, if it is one of them.
    pub async fn legacy_capabilities(&self) -> Result<Option<Capabilities>> {
        match self.legacy().await? {
            Some(source) => Ok(self.sources[source].known().cloned()),
            None => Ok(None),
        }
    }
}

/// Repo root, inferred from the crate location.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_are_listed_with_their_tokens() {
        let dir = std::env::temp_dir().join(format!("stelly-providers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "STELLY_PROVIDERS=http://127.0.0.1:7711, https://secret@music.example/\nSTELLY_PROVIDER_TOKEN=shared\n",
        )
        .unwrap();
        let configs = configured(&dir);
        let pairs: Vec<(&str, Option<&str>)> = configs.iter().map(|c| (c.url.as_str(), c.token.as_deref())).collect();
        assert_eq!(pairs, [("http://127.0.0.1:7711", Some("shared")), ("https://music.example", Some("secret"))]);
    }

    /// One provider that never answers holds up neither the first request,
    /// which waits for the one that does, nor any after; and a call past its
    /// deadline fails and sends that provider to the back.
    #[tokio::test]
    async fn a_provider_that_hangs_holds_nothing_up() {
        let hung = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (hung_at, up_at) = (hung.local_addr().unwrap(), up.local_addr().unwrap());
        let capabilities = Capabilities { name: "up".into(), title: "Up".into(), ..Default::default() };
        let app = axum::Router::new().route("/v1/capabilities", axum::routing::get(move || async move { axum::Json(capabilities) }));
        tokio::spawn(async move { axum::serve(up, app).await });

        let dir = std::env::temp_dir().join(format!("stelly-provider-hangs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), format!("STELLY_PROVIDERS=http://{hung_at},http://{up_at}\n")).unwrap();
        let provider = Provider::from_env(&dir, &dir.join("stelly.db"));

        let asked = Instant::now();
        let first = futures_util::future::join_all((0..8).map(|_| provider.each_capabilities())).await;
        assert!(asked.elapsed() < Duration::from_secs(1), "waited {:?}", asked.elapsed());
        for answer in first {
            assert_eq!(answer.unwrap().len(), 1);
        }

        let late = provider.within(0, Duration::from_millis(50), std::future::pending::<Result<()>>()).await;
        assert!(late.unwrap_err().to_string().contains("did not answer"));
        assert!(provider.sources[0].late());
        let bulk = provider.bulk();
        let unbounded = tokio::time::timeout(Duration::from_millis(200), bulk.within(0, Duration::from_millis(50), std::future::pending::<Result<()>>()));
        assert!(unbounded.await.is_err(), "bulk calls have no deadline");
        drop(hung);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A search's `Server-Timing` names each provider's wait, at least as
    /// long as it took to answer, and its ids.
    #[tokio::test]
    async fn a_search_says_where_its_time_went() {
        let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = slow.local_addr().unwrap();
        let capabilities = Capabilities { name: "slow".into(), title: "Slow".into(), ..Default::default() };
        let found = SearchResults { tracks: vec![RemoteTrack { id: 1, title: "T".into(), artist: "A".into(), ..Default::default() }], ..Default::default() };
        let app = axum::Router::new()
            .route("/v1/capabilities", axum::routing::get(move || async move { axum::Json(capabilities) }))
            .route(
                "/v1/search",
                axum::routing::get(move || async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    axum::Json(found)
                }),
            );
        tokio::spawn(async move { axum::serve(slow, app).await });

        let dir = std::env::temp_dir().join(format!("stelly-provider-timings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), format!("STELLY_PROVIDERS=http://{at}\n")).unwrap();
        crate::db::open_for_write(&dir.join("stelly.db")).unwrap();
        let provider = Provider::from_env(&dir, &dir.join("stelly.db"));

        let timings = Timings::default();
        assert_eq!(provider.search("q", 10, &[], "box", &timings).await.unwrap().tracks.len(), 1);
        let header = timings.header(Duration::from_millis(250));
        let dur = |name: &str| -> f64 {
            let step = header.split(", ").find(|s| s.starts_with(&format!("{name};"))).unwrap_or_else(|| panic!("no {name} in {header}"));
            step.split("dur=").nth(1).unwrap().parse().unwrap()
        };
        assert!(dur("slow-ask") >= 200.0, "{header}");
        assert!(dur("slow-ids") >= 0.0);
        assert_eq!(dur("total"), 250.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each search names its search box to every provider; one a provider
    /// gave up on for a newer search from that box is not answered with
    /// what the others found.
    #[tokio::test]
    async fn a_search_names_its_box_and_one_replaced_is_not_answered() {
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let mut urls = Vec::new();
        for (name, replaced) in [("gateway", true), ("other", false)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            urls.push(format!("http://{}", listener.local_addr().unwrap()));
            let capabilities = Capabilities { name: name.into(), title: name.into(), ..Default::default() };
            let sessions = sessions.clone();
            let search = move |headers: axum::http::HeaderMap| async move {
                let session = headers.get(SEARCH_SESSION_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
                sessions.lock().unwrap().push(session.to_string());
                let status = if replaced { axum::http::StatusCode::CONFLICT } else { axum::http::StatusCode::OK };
                (status, axum::Json(SearchResults::default()))
            };
            let app = axum::Router::new()
                .route("/v1/capabilities", axum::routing::get(move || async move { axum::Json(capabilities) }))
                .route("/v1/search", axum::routing::get(search));
            tokio::spawn(async move { axum::serve(listener, app).await });
        }

        let dir = std::env::temp_dir().join(format!("stelly-provider-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), format!("STELLY_PROVIDERS={}\n", urls.join(","))).unwrap();
        crate::db::open_for_write(&dir.join("stelly.db")).unwrap();
        let provider = Provider::from_env(&dir, &dir.join("stelly.db"));
        provider.each_capabilities().await.unwrap();
        while provider.answering().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let session = search_session(7);
        let answer = provider.search("q", 10, &[], &session, &Timings::default()).await;
        assert!(answer.is_err_and(|err| superseded(&err)));
        assert_eq!(*sessions.lock().unwrap(), [session.clone(), session.clone()]);
        assert_eq!(search_session(7), session, "the same box across a burst");
        assert_ne!(search_session(8), session, "never another device's");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A discography's `Server-Timing` names the provider's wait and its
    /// ids, or the cache alone. A shorter ask after a longer one is the
    /// start of the held one, asking nothing; and fetched again once old,
    /// it is the longer one that is fetched, not replaced by the shorter.
    #[tokio::test]
    async fn a_first_page_neither_refetches_nor_shrinks_the_whole_discography() {
        let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = slow.local_addr().unwrap();
        let capabilities = Capabilities { name: "slow".into(), title: "Slow".into(), ..Default::default() };
        let asked = Arc::new(Mutex::new(Vec::new()));
        let caps = asked.clone();
        let app = axum::Router::new()
            .route("/v1/capabilities", axum::routing::get(move || async move { axum::Json(capabilities) }))
            .route(
                "/v1/artists/{id}/releases",
                axum::routing::get(move |axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>| async move {
                    let cap: usize = query["cap"].parse().unwrap();
                    caps.lock().unwrap().push(cap);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let items: Vec<RemoteAlbum> = (0..cap.min(300))
                        .map(|n| RemoteAlbum {
                            id: format!("a{n}"),
                            title: format!("A{n}"),
                            artist: "A".into(),
                            artist_id: Some(50),
                            released: Some(format!("{}", 3000 - n)),
                            ..Default::default()
                        })
                        .collect();
                    axum::Json(serde_json::json!({ "total": items.len(), "items": items }))
                }),
            );
        tokio::spawn(async move { axum::serve(slow, app).await });

        let dir = std::env::temp_dir().join(format!("stelly-provider-pages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), format!("STELLY_PROVIDERS=http://{at}\n")).unwrap();
        let db_path = dir.join("stelly.db");
        crate::db::open_for_write(&db_path).unwrap();
        share_catalogue(&db_path, Revalidating::new(Duration::from_millis(300), Duration::from_secs(60), 100));
        let provider = Provider::from_env(&dir, &db_path);
        provider.ready().await.unwrap();
        let artist = RemoteArtist { id: 50, name: "A".into(), ..Default::default() };
        let id = provider.own_artists("slow", vec![artist]).await.unwrap()[0].id;
        let steps = |timings: &Timings| -> Vec<String> {
            let header = timings.header(Duration::ZERO);
            header.split(", ").map(|step| step.split(';').next().unwrap().to_string()).collect()
        };

        let timings = Timings::default();
        let whole = provider.artist_albums(id, 500, &timings).await.unwrap();
        assert_eq!(whole.len(), 300);
        assert_eq!(steps(&timings), ["slow-ask", "slow-ids", "total"]);

        let timings = Timings::default();
        let first = provider.artist_albums(id, 100, &timings).await.unwrap();
        assert_eq!(first[..], whole[..100]);
        assert_eq!(steps(&timings), ["cache", "total"]);
        assert_eq!(*asked.lock().unwrap(), [500]);

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(provider.artist_albums(id, 100, &Timings::default()).await.unwrap().len(), 100, "the old one, at once");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(*asked.lock().unwrap(), [500, 500], "fetched again whole");
        assert_eq!(provider.artist_albums(id, 500, &Timings::default()).await.unwrap().len(), 300);
        assert_eq!(*asked.lock().unwrap(), [500, 500]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Items seen before are given their ids while a stage holds the write
    /// lock; new ones wait for it, and two answers at once with the same new
    /// item give it one id.
    #[tokio::test]
    async fn ids_already_known_wait_on_no_writer() {
        use diesel::connection::SimpleConnection;

        let dir = std::env::temp_dir().join(format!("stelly-provider-ids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("stelly.db");
        crate::db::open_for_write(&db_path).unwrap();
        let provider = Provider::from_env(&dir, &db_path);
        let track = |id: i64| RemoteTrack {
            id,
            title: format!("T{id}"),
            artist: "A".into(),
            artist_id: Some(7),
            album_id: Some("b".into()),
            ..Default::default()
        };

        let (one, two) = tokio::join!(
            provider.own_tracks("p", vec![track(1), track(2)]),
            provider.own_tracks("p", vec![track(2), track(3)]),
        );
        let (one, two) = (one.unwrap(), two.unwrap());
        assert_eq!(one[1], two[0], "the same new item, one id");
        assert_ne!(one[0].id, one[1].id);

        let stage = &mut crate::db::connect(&db_path, std::time::Duration::from_secs(1)).unwrap();
        stage.batch_execute("BEGIN IMMEDIATE;").unwrap();
        let asked = std::time::Instant::now();
        assert_eq!(provider.own_tracks("p", vec![track(1)]).await.unwrap()[0], one[0]);
        assert!(asked.elapsed() < std::time::Duration::from_millis(500), "waited {:?}", asked.elapsed());
        // A new one waits for the lock, briefly, then says so.
        assert!(provider.own_tracks("p", vec![track(4)]).await.is_err());
        stage.batch_execute("COMMIT;").unwrap();
        assert!(provider.own_tracks("p", vec![track(4)]).await.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
