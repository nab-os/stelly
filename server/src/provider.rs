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

use crate::refs::{self, Item, Kind as RefKind};
use anyhow::{bail, Context, Result};
use diesel::SqliteConnection;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use stelly_provider::{api, AlbumTracks, Client, Config, FileUrl, Kind, Priority, ProviderError};
use tokio::sync::OnceCell;

pub use stelly_core::catalogue::{Capabilities, Quality, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
pub use stelly_provider::Account;

/// How long a search waits for any one provider: one slow or down is left
/// out rather than holding every search up.
const SEARCH_WAIT: std::time::Duration = std::time::Duration::from_secs(6);

/// How long a provider has to say what it is, and how long one that did
/// not is left alone before it is asked again: one down holds nothing up.
const ASK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const DOWN_FOR: std::time::Duration = std::time::Duration::from_secs(30);

/// One configured provider, and what it said it is.
struct Source {
    client: Client,
    capabilities: OnceCell<Capabilities>,
    /// When it last failed to say, if it has not said since.
    down_since: Mutex<Option<std::time::Instant>>,
}

impl Source {
    async fn capabilities(&self) -> Result<&Capabilities> {
        if let Some(known) = self.capabilities.get() {
            return Ok(known);
        }
        let url = self.client.url();
        if let Some(at) = *self.down_since.lock().unwrap_or_else(|e| e.into_inner()) {
            if at.elapsed() < DOWN_FOR {
                bail!("the music provider at {url} did not answer {}s ago", at.elapsed().as_secs());
            }
        }
        let ask = self.capabilities.get_or_try_init(|| async {
            self.client.send(api::capabilities()).await.with_context(|| format!("asking the music provider at {url} what it is"))
        });
        let answer = match tokio::time::timeout(ASK_WAIT, ask).await {
            Ok(answer) => answer,
            Err(_) => Err(anyhow::anyhow!("the music provider at {url} did not answer in {}s", ASK_WAIT.as_secs())),
        };
        *self.down_since.lock().unwrap_or_else(|e| e.into_inner()) = answer.is_err().then(std::time::Instant::now);
        answer
    }

    async fn name(&self) -> Result<&str> {
        Ok(&self.capabilities().await?.name)
    }
}

/// Cheap to clone; clones share connections, what each provider said it is,
/// and the database the ids are kept in.
#[derive(Clone)]
pub struct Provider {
    sources: Arc<Vec<Source>>,
    priority: Priority,
    db_path: PathBuf,
    conn: Arc<Mutex<Option<SqliteConnection>>>,
    ready: Arc<OnceCell<()>>,
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
            .map(|config| Source { client: Client::new(config), capabilities: OnceCell::new(), down_since: Mutex::new(None) })
            .collect();
        Self {
            sources: Arc::new(sources),
            priority: Priority::Interactive,
            db_path: db_path.to_path_buf(),
            conn: Arc::new(Mutex::new(None)),
            ready: Arc::new(OnceCell::new()),
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

    /// `f` with the ids' database, opened on first use. Never held across an
    /// await.
    fn with_refs<T>(&self, f: impl FnOnce(&mut SqliteConnection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if conn.is_none() {
            *conn = Some(crate::db::open_for_write(&self.db_path)?);
        }
        f(conn.as_mut().expect("opened just above"))
    }

    /// The provider a catalogue from before several providers came from:
    /// the one that can convert what it stored. None when it is not among
    /// them: until it is, that catalogue is nobody's.
    async fn legacy(&self) -> Result<Option<usize>> {
        for (source, s) in self.sources.iter().enumerate() {
            if s.capabilities().await.is_ok_and(|c| c.import) {
                return Ok(Some(source));
            }
        }
        anyhow::ensure!(!self.sources.is_empty(), "no music provider is configured");
        Ok(None)
    }

    /// The providers have said what they are, and a catalogue from before
    /// several providers has its ids taken for its provider's, if that
    /// provider is one of them. Those that did not answer are asked again
    /// next time, and the rest carry on without them.
    async fn ready(&self) -> Result<()> {
        if self.ready.initialized() {
            return Ok(());
        }
        let mut names = HashSet::new();
        let mut all = true;
        for source in self.sources.iter() {
            match source.name().await {
                Ok(name) => {
                    if !names.insert(name.to_string()) {
                        bail!("two music providers are both called {name:?}");
                    }
                }
                Err(err) => {
                    eprintln!("{err:#}");
                    all = false;
                }
            }
        }
        if let Some(source) = self.legacy().await? {
            let legacy = self.name(source).await?;
            let taken = self.with_refs(|conn| refs::bootstrap(conn, &legacy))?;
            if taken > 0 {
                eprintln!("took {taken} catalogue ids for {legacy}'s");
            }
        }
        if all {
            let _ = self.ready.set(());
        }
        Ok(())
    }

    /// The providers that have said what they are, by name.
    async fn answering(&self) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for source in 0..self.sources.len() {
            if let Ok(name) = self.name(source).await {
                out.push((source, name));
            }
        }
        out
    }

    async fn name(&self, source: usize) -> Result<String> {
        Ok(self.sources[source].name().await?.to_string())
    }

    /// The providers that have Stelly's `id`, most preferred first, with
    /// their own id for it.
    async fn routes(&self, kind: RefKind, id: &str) -> Result<Vec<(usize, String)>> {
        self.ready().await?;
        let known = self.with_refs(|conn| refs::routes(conn, kind, id))?;
        let mut out = Vec::new();
        for (source, name) in self.answering().await {
            if let Some((_, external)) = known.iter().find(|(provider, _)| *provider == name) {
                out.push((source, external.clone()));
            }
        }
        if out.is_empty() {
            bail!("no music provider that is answering has {} {id}", kind.as_str());
        }
        Ok(out)
    }

    /// The first provider that has `id` and answers `call`, and its answer.
    async fn first<T, F, Fut>(&self, kind: RefKind, id: &str, call: F) -> Result<(usize, T)>
    where
        F: Fn(Client, String) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut last = None;
        for (source, external) in self.routes(kind, id).await? {
            match call(self.client(source), external).await {
                Ok(answer) => return Ok((source, answer)),
                Err(err) => last = Some(err),
            }
        }
        Err(last.expect("routes is never empty"))
    }

    /// Every provider in turn whose capabilities `can` says yes to.
    async fn able(&self, can: impl Fn(&Capabilities) -> bool) -> Result<Vec<usize>> {
        self.ready().await?;
        let mut out = Vec::new();
        for (source, s) in self.sources.iter().enumerate() {
            if s.capabilities().await.is_ok_and(&can) {
                out.push(source);
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ ids

    fn number(id: String) -> Result<i64> {
        id.parse().with_context(|| format!("{id:?} is not a number"))
    }

    fn own_artist(conn: &mut SqliteConnection, provider: &str, id: Option<i64>, name: &str) -> Result<Option<i64>> {
        let Some(id) = id else { return Ok(None) };
        let key = Some(refs::key(&[name])).filter(|_| !name.is_empty());
        let item = Item { external: id.to_string(), key, ..Default::default() };
        Ok(Some(Self::number(refs::resolve(conn, provider, RefKind::Artist, &item)?)?))
    }

    fn own_track(conn: &mut SqliteConnection, provider: &str, track: &mut RemoteTrack) -> Result<()> {
        let item = Item {
            external: track.id.to_string(),
            isrc: track.isrc.as_deref(),
            key: Some(refs::credit_key(&track.artist, &track.title)),
            seconds: track.duration,
        };
        track.id = Self::number(refs::resolve(conn, provider, RefKind::Track, &item)?)?;
        track.artist_id = Self::own_artist(conn, provider, track.artist_id, &track.artist)?;
        if let Some(album) = track.album_id.take() {
            // The album's artist is not always the track's: no name to match
            // on until the album itself is seen.
            let item = Item { external: album, ..Default::default() };
            track.album_id = Some(refs::resolve(conn, provider, RefKind::Album, &item)?);
        }
        Ok(())
    }

    fn own_album(conn: &mut SqliteConnection, provider: &str, album: &mut RemoteAlbum) -> Result<()> {
        let key = Some(refs::credit_key(&album.artist, &album.title));
        let item = Item { external: album.id.clone(), key, ..Default::default() };
        album.id = refs::resolve(conn, provider, RefKind::Album, &item)?;
        album.artist_id = Self::own_artist(conn, provider, album.artist_id, &album.artist)?;
        Ok(())
    }

    fn own_playlist(conn: &mut SqliteConnection, provider: &str, playlist: &mut RemotePlaylist) -> Result<()> {
        let item = Item { external: playlist.id.to_string(), ..Default::default() };
        playlist.id = Self::number(refs::resolve(conn, provider, RefKind::Playlist, &item)?)?;
        Ok(())
    }

    fn own_tracks(&self, provider: &str, mut tracks: Vec<RemoteTrack>) -> Result<Vec<RemoteTrack>> {
        self.with_refs(|conn| {
            for track in &mut tracks {
                Self::own_track(conn, provider, track)?;
            }
            Ok(tracks)
        })
    }

    fn own_albums(&self, provider: &str, mut albums: Vec<RemoteAlbum>) -> Result<Vec<RemoteAlbum>> {
        self.with_refs(|conn| {
            for album in &mut albums {
                Self::own_album(conn, provider, album)?;
            }
            Ok(albums)
        })
    }

    fn own_artists(&self, provider: &str, mut artists: Vec<RemoteArtist>) -> Result<Vec<RemoteArtist>> {
        self.with_refs(|conn| {
            for artist in &mut artists {
                artist.id = Self::own_artist(conn, provider, Some(artist.id), &artist.name)?.expect("an id was given");
            }
            Ok(artists)
        })
    }

    // ------------------------------------------------------------ what they are

    /// Every answering provider's capabilities, in order.
    pub async fn each_capabilities(&self) -> Result<Vec<Capabilities>> {
        let mut out = Vec::new();
        let mut last = None;
        for source in self.sources.iter() {
            match source.capabilities().await {
                Ok(capabilities) => out.push(capabilities.clone()),
                Err(err) => last = Some(err),
            }
        }
        match (out.is_empty(), last) {
            (true, Some(err)) => Err(err),
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
        let mut out = Vec::new();
        for (source, s) in self.sources.iter().enumerate() {
            let what = match s.capabilities().await {
                Ok(c) => format!("{} ({})", c.title, c.name),
                Err(err) => format!("unreachable: {err:#}"),
            };
            out.push((what, s.client.url().to_string(), self.client(source).send(api::account()).await));
        }
        out
    }

    /// The provider called `name`.
    async fn named(&self, name: &str) -> Result<usize> {
        self.ready().await?;
        let answering = self.answering().await;
        if let Some((source, _)) = answering.iter().find(|(_, theirs)| theirs == name) {
            return Ok(*source);
        }
        let known: Vec<&str> = answering.iter().map(|(_, n)| n.as_str()).collect();
        bail!("no answering music provider is called {name:?}; there are {}", known.join(", "))
    }

    // ------------------------------------------------------------ browsing

    /// Every provider's results, the first one's first, each item once; or
    /// only those of the providers `only` names, when it names any.
    pub async fn search(&self, query: &str, limit: usize, only: &[String]) -> Result<SearchResults> {
        self.ready().await?;
        let mut asked = Vec::new();
        for name in only {
            asked.push(self.named(name).await?);
        }
        if asked.is_empty() {
            asked = self.answering().await.into_iter().map(|(source, _)| source).collect();
        }
        let asks = asked.into_iter().map(|source| async move {
            let client = self.client(source);
            let answer = match tokio::time::timeout(SEARCH_WAIT, client.send(api::search(query, limit))).await {
                Ok(answer) => answer,
                Err(_) => Err(anyhow::anyhow!("no answer in {}s", SEARCH_WAIT.as_secs())),
            };
            (source, answer)
        });
        let answers = futures_util::future::join_all(asks).await;
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
            let name = self.name(source).await?;
            out.tracks.extend(self.own_tracks(&name, found.tracks)?.into_iter().filter(|t| tracks.insert(t.id)));
            out.albums.extend(self.own_albums(&name, found.albums)?.into_iter().filter(|a| albums.insert(a.id.clone())));
            out.artists.extend(self.own_artists(&name, found.artists)?.into_iter().filter(|a| artists.insert(a.id)));
        }
        match failed {
            // Nobody answered: say why rather than that nothing was found.
            Some(err) if out.tracks.is_empty() && out.albums.is_empty() && out.artists.is_empty() => Err(err),
            _ => Ok(out),
        }
    }

    pub async fn track(&self, track_id: i64) -> Result<RemoteTrack> {
        let ask = |client: Client, id: String| async move { client.send(api::track(Self::number(id)?)).await };
        let (source, track) = self.first(RefKind::Track, &track_id.to_string(), ask).await?;
        let name = self.name(source).await?;
        Ok(self.own_tracks(&name, vec![track])?.remove(0))
    }

    /// An album and its tracklist, in one request.
    pub async fn album(&self, album_id: &str) -> Result<AlbumTracks> {
        let ask = |client: Client, id: String| async move { client.send(api::album(&id)).await };
        let (source, AlbumTracks { mut album, tracks }) = self.first(RefKind::Album, album_id, ask).await?;
        let name = self.name(source).await?;
        self.with_refs(|conn| Self::own_album(conn, &name, &mut album))?;
        // A tracklist names the album it is on: this one.
        let mut tracks = self.own_tracks(&name, tracks)?;
        for track in &mut tracks {
            track.album_id = Some(album.id.clone());
        }
        Ok(AlbumTracks { album, tracks })
    }

    pub async fn album_tracks(&self, album_id: &str) -> Result<(RemoteAlbum, Vec<RemoteTrack>)> {
        let AlbumTracks { album, tracks } = self.album(album_id).await?;
        Ok((album, tracks))
    }

    /// One artist on their own: name, portrait and biography, no discography.
    pub async fn artist(&self, artist_id: i64) -> Result<RemoteArtist> {
        let ask = |client: Client, id: String| async move { client.send(api::artist(Self::number(id)?)).await };
        let (source, artist) = self.first(RefKind::Artist, &artist_id.to_string(), ask).await?;
        let name = self.name(source).await?;
        Ok(self.own_artists(&name, vec![artist])?.remove(0))
    }

    /// An artist's discography from every provider that has them, newest
    /// first, each album once.
    async fn releases(&self, artist_id: i64, own: bool, cap: usize) -> Result<Vec<RemoteAlbum>> {
        let mut out: Vec<RemoteAlbum> = Vec::new();
        let mut seen = HashSet::new();
        let mut last = None;
        for (source, external) in self.routes(RefKind::Artist, &artist_id.to_string()).await? {
            match self.client(source).send(api::artist_releases(Self::number(external)?, own, cap)).await {
                Ok(page) => {
                    let name = self.name(source).await?;
                    out.extend(self.own_albums(&name, page.items)?.into_iter().filter(|a| seen.insert(a.id.clone())));
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

    pub async fn artist_albums(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        self.releases(artist_id, false, cap).await
    }

    /// An artist's own latest releases, newest first: albums, EPs, singles
    /// and live records, none of everyone else's covers of their songs.
    pub async fn artist_recent_releases(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        self.releases(artist_id, true, cap).await
    }

    /// Artists the providers consider close to this one, from every one that
    /// can say. The same call the crawler expands the frontier with.
    pub async fn similar_artists(&self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        let able = self.able(|c| c.similar_artists).await?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (source, external) in self.routes(RefKind::Artist, &artist_id.to_string()).await? {
            if !able.contains(&source) {
                continue;
            }
            match self.client(source).send(api::similar_artists(Self::number(external)?, limit)).await {
                Ok(similar) => {
                    let name = self.name(source).await?;
                    out.extend(self.own_artists(&name, similar)?.into_iter().filter(|a| seen.insert(a.id)));
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
        let capabilities = self.sources[source].capabilities().await?;
        anyhow::ensure!(capabilities.favourites, "{} keeps no likes of its own", capabilities.title);
        Ok(source)
    }

    /// The likes of the account behind the provider called `name`.
    pub async fn favourite_tracks(&self, name: &str, cap: usize) -> Result<Vec<RemoteTrack>> {
        let source = self.liking(name).await?;
        let found = self.client(source).send(api::favourite_tracks(cap)).await?.items;
        self.own_tracks(name, found)
    }

    pub async fn favourite_albums(&self, name: &str, cap: usize) -> Result<Vec<RemoteAlbum>> {
        let source = self.liking(name).await?;
        let found = self.client(source).send(api::favourite_albums(cap)).await?.items;
        self.own_albums(name, found)
    }

    pub async fn favourite_artists(&self, name: &str, cap: usize) -> Result<Vec<RemoteArtist>> {
        let source = self.liking(name).await?;
        let found = self.client(source).send(api::favourite_artists(cap)).await?.items;
        self.own_artists(name, found)
    }

    /// The playlists of every provider that keeps any.
    pub async fn user_playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        let mut out = Vec::new();
        for source in self.able(|c| c.playlists).await? {
            let name = self.name(source).await?;
            let mut found = self.client(source).send(api::playlists(cap)).await?.items;
            self.with_refs(|conn| {
                for playlist in &mut found {
                    Self::own_playlist(conn, &name, playlist)?;
                }
                Ok(())
            })?;
            out.extend(found);
        }
        Ok(out)
    }

    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        let ask = |client: Client, id: String| async move { client.send(api::playlist_tracks(Self::number(id)?, cap)).await };
        let (source, page) = self.first(RefKind::Playlist, &playlist_id.to_string(), ask).await?;
        let name = self.name(source).await?;
        self.own_tracks(&name, page.items)
    }

    // ------------------------------------------------------------ audio

    /// Where a track's audio is, in up to `quality`, from the first provider
    /// that has it and answers.
    pub async fn file(&self, track_id: i64, quality: Quality) -> Result<FileUrl> {
        let ask = |client: Client, id: String| async move { client.send(api::file_url(Self::number(id)?, quality)).await };
        Ok(self.first(RefKind::Track, &track_id.to_string(), ask).await?.1)
    }

    pub async fn file_url(&self, track_id: i64, quality: Quality) -> Result<String> {
        Ok(self.file(track_id, quality).await?.url)
    }

    /// Write a path out as a playlist of the first provider that can, with
    /// those of its tracks that provider has. Returns its id.
    pub async fn export_playlist(&self, name: &str, track_ids: &[i64]) -> Result<i64> {
        let source = *self.able(|c| c.export).await?.first().context("no music provider can write playlists")?;
        let provider = self.name(source).await?;
        let mut theirs = Vec::new();
        for id in track_ids {
            let routes = self.with_refs(|conn| refs::routes(conn, RefKind::Track, &id.to_string()))?;
            if let Some((_, external)) = routes.into_iter().find(|(p, _)| *p == provider) {
                theirs.push(Self::number(external)?);
            }
        }
        if theirs.is_empty() {
            bail!("{provider} has none of these tracks");
        }
        let created = self.client(source).send(api::create_playlist(name, &theirs)).await?.id;
        let mut playlist = RemotePlaylist { id: created, ..Default::default() };
        self.with_refs(|conn| Self::own_playlist(conn, &provider, &mut playlist))?;
        Ok(playlist.id)
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
            Some(source) => Ok(Some(self.sources[source].capabilities().await?.clone())),
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
}
