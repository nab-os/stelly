//! The same surface, over HTTP to a server.
//!
//! Browsing, playback URLs, the block list, the pipeline and the text tower
//! live on the server. The space is *synced*, not queried, so neighbours and
//! the sliders stay local. Audio does not proxy.

use crate::api::{
    ApiError, BlockedArtist, Corpus, CrawlStatus, Device, PairingGrant, PipelineStatus, Scope,
    Stage, SyncManifest, Target,
};
use crate::session::{Command, Session, Update};
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use crate::logbuffer::LogBuffer;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What a sync is doing, as far as someone waiting on it cares.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum SyncStep {
    /// Asking the server what it holds and comparing digests.
    #[default]
    Checking,
    Downloading,
    Done,
}

/// How far a sync has got. Totals count only the files that were stale.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncProgress {
    pub step: SyncStep,
    /// The one coming down, while `Downloading`.
    pub file: String,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

pub struct Remote {
    http: reqwest::Client,
    /// No trailing slash.
    base: String,
    token: String,
    /// Where synced artefacts land, and where the engine loads them from.
    data_dir: PathBuf,
    log: Arc<LogBuffer>,
    /// How far the running `sync_space` has got, for the loading screen.
    sync: Arc<Mutex<SyncProgress>>,
    /// Held for a whole sync. It runs behind the app now, so a second rebuild
    /// can land mid-download, and two syncs would write the same `.partial`.
    syncing: tokio::sync::Mutex<()>,
    /// Whether the SSE task is already running. The log is pushed, not polled,
    /// so a stage that prints nothing for minutes costs nothing to watch.
    streaming: Arc<AtomicBool>,
}

