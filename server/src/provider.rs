//! The music provider: browsing, stream URLs, playlists, and the bulk reads
//! the crawl and the analyser make, all through the provider protocol.
//!
//! The provider is a service of its own, found at `STELLY_PROVIDER_URL`; it
//! holds whatever account the catalogue needs, and its request budget,
//! shared with whatever else on the machine asks it. It answers in
//! `stelly_provider`'s shapes, the same `stelly_core::catalogue` hands the
//! app. Browsing queues as interactive, and the crawl and analyse as bulk,
//! so a search never waits behind them.

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::{Path, PathBuf};
use stelly_provider::{api, AlbumTracks, Client, Config, FileUrl, Kind, Priority};

pub use stelly_core::catalogue::{Capabilities, Quality, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
pub use stelly_provider::Account;

/// Cheap to clone; clones share the provider's connections.
#[derive(Clone)]
pub struct Provider {
    client: Client,
}

impl Provider {
    /// The provider at `STELLY_PROVIDER_URL`, with `STELLY_PROVIDER_TOKEN`,
    /// from the environment over the .env in `env_dir`; by default one on
    /// this machine.
    pub fn from_env(env_dir: &Path) -> Self {
        Self {
            client: Client::new(Config::from_env(Some(&env_dir.join(".env")))),
        }
    }

    /// The same provider, this client's requests queued behind browsing: for
    /// the crawl and analyse, which can wait.
    pub fn bulk(&self) -> Self {
        Self {
            client: self.client.at(Priority::Bulk),
        }
    }

    pub fn url(&self) -> &str {
        self.client.url()
    }

    /// What it is, and which of the optional calls it answers.
    pub async fn capabilities(&self) -> Result<Capabilities> {
        self.client.send(api::capabilities()).await
    }

    /// Who it is signed in as, which is also the check that it can answer.
    pub async fn account(&self) -> Result<Account> {
        self.client.send(api::account()).await
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults> {
        self.client.send(api::search(query, limit)).await
    }

    /// The account's own likes, on providers that keep any.
    pub async fn favourite_tracks(&self, cap: usize) -> Result<Vec<RemoteTrack>> {
        Ok(self.client.send(api::favourite_tracks(cap)).await?.items)
    }

    pub async fn favourite_albums(&self, cap: usize) -> Result<Vec<RemoteAlbum>> {
        Ok(self.client.send(api::favourite_albums(cap)).await?.items)
    }

    pub async fn favourite_artists(&self, cap: usize) -> Result<Vec<RemoteArtist>> {
        Ok(self.client.send(api::favourite_artists(cap)).await?.items)
    }

    pub async fn user_playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        Ok(self.client.send(api::playlists(cap)).await?.items)
    }

    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        Ok(self.client.send(api::playlist_tracks(playlist_id, cap)).await?.items)
    }

    pub async fn track(&self, track_id: i64) -> Result<RemoteTrack> {
        self.client.send(api::track(track_id)).await
    }

    /// An album and its tracklist, in one request.
    pub async fn album(&self, album_id: &str) -> Result<AlbumTracks> {
        self.client.send(api::album(album_id)).await
    }

    pub async fn album_tracks(&self, album_id: &str) -> Result<(RemoteAlbum, Vec<RemoteTrack>)> {
        let AlbumTracks { album, tracks } = self.album(album_id).await?;
        Ok((album, tracks))
    }

    /// One artist on their own: name, portrait and biography, no discography.
    pub async fn artist(&self, artist_id: i64) -> Result<RemoteArtist> {
        self.client.send(api::artist(artist_id)).await
    }

    /// An artist's discography, newest first, each album marked with what
    /// sort of release it is and how they are credited on it.
    pub async fn artist_albums(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        Ok(self.client.send(api::artist_releases(artist_id, false, cap)).await?.items)
    }

    /// An artist's own latest releases, newest first: albums, EPs, singles
    /// and live records, none of everyone else's covers of their songs.
    pub async fn artist_recent_releases(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        Ok(self.client.send(api::artist_releases(artist_id, true, cap)).await?.items)
    }

    /// Artists the provider considers close to this one. The same call the
    /// crawler expands the frontier with.
    pub async fn similar_artists(&self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        self.client.send(api::similar_artists(artist_id, limit)).await
    }

    /// Where a track's audio is, in up to `quality`, and what is there.
    pub async fn file(&self, track_id: i64, quality: Quality) -> Result<FileUrl> {
        self.client.send(api::file_url(track_id, quality)).await
    }

    pub async fn file_url(&self, track_id: i64, quality: Quality) -> Result<String> {
        Ok(self.file(track_id, quality).await?.url)
    }

    /// Write a path out as a playlist of the provider's. Returns its id.
    pub async fn export_playlist(&self, name: &str, track_ids: &[i64]) -> Result<i64> {
        Ok(self.client.send(api::create_playlist(name, track_ids)).await?.id)
    }

    /// Items in the provider's own shapes, as an earlier Stelly stored them,
    /// in the protocol's.
    pub async fn import<T: DeserializeOwned>(&self, kind: Kind, items: &[Value]) -> Result<Vec<Option<T>>> {
        self.client.send(api::import(kind, items)).await
    }
}

/// Repo root, inferred from the crate location.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}
