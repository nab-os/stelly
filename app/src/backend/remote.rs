//! The same surface, over HTTP to a server.
//!
//! Browsing, playback URLs, the block list, the pipeline and the space all
//! live on the server. The space is queried, not synced: a phone has nothing
//! to download before it can generate. Audio does not proxy.

use crate::api::{
    ApiError, BlockedArtist, Corpus, CrawlStatus, Device, GenerateRequest, OrderRequest,
    PairingGrant, PipelineStatus, Scope, SpaceIndex, SpaceInfo, SpaceSearch, SpaceSort, Stage,
    Step, Target, TrackMeta, TracksRequest, User, Imported,
};
use crate::session::{Command, Session, Update};
use crate::qobuz::{RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use crate::logbuffer::LogBuffer;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The token was refused: not a passing failure, the pairing itself is gone.
#[derive(Debug)]
pub struct Unauthorised(String);

impl std::fmt::Display for Unauthorised {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unauthorised {}

pub fn is_unauthorised(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Unauthorised>().is_some()
}

pub struct Remote {
    http: reqwest::Client,
    /// No trailing slash.
    base: String,
    token: String,
    log: Arc<LogBuffer>,
    /// Whether the SSE task is already running. The log is pushed, not polled,
    /// so a stage that prints nothing for minutes costs nothing to watch.
    streaming: Arc<AtomicBool>,
}

impl Remote {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        let base = base.into();
        Self {
            http: http_client(),
            base: base.trim_end_matches('/').to_string(),
            token: token.into(),
            log: Arc::new(LogBuffer::default()),
            streaming: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Names this pairing without saying anything about the token, for
    /// keeping what one pairing cached apart from another's. Only has to be
    /// stable for as long as the cache is, a toolchain changing it costs one
    /// cold start.
    pub fn cache_key(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (&self.base, &self.token).hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Turn a non-2xx into the message the server would have logged.
    /// Deliberately not sanitised, a family's server on a private network.
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
            return Err(Unauthorised(format!(
                "the server rejected this device's token ({status}): {message}"
            ))
            .into());
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

    /// Copy the Qobuz account's favourites into this user's likes.
    pub async fn import_favourites(&self) -> Result<Imported> {
        self.post("/api/favourites/import", &serde_json::json!({})).await
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

    pub async fn space_info(&self) -> Result<SpaceInfo> {
        self.get("/api/space").await
    }

    pub async fn space_index(&self) -> Result<SpaceIndex> {
        self.get("/api/space/index").await
    }

    /// The space's rows for these ids, leaving out any it does not hold.
    pub async fn space_tracks(&self, ids: Vec<i64>) -> Result<Vec<TrackMeta>> {
        self.post("/api/space/tracks", &TracksRequest { ids }).await
    }

    pub async fn space_search(
        &self,
        query: &str,
        sort: SpaceSort,
        descending: bool,
        limit: usize,
    ) -> Result<SpaceSearch> {
        let sort = match sort {
            SpaceSort::Rank => "rank",
            SpaceSort::Title => "title",
            SpaceSort::Artist => "artist",
            SpaceSort::Album => "album",
        };
        self.get(&format!(
            "/api/space/search?q={}&sort={sort}&desc={descending}&limit={limit}",
            urlencode(query)
        ))
        .await
    }

    /// The map's points, `map::payload`'s binary, or with `meta` its JSON.
    pub async fn space_points(&self, meta: bool) -> Result<Vec<u8>> {
        let path = if meta { "/api/space/points/meta" } else { "/api/space/points" };
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        Ok(Self::check(response).await?.bytes().await?.to_vec())
    }

    pub async fn generate(&self, request: &GenerateRequest) -> Result<Vec<Step>> {
        self.post("/api/space/generate", request).await
    }

    /// The queue's upcoming tracks in an order that flows, those the space
    /// does not hold left out.
    pub async fn order(&self, request: &OrderRequest) -> Result<Vec<i64>> {
        self.post("/api/space/order", request).await
    }

    // ------------------------------------------------------------ devices

    pub async fn devices(&self) -> Result<Vec<Device>> {
        self.get("/api/devices").await
    }

    /// For `user`, by name, or for this device's own user.
    pub async fn pair_device(&self, name: &str, scope: Scope, user: Option<&str>) -> Result<PairingGrant> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            name: &'a str,
            scope: Scope,
            #[serde(skip_serializing_if = "Option::is_none")]
            user: Option<&'a str>,
        }
        self.post("/api/devices", &Body { name, scope, user }).await
    }

    /// This device, whose it is and what it may do.
    pub async fn me(&self) -> Result<Device> {
        self.get("/api/me").await
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        self.get("/api/users").await
    }

    pub async fn add_user(&self, name: &str) -> Result<User> {
        self.post("/api/users", &serde_json::json!({ "name": name })).await
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
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("a client with only a connect timeout configured")
}

/// How long to wait for a server to answer at all. Short: on a phone with a
/// poor connection a request that will fail should say so while the cached
/// copy is still on screen, not after the OS gives up. Never an overall
/// timeout, the session and the log are streams that stay open.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

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
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("a client with only root certificates configured")
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