impl Remote {
    pub fn new(base: impl Into<String>, token: impl Into<String>, data_dir: PathBuf) -> Self {
        let base = base.into();
        Self {
            http: http_client(),
            base: base.trim_end_matches('/').to_string(),
            token: token.into(),
            data_dir,
            log: Arc::new(LogBuffer::default()),
            sync: Arc::new(Mutex::new(SyncProgress::default())),
            syncing: tokio::sync::Mutex::new(()),
            streaming: Arc::new(AtomicBool::new(false)),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Turn a non-2xx into the message the server would have logged.
    /// Deliberately not sanitised, single-user system on a private network.
    async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<ApiError>(&body)
            .map(|e| e.message)
            .unwrap_or(body);

        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("the server rejected this device's token ({status}): {message}");
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            anyhow::bail!("this device is not paired for that ({status}): {message}");
        }
        // A handler's own "not found" carries an `ApiError` body; an empty
        // 404 is axum finding no route at all, a server built before the
        // endpoint this client is asking for existed. Worth saying so, since
        // the client and the server are deployed separately and drift.
        if status == reqwest::StatusCode::NOT_FOUND && message.trim().is_empty() {
            anyhow::bail!(
                "the server does not have this feature yet (404), it is older than this \
                 app; update it to use this"
            );
        }
        anyhow::bail!("server returned {status}: {message}")
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("GET {}", self.url(path)))?;
        Ok(Self::check(response).await?.json().await?)
    }

    async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {}", self.url(path)))?;
        Ok(Self::check(response).await?.json().await?)
    }

    // -------------------------------------------------------------- browsing

    pub async fn search(&self, query: &str, limit: usize) -> Result<SearchResults> {
        self.get(&format!(
            "/api/search?q={}&limit={limit}",
            urlencode(query)
        ))
        .await
    }

    pub async fn favourite_tracks(&self, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.get(&format!("/api/favourites/tracks?cap={cap}")).await
    }

    pub async fn favourite_albums(&self, cap: usize) -> Result<Vec<RemoteAlbum>> {
        self.get(&format!("/api/favourites/albums?cap={cap}")).await
    }

    pub async fn favourite_artists(&self, cap: usize) -> Result<Vec<RemoteArtist>> {
        self.get(&format!("/api/favourites/artists?cap={cap}")).await
    }

    pub async fn playlists(&self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        self.get(&format!("/api/playlists?cap={cap}")).await
    }

    pub async fn playlist_tracks(&self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.get(&format!("/api/playlists/{playlist_id}?cap={cap}"))
            .await
    }

    pub async fn album_tracks(&self, album_id: &str) -> Result<Vec<RemoteTrack>> {
        self.get(&format!("/api/albums/{}", urlencode(album_id)))
            .await
    }

    /// Portrait and biography. A server older than this route answers 404,
    /// which the artist page treats as "nothing more to show".
    pub async fn artist(&self, artist_id: i64) -> Result<RemoteArtist> {
        self.get(&format!("/api/artists/{artist_id}")).await
    }

    pub async fn artist_albums(&self, artist_id: i64, cap: usize) -> Result<Vec<RemoteAlbum>> {
        let albums: Vec<RemoteAlbum> = self
            .get(&format!("/api/artists/{artist_id}/albums?cap={cap}"))
            .await?;
        // Filtered here as well as in `QobuzClient::artist_albums`: the
        // server and this app are deployed separately, and a server built
        // before that filter still sends every album the artist is merely
        // credited on. Idempotent against one that already filters.
        Ok(crate::qobuz::own_releases(artist_id, albums))
    }

    pub async fn similar_artists(&self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        self.get(&format!("/api/artists/{artist_id}/similar?limit={limit}"))
            .await
    }

    // -------------------------------------------------------------- playback

    pub async fn file_url(&self, track_id: i64, format_id: u32) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct Url {
            url: String,
        }
        let found: Url = self
            .get(&format!("/api/tracks/{track_id}/url?format={format_id}"))
            .await?;
        Ok(found.url)
    }

    // --------------------------------------------------------------- session

    pub async fn session(&self) -> Result<Session> {
        self.get("/api/session").await
    }

    /// Send one change to the shared session. `Ok(false)` when the server
    /// refused it for naming rows of a queue that has changed since, which is
    /// a reason to catch up rather than an error to show.
    pub async fn session_command(&self, command: &Command) -> Result<bool> {
        let response = self
            .http
            .post(self.url("/api/session"))
            .bearer_auth(&self.token)
            .json(command)
            .send()
            .await
            .with_context(|| format!("POST {}", self.url("/api/session")))?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            return Ok(false);
        }
        Self::check(response).await?;
        Ok(true)
    }

    /// Follow the shared session for as long as the receiver is kept.
    ///
    /// Reconnects on its own when the stream drops, saying so with `Lost`.
    /// Sends `Unsupported` and stops when the server is older than the
    /// session, and the app then plays on its own as it used to.
    pub fn follow_session(&self) -> tokio::sync::mpsc::UnboundedReceiver<SessionFeed> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let http = self.http.clone();
        let url = self.url("/api/session/events");
        let token = self.token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sender.closed() => return,
                    finished = read_session(&http, &url, &token, &sender) => match finished {
                        Followed::Unsupported => {
                            let _ = sender.send(SessionFeed::Unsupported);
                            return;
                        }
                        Followed::Dropped => {
                            if sender.send(SessionFeed::Lost).is_err() {
                                return;
                            }
                        }
                    },
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });

        receiver
    }

    pub async fn favorite_add(&self, kind: &str, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .post(&format!("/api/favourites/{kind}/{id}"), &serde_json::json!({}))
            .await?;
        Ok(())
    }

    pub async fn favorite_remove(&self, kind: &str, id: &str) -> Result<()> {
        let response = self
            .http
            .delete(self.url(&format!("/api/favourites/{kind}/{id}")))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::check(response).await?;
        Ok(())
    }

    pub async fn export_playlist(&self, name: &str, track_ids: &[i64]) -> Result<i64> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            name: &'a str,
            track_ids: &'a [i64],
        }
        #[derive(serde::Deserialize)]
        struct Created {
            id: i64,
        }
        let created: Created = self
            .post("/api/playlists", &Body { name, track_ids })
            .await?;
        Ok(created.id)
    }

    // --------------------------------------------------------- text steering

    pub async fn embed(&self, phrase: &str) -> Result<Vec<f32>> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            phrase: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct Embedding {
            embedding: Vec<f32>,
        }
        let found: Embedding = self.post("/api/embed", &Body { phrase }).await?;
        Ok(found.embedding)
    }

    pub async fn can_steer(&self) -> Result<bool> {
        #[derive(serde::Deserialize)]
        struct Steering {
            available: bool,
        }
        // Not being able to ask is not the same as the answer being no, but
        // it produces the same UI.
        let found: Steering = self.get("/api/embed/available").await?;
        Ok(found.available)
    }

    // ---------------------------------------------------------------- hiding

    pub async fn blocked_artists(&self) -> Result<Vec<BlockedArtist>> {
        self.get("/api/blocked").await
    }

    pub async fn block_artist(&self, artist_id: i64, name: &str) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            artist_id: i64,
            name: &'a str,
        }
        let _: serde_json::Value = self.post("/api/blocked", &Body { artist_id, name }).await?;
        Ok(())
    }

    pub async fn unblock_artist(&self, artist_id: i64) -> Result<()> {
        let response = self
            .http
            .delete(self.url(&format!("/api/blocked/{artist_id}")))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::check(response).await?;
        Ok(())
    }

    pub async fn corpus(&self) -> Result<Corpus> {
        self.get("/api/corpus").await
    }

    // -------------------------------------------------------------- crawling

    pub async fn crawl_start(&self, max_distance: i64) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body {
            max_distance: i64,
        }
        let _: serde_json::Value = self
            .post("/api/crawl/start", &Body { max_distance })
            .await?;
        Ok(())
    }

    pub async fn crawl_stop(&self) -> Result<()> {
        let _: serde_json::Value = self.post("/api/crawl/stop", &()).await?;
        Ok(())
    }

    pub async fn crawl_status(&self) -> Result<CrawlStatus> {
        self.get("/api/crawl").await
    }

    // -------------------------------------------------------------- pipeline

    pub async fn pipeline_start(&self, stage: Stage) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body {
            stage: Stage,
        }
        let _: serde_json::Value = self.post("/api/pipeline/start", &Body { stage }).await?;
        Ok(())
    }

    pub async fn pipeline_start_full(&self) -> Result<()> {
        let _: serde_json::Value = self.post("/api/pipeline/start-full", &()).await?;
        Ok(())
    }

    /// Put one track, album or artist on the map, ahead of the backlog.
    pub async fn pipeline_add(&self, target: &Target) -> Result<()> {
        let _: serde_json::Value = self.post("/api/pipeline/add", target).await?;
        Ok(())
    }

    pub async fn pipeline_stop(&self) -> Result<()> {
        let _: serde_json::Value = self.post("/api/pipeline/stop", &()).await?;
        Ok(())
    }

    pub async fn pipeline_status(&self) -> Result<PipelineStatus> {
        self.get("/api/pipeline").await
    }

    /// Read from the locally mirrored log, and make sure something is filling
    /// it. SSE rather than a poll, so a silent `layout` costs one idle socket.
    pub async fn pipeline_log(&self) -> Result<Vec<String>> {
        self.ensure_streaming();
        Ok(self.log.all())
    }

    pub async fn pipeline_clear_log(&self) -> Result<()> {
        // Local only. The server's buffer is shared by every device, so one
        // phone clearing its view must not blank the desktop's.
        self.log.clear();
        Ok(())
    }

    fn ensure_streaming(&self) {
        if self.streaming.swap(true, Ordering::SeqCst) {
            return;
        }

        let http = self.http.clone();
        let url = self.url("/api/pipeline/log");
        let token = self.token.clone();
        let log = self.log.clone();
        let streaming = self.streaming.clone();

        tokio::spawn(async move {
            // One reconnecting reader for the life of the process. A dropped
            // connection is ordinary, so it backs off rather than freezing the
            // view on the last line it saw.
            loop {
                match http.get(&url).bearer_auth(&token).send().await {
                    Ok(response) if response.status().is_success() => {
                        let mut stream = response.bytes_stream();
                        let mut buffer = String::new();

                        while let Some(Ok(chunk)) = stream.next().await {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));

                            // SSE frames are separated by a blank line; each
                            // `data:` field is one log line.
                            while let Some(end) = buffer.find("\n\n") {
                                let frame = buffer[..end].to_string();
                                buffer.drain(..end + 2);
                                for line in frame.lines() {
                                    if let Some(rest) = line.strip_prefix("data:") {
                                        log.push(rest.trim_start().to_string());
                                    }
                                }
                            }
                        }
                    }
                    Ok(response) => {
                        log.push(format!("log stream refused: {}", response.status()));
                        streaming.store(false, Ordering::SeqCst);
                        return;
                    }
                    Err(err) => {
                        log.push(format!("log stream lost: {err}"));
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    }

    // ----------------------------------------------------------- the space

    /// Bring the local copy of the space up to date. Digest-compared per file,
    /// not by generation alone: a restart resets the counter.
    pub async fn sync_space(&self) -> Result<bool> {
        let _syncing = self.syncing.lock().await;
        self.set_sync(SyncProgress::default());
        let manifest: SyncManifest = self.get("/api/sync/manifest").await?;
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating {}", self.data_dir.display()))?;

        let stale: Vec<_> = manifest
            .files
            .iter()
            .filter(|file| {
                local_digest(&self.data_dir.join(&file.name)).as_deref() != Some(file.digest.as_str())
            })
            .collect();
        let mut progress = SyncProgress {
            step: SyncStep::Downloading,
            files_total: stale.len(),
            bytes_total: stale.iter().map(|file| file.bytes).sum(),
            ..SyncProgress::default()
        };

        for file in &stale {
            progress.file = file.name.clone();
            self.set_sync(progress.clone());

            let target = self.data_dir.join(&file.name);
            let response = self
                .http
                .get(self.url(&format!("/api/sync/{}", urlencode(&file.name))))
                .bearer_auth(&self.token)
                .send()
                .await
                .with_context(|| format!("downloading {}", file.name))?;
            // Comes down zstd-compressed and is inflated as it arrives, so
            // the counts below are still in the manifest's uncompressed bytes.
            let mut body = Self::check(response).await?.bytes_stream();

            // Write beside the target and rename: an interrupted sync must
            // not leave a half-written space.bin for the engine to map.
            let staging = target.with_extension("partial");
            let mut out = std::fs::File::create(&staging)
                .with_context(|| format!("writing {}", staging.display()))?;
            while let Some(chunk) = body.next().await {
                let chunk = chunk.with_context(|| format!("downloading {}", file.name))?;
                std::io::Write::write_all(&mut out, &chunk)
                    .with_context(|| format!("writing {}", staging.display()))?;
                progress.bytes_done += chunk.len() as u64;
                self.set_sync(progress.clone());
            }
            drop(out);
            std::fs::rename(&staging, &target)
                .with_context(|| format!("replacing {}", target.display()))?;
            progress.files_done += 1;
        }

        progress.step = SyncStep::Done;
        self.set_sync(progress);
        Ok(!stale.is_empty())
    }

    /// Where the running sync is, or where the last one stopped.
    pub fn sync_progress(&self) -> SyncProgress {
        self.sync.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_sync(&self, progress: SyncProgress) {
        *self.sync.lock().unwrap_or_else(|e| e.into_inner()) = progress;
    }

    // ------------------------------------------------------------ devices

    pub async fn devices(&self) -> Result<Vec<Device>> {
        self.get("/api/devices").await
    }

    pub async fn pair_device(&self, name: &str, scope: Scope) -> Result<PairingGrant> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            name: &'a str,
            scope: Scope,
        }
        self.post("/api/devices", &Body { name, scope }).await
    }

    pub async fn revoke_device(&self, device_id: i64) -> Result<()> {
        let response = self
            .http
            .delete(self.url(&format!("/api/devices/{device_id}")))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::check(response).await?;
        Ok(())
    }
}

/// What the session stream says, as `follow_session` passes it on.
pub enum SessionFeed {
    Update(Update),
    /// The stream dropped and is being reopened. The app keeps its copy.
    Lost,
    /// The server has no session to follow.
    Unsupported,
}

enum Followed {
    Dropped,
    Unsupported,
}

/// The server sends a keep-alive every 15s, so this long without a byte means
/// the connection died without saying so, as a phone's does when it changes
/// networks.
const SILENCE: std::time::Duration = std::time::Duration::from_secs(45);

/// Read one connection's worth of the session stream.
async fn read_session(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    sender: &tokio::sync::mpsc::UnboundedSender<SessionFeed>,
) -> Followed {
    let response = match http.get(url).bearer_auth(token).send().await {
        Ok(response) => response,
        Err(err) => {
            eprintln!("session stream: {err}");
            return Followed::Dropped;
        }
    };
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Followed::Unsupported;
    }
    if !response.status().is_success() {
        eprintln!("session stream refused: {}", response.status());
        return Followed::Dropped;
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Ok(Some(Ok(chunk))) = tokio::time::timeout(SILENCE, stream.next()).await {
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            let data: String = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect();
            if data.is_empty() {
                // A keep-alive, which is only a comment.
                continue;
            }
            match serde_json::from_str::<Update>(&data) {
                Ok(update) => {
                    if sender.send(SessionFeed::Update(update)).is_err() {
                        return Followed::Dropped;
                    }
                }
                Err(err) => eprintln!("session update unreadable: {err}"),
            }
        }
    }
    Followed::Dropped
}

