//! Qobuz, through stelly-qobuz's gateway: browsing, stream URLs, playlists,
//! and the bulk reads the crawl and the analyser make.
//!
//! The gateway holds the account, its credentials, signing and its one
//! request queue, shared with whatever else on the machine talks to Qobuz.
//! It answers with Qobuz's own JSON; this turns that into
//! `stelly_core::qobuz`'s shapes, shared with the app. Browsing queues as
//! interactive, and the crawl and analyse as bulk, so a search never waits
//! behind them.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use stelly_qobuz_client::{api, Client, Config, Priority};

pub use stelly_core::qobuz::{
    discography, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults, FORMAT_MP3_320,
};

/// Cheap to clone; clones share the gateway's connections.
#[derive(Clone)]
pub struct QobuzClient {
    gateway: Client,
}

impl QobuzClient {
    /// The gateway at `STELLY_QOBUZ_URL`, with `STELLY_QOBUZ_TOKEN`, from the
    /// environment over the .env in `env_dir`; by default the gateway on
    /// this machine.
    pub fn from_env(env_dir: &Path) -> Self {
        Self {
            gateway: Client::new(Config::from_env(Some(&env_dir.join(".env")))),
        }
    }

    /// The same gateway, this client's requests queued behind browsing: for
    /// the crawl and analyse, which can wait.
    pub fn bulk(&self) -> Self {
        Self {
            gateway: self.gateway.at(Priority::Bulk),
        }
    }

    /// The signed-in account, which is also the check that the token works.
    pub async fn user(&self) -> Result<Value> {
        self.gateway.send(api::user()).await
    }

    /// Full-catalogue search: tracks, albums and artists in one request.
    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults> {
        let data = self.gateway.send(api::search(query, limit, None)).await?;
        let items = |key: &str| list(&data[key]["items"]);
        Ok(SearchResults {
            tracks: items("tracks").iter().filter_map(|v| RemoteTrack::parse(v, None)).collect(),
            albums: items("albums").iter().filter_map(RemoteAlbum::parse).collect(),
            artists: items("artists").iter().filter_map(RemoteArtist::parse).collect(),
        })
    }

    /// Raw favourites, exactly as Qobuz returned them. `kind` is 'tracks',
    /// 'albums' or 'artists'. What `favourites` prints.
    pub async fn favorites_raw(&self, kind: &str, cap: usize) -> Result<Vec<Value>> {
        Ok(self.gateway.send(api::favourites(kind, cap)).await?.items)
    }

    pub async fn user_playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        let page = self.gateway.send(api::playlists(cap)).await?;
        Ok(page.items.iter().filter_map(RemotePlaylist::parse).collect())
    }

    /// A playlist's tracklist. These carry their own album/performer, so no
    /// parent context is needed.
    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        let page = self.gateway.send(api::playlist_tracks(&playlist_id.to_string(), cap)).await?;
        Ok(page.items.iter().filter_map(|v| RemoteTrack::parse(v, None)).collect())
    }

    /// The whole `album/get` payload, tracklist included.
    pub async fn album_raw(&self, album_id: &str) -> Result<Value> {
        self.gateway.send(api::album(album_id)).await
    }

    /// The whole `track/get` payload, its album included.
    pub async fn track_raw(&self, track_id: i64) -> Result<Value> {
        self.gateway.send(api::track(&track_id.to_string())).await
    }

    /// An album and its tracklist, in one request.
    pub async fn album_tracks(&self, album_id: &str) -> Result<(RemoteAlbum, Vec<RemoteTrack>)> {
        let data = self.album_raw(album_id).await?;
        let album = RemoteAlbum::parse(&data).with_context(|| format!("album {album_id} had no usable metadata"))?;
        let tracks = list(&data["tracks"]["items"])
            .iter()
            .filter_map(|v| RemoteTrack::parse(v, Some(&album)))
            .collect();
        Ok((album, tracks))
    }

    /// One artist on its own: name, portrait and biography, no discography.
    pub async fn artist(&self, artist_id: i64) -> Result<RemoteArtist> {
        let data = self.artist_raw(artist_id).await?;
        RemoteArtist::parse(&data).with_context(|| format!("artist {artist_id} has no id"))
    }

    /// The whole `artist/get` payload.
    pub async fn artist_raw(&self, artist_id: i64) -> Result<Value> {
        self.gateway.send(api::artist(&artist_id.to_string())).await
    }

    /// Raw album objects for an artist, paginated through `artist/get`.
    pub async fn artist_albums_raw(&self, artist_id: i64, cap: usize) -> Result<Vec<Value>> {
        Ok(self.gateway.send(api::artist_albums(&artist_id.to_string(), cap)).await?.items)
    }

    /// An artist's discography, newest first, each album marked with what
    /// sort of release it is and how they are credited on it.
    pub async fn artist_albums(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        // `artist/get`'s listing is the same albums without a release type,
        // so the page still loads, ungrouped, if the web player's endpoint
        // ever goes away.
        let raw = match self.artist_releases_raw(artist_id, "all", cap).await {
            Ok(raw) => raw,
            Err(err) => {
                eprintln!("artist/getReleasesList for {artist_id} failed, using artist/get: {err:#}");
                self.artist_albums_raw(artist_id, cap).await?
            }
        };
        let mut out = discography(artist_id, &raw);
        out.sort_by(|a, b| b.year().cmp(&a.year()));
        Ok(out)
    }

    /// An artist's own latest releases, newest first: albums, EPs, singles
    /// and live records. `all` would also list everyone else's covers of
    /// their songs, and for a much-covered artist those bury a year of their
    /// own releases.
    pub async fn artist_recent_releases(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        let raw = self.artist_releases_raw(artist_id, "album,epSingle,live", cap).await?;
        Ok(discography(artist_id, &raw))
    }

    /// Raw release objects for an artist, through `artist/getReleasesList`,
    /// which unlike `artist/get` reports `release_type`. Newest first.
    async fn artist_releases_raw(&self, artist_id: i64, release_type: &str, cap: usize) -> Result<Vec<Value>> {
        let call = api::artist_releases(&artist_id.to_string(), release_type, cap);
        Ok(self.gateway.send(call).await?.items)
    }

    /// Artists Qobuz considers close to this one. The same endpoint the
    /// crawler expands the frontier with.
    pub async fn similar_artists_raw(&self, artist_id: i64, limit: usize) -> Result<Vec<Value>> {
        self.gateway.send(api::similar_artists(&artist_id.to_string(), limit)).await
    }

    pub async fn similar_artists(&self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        Ok(self
            .similar_artists_raw(artist_id, limit)
            .await?
            .iter()
            .filter_map(RemoteArtist::parse)
            .collect())
    }

    /// Signed stream URL.
    pub async fn file_url(&self, track_id: i64, format_id: u32) -> Result<String> {
        Ok(self.gateway.send(api::file_url(&track_id.to_string(), format_id)).await?.url)
    }

    /// Write a path out as a real Qobuz playlist. Returns the playlist id.
    pub async fn export_playlist(&self, name: &str, track_ids: &[i64]) -> Result<i64> {
        let ids: Vec<String> = track_ids.iter().map(i64::to_string).collect();
        Ok(self.gateway.send(api::create_playlist(name, &ids)).await?.id)
    }
}

fn list(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

/// Repo root, inferred from the crate location.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}