#[cfg(not(target_os = "android"))]
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

/// reqwest's default verifier is rustls-platform-verifier, which on Android
/// calls into Java and panics unless it was handed a JNI context at startup,
/// and the Kotlin half it calls is not in the APK anyway. The panic lands in
/// the task awaiting the first https request, so pairing sat on "connecting…"
/// forever, while plain http never touched it. Instead, verify with webpki
/// against the certificates Android itself trusts, read off disk: the system
/// store, the one Conscrypt updates through its APEX on 14+, and whatever
/// the user installed, which is how a private CA in front of nginx gets in.
#[cfg(target_os = "android")]
pub(crate) fn http_client() -> reqwest::Client {
    const STORES: [&str; 3] = [
        "/apex/com.android.conscrypt/cacerts",
        "/system/etc/security/cacerts",
        "/data/misc/user/0/cacerts-added",
    ];

    let certs: Vec<reqwest::Certificate> = STORES
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| std::fs::read(entry.ok()?.path()).ok())
        .filter_map(|pem| reqwest::Certificate::from_pem(&pem).ok())
        .collect();
    log::info!("trusting {} CA certificates from the system", certs.len());

    reqwest::Client::builder()
        .tls_certs_only(certs)
        .build()
        .expect("a client with only root certificates configured")
}

/// Hex md5 of a file, or None if it is not there. md5 because it is already a
/// dependency for request signing, and this is a cache key.
fn local_digest(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(crate::api::digest(&bytes))
}

/// Percent-encode the handful of characters that actually turn up in a search
/// phrase or an album id. Not a general encoder, and does not need to be.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
