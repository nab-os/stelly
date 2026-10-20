//! The same surface, over HTTP to a server.
//!
//! Browsing, playback URLs, the block list, the pipeline and the space all
//! live on the server. The space is queried, not synced: a phone has nothing
//! to download before it can generate. Audio does not proxy.

use crate::api::{
    ApiError, BlockedArtist, Corpus, CrawlStatus, Device, GenerateRequest, OrderRequest,
    PairingGrant, PipelineStatus, Scope, SpaceIndex, SpaceInfo, SpaceSearch, SpaceSort, Stage,
    Step, Target, TrackMeta, TracksRequest, User, Word,
};
use crate::session::{Command, Session, Update};
use crate::catalogue::{Quality, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults};
use crate::logbuffer::LogBuffer;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::sync::{Arc, Mutex};

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

/// How the server took a session command.
pub enum Sent {
    Applied,
    /// Refused for naming rows of a queue that has changed since, which is a
    /// reason to catch up rather than an error to show.
    Stale,
    /// The server is older than the op.
    Unknown,
}

/// Who is following the pipeline log, and the task doing it.
#[derive(Default)]
struct Following {
    views: usize,
    task: Option<tokio::task::AbortHandle>,
}

pub struct Remote {
    http: reqwest::Client,
    /// No trailing slash.
    base: String,
    token: String,
    log: Arc<LogBuffer>,
    /// The SSE task and how many views show the log. Open only while one
    /// does: every client holding a socket for its whole life adds up.
    following: Mutex<Following>,
}

impl Remote {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        let base = base.into();
        Self {
            http: http_client(),
            base: base.trim_end_matches('/').to_string(),
            token: token.into(),
            log: Arc::new(LogBuffer::default()),
            following: Mutex::new(Following::default()),
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

    /// The server's address, as paired.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The token this device was paired with, for keeping it across a move
    /// to another address.
    pub fn token(&self) -> &str {
        &self.token
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
        self.get_within(path, REQUEST_TIMEOUT).await
    }

    async fn get_within<T: DeserializeOwned>(&self, path: &str, timeout: std::time::Duration) -> Result<T> {
        let body = self
            .exchange("GET", path, self.http.get(self.url(path)).timeout(timeout))
            .await?;
        Ok(serde_json::from_slice(body.as_ref())?)
    }

    async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        self.post_within(path, body, REQUEST_TIMEOUT).await
    }

    async fn post_within<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: std::time::Duration,
    ) -> Result<T> {
        let body = self
            .exchange("POST", path, self.http.post(self.url(path)).json(body).timeout(timeout))
            .await?;
        Ok(serde_json::from_slice(body.as_ref())?)
    }

    /// Send a request and read all of the answer, or say what the server
    /// refused it with. Everything but the session command and the two
    /// streams comes through here, which is where `timings` sees it.
    async fn exchange(
        &self,
        method: &str,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<impl AsRef<[u8]>> {
        let started = crate::timings::clock();
        let sent = request.bearer_auth(&self.token).send().await;
        let status = sent.as_ref().ok().map(|response| response.status().as_u16());
        let body = async {
            let response = sent.with_context(|| format!("{method} {}", self.url(path)))?;
            anyhow::Ok(Self::check(response).await?.bytes().await?)
        }
        .await;
        let bytes = body.as_ref().map_or(0, |body| body.len());
        crate::timings::http(method, path, status, started, bytes);
        body
    }

    // -------------------------------------------------------------- browsing

    /// `only` names the one provider to ask; none is all of them.
    pub async fn search(&self, query: &str, limit: usize, only: Option<&str>) -> Result<SearchResults> {
        let only = only.map(|name| format!("&providers={}", urlencode(name))).unwrap_or_default();
        self.get(&format!("/api/search?q={}&limit={limit}{only}", urlencode(query))).await
    }

    /// Each provider, most preferred first.
    pub async fn providers(&self) -> Result<Vec<crate::catalogue::Capabilities>> {
        self.get("/api/providers").await
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

    /// What the liked artists put out in the last `days`, newest first. One
    /// Qobuz request per liked artist the server has not asked about in the
    /// last few hours, at two a second, so the first time can take minutes.
    pub async fn recent_releases(&self, days: u32) -> Result<Vec<RemoteAlbum>> {
        self.get_within(&format!("/api/releases?days={days}"), RELEASES_TIMEOUT)
            .await
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
        // Every album that credits them, each marked with how; the artist
        // page sorts them into sections. A server from before the marks sends
        // none, which `RemoteAlbum::credit_on` covers.
        self.get(&format!("/api/artists/{artist_id}/albums?cap={cap}")).await
    }

    pub async fn similar_artists(&self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        self.get(&format!("/api/artists/{artist_id}/similar?limit={limit}"))
            .await
    }

    // -------------------------------------------------------------- playback

    pub async fn file_url(&self, track_id: i64, quality: Quality) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct Url {
            url: String,
        }
        let found: Url = self
            .get(&format!("/api/tracks/{track_id}/url?quality={}", quality.as_str()))
            .await?;
        Ok(found.url)
    }

    // --------------------------------------------------------------- session

    pub async fn session(&self) -> Result<Session> {
        self.get("/api/session").await
    }

    /// Send one change to the shared session.
    ///
    /// Commands go out one at a time, so one that hangs holds up every play
    /// and skip behind it: hence a short timeout, and the caller catching up
    /// once it runs out.
    pub async fn session_command(&self, command: &Command) -> Result<Sent> {
        let started = crate::timings::clock();
        let send = || {
            self.http
                .post(self.url("/api/session"))
                .bearer_auth(&self.token)
                .json(command)
                .timeout(SESSION_TIMEOUT)
                .send()
        };
        let sent = retry_unsent(send).await;
        let status = sent.as_ref().ok().map(|response| response.status().as_u16());
        crate::timings::http("POST", "/api/session", status, started, 0);
        let response = sent.with_context(|| format!("POST {}", self.url("/api/session")))?;
        match response.status() {
            reqwest::StatusCode::CONFLICT => return Ok(Sent::Stale),
            // Axum's answer to a body it cannot read: an op newer than the
            // server.
            reqwest::StatusCode::UNPROCESSABLE_ENTITY => return Ok(Sent::Unknown),
            _ => {}
        }
        Self::check(response).await?;
        Ok(Sent::Applied)
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
        let path = format!("/api/favourites/{kind}/{id}");
        self.exchange("DELETE", &path, self.http.delete(self.url(&path)).timeout(REQUEST_TIMEOUT)).await?;
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
        let path = format!("/api/blocked/{artist_id}");
        self.exchange("DELETE", &path, self.http.delete(self.url(&path)).timeout(REQUEST_TIMEOUT)).await?;
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

    /// The locally mirrored log, filled while it is followed.
    pub fn pipeline_log(&self) -> Vec<String> {
        self.log.all()
    }

    /// How many lines have come in, for `pipeline_log_past`.
    pub fn pipeline_log_cursor(&self) -> u64 {
        self.log.cursor()
    }

    /// Returns once a line has come in past `since`.
    pub async fn pipeline_log_past(&self, since: u64) {
        self.log.past(since).await;
    }

    pub async fn pipeline_clear_log(&self) -> Result<()> {
        // Local only. The server's buffer is shared by every device, so one
        // phone clearing its view must not blank the desktop's.
        self.log.clear();
        Ok(())
    }

    /// Follow the server's log while a view shows it. SSE rather than a
    /// poll, so a silent `layout` costs one idle socket. Pair with
    /// `unfollow_pipeline_log`.
    pub fn follow_pipeline_log(&self) {
        let mut following = self.following.lock().unwrap_or_else(|e| e.into_inner());
        following.views += 1;
        if following.task.is_none() {
            following.task = Some(self.stream_log());
        }
    }

    /// The last view showing the log is gone: drop the socket.
    pub fn unfollow_pipeline_log(&self) {
        let mut following = self.following.lock().unwrap_or_else(|e| e.into_inner());
        following.views = following.views.saturating_sub(1);
        if following.views == 0 {
            if let Some(task) = following.task.take() {
                task.abort();
            }
        }
    }

    fn stream_log(&self) -> tokio::task::AbortHandle {
        let http = self.http.clone();
        let url = self.url("/api/pipeline/log");
        let token = self.token.clone();
        let log = self.log.clone();

        let task = tokio::spawn(async move {
            // One reconnecting reader for as long as the log is shown. A
            // dropped connection is ordinary, so it backs off rather than
            // freezing the view on the last line it saw.
            loop {
                let started = crate::timings::clock();
                let sent = http.get(&url).bearer_auth(&token).send().await;
                let status = sent.as_ref().ok().map(|response| response.status().as_u16());
                crate::timings::http("GET", "/api/pipeline/log", status, started, 0);
                match sent {
                    Ok(response) if response.status().is_success() => {
                        let mut stream = response.bytes_stream();
                        let mut buffer = String::new();
                        // Each connection opens with the server's retained
                        // tail, which replaces what is held rather than
                        // following it, or a reopened view shows it twice.
                        let mut replaced = false;

                        while let Some(Ok(chunk)) = stream.next().await {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));

                            // SSE frames are separated by a blank line; each
                            // `data:` field is one log line.
                            while let Some(end) = buffer.find("\n\n") {
                                let frame = buffer[..end].to_string();
                                buffer.drain(..end + 2);
                                for line in frame.lines() {
                                    if let Some(rest) = line.strip_prefix("data:") {
                                        if !replaced {
                                            log.clear();
                                            replaced = true;
                                        }
                                        log.push(rest.trim_start().to_string());
                                    }
                                }
                            }
                        }
                    }
                    Ok(response) => {
                        // Tried again the next time the log is opened.
                        log.push(format!("log stream refused: {}", response.status()));
                        return;
                    }
                    Err(err) => {
                        log.push(format!("log stream lost: {err}"));
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
        task.abort_handle()
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
        Ok(self.exchange("GET", path, self.http.get(self.url(path)).timeout(POINTS_TIMEOUT)).await?.as_ref().to_vec())
    }

    /// A track of the space in words, the ones it sounds like most next to
    /// the rest of the space, best first.
    pub async fn describe(&self, track_id: i64) -> Result<Vec<Word>> {
        self.get(&format!("/api/space/describe/{track_id}")).await
    }

    pub async fn generate(&self, request: &GenerateRequest) -> Result<Vec<Step>> {
        // Shorter than the rest: a button says "working…" for as long as
        // this takes, and a walk the server can do at all is done in seconds.
        self.post_within("/api/space/generate", request, std::time::Duration::from_secs(30))
            .await
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
    pub async fn capabilities(&self) -> Result<crate::catalogue::Capabilities> {
        self.get("/api/capabilities").await
    }

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
        let path = format!("/api/devices/{device_id}");
        self.exchange("DELETE", &path, self.http.delete(self.url(&path)).timeout(REQUEST_TIMEOUT)).await?;
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
    let started = crate::timings::clock();
    let sent = http.get(url).bearer_auth(token).send().await;
    let status = sent.as_ref().ok().map(|response| response.status().as_u16());
    crate::timings::http("GET", "/api/session/events", status, started, 0);
    let response = match sent {
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
        .pool_idle_timeout(IDLE_TIMEOUT)
        .tcp_keepalive(KEEPALIVE)
        .build()
        .expect("a client with only timeouts configured")
}

/// How long to wait for a server to answer at all. Short: on a phone with a
/// poor connection a request that will fail should say so while the cached
/// copy is still on screen, not after the OS gives up. Never an overall
/// timeout, the session and the log are streams that stay open.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// How long a plain request may take from start to end, for everything but
/// the streams. The connect timeout alone does not cover a pooled connection
/// that died with the server: reusing it skips connecting, and the request
/// then waits on the OS to give up, minutes, with a button stuck on
/// "working…" all along. Generous, since a long favourites list is paged
/// through Qobuz behind one request.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A session command is small and the server applies it at once, so this
/// long means the connection is gone, not that the server is busy.
const SESSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The map's points run to megabytes, which on a phone's connection can take
/// a while to come down.
const POINTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// A pooled connection unused for this long is closed rather than reused: a
/// phone that changed networks since has nothing at the other end of it.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Probes on every open connection, so one that died without a word is
/// noticed by the OS and dropped from the pool. The session stream's own
/// keep-alive still decides when that one has gone quiet for too long.
const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(30);

/// Send, and send again once if the first never got as far as a connection.
/// Only then: a request that went out may have been applied, and an append
/// sent twice is a track queued twice.
async fn retry_unsent<F, Fut, T>(send: F) -> reqwest::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<T>>,
{
    match send().await {
        Err(err) if err.is_connect() => send().await,
        sent => sent,
    }
}

/// Recent releases cost the server one Qobuz request per liked artist when
/// its cache is cold: five hundred artists at two a second is four minutes.
const RELEASES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// The address could not even be made into a request: a stray character,
/// a port out of range.
pub fn is_bad_address(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|err| err.is_builder())
}

/// The server could not be reached or did not answer in time, as opposed to
/// answering with an error of its own.
pub fn is_unreachable(err: &anyhow::Error) -> bool {
    if is_bad_address(err) {
        return false;
    }
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|err| err.is_connect() || err.is_timeout() || err.is_request())
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
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(IDLE_TIMEOUT)
        .tcp_keepalive(KEEPALIVE)
        .build()
        .expect("a client with only root certificates and timeouts configured")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Count the attempts at one address.
    async fn attempts(url: &str, timeout: Duration) -> (usize, reqwest::Error) {
        let http = http_client();
        let tries = AtomicUsize::new(0);
        let err = retry_unsent(|| {
            tries.fetch_add(1, Ordering::SeqCst);
            http.post(url).timeout(timeout).send()
        })
        .await
        .expect_err("nothing there to answer");
        (tries.into_inner(), err)
    }

    #[tokio::test]
    async fn a_refused_connection_is_tried_again() {
        // A port that was free a moment ago, and so refuses.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (tries, err) = attempts(&format!("http://127.0.0.1:{port}/"), SESSION_TIMEOUT).await;
        assert!(err.is_connect());
        assert_eq!(tries, 2);
    }

    #[tokio::test]
    async fn a_request_that_went_out_is_not_sent_twice() {
        // Connects, through the backlog, and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tries, err) = attempts(&format!("http://127.0.0.1:{port}/"), Duration::from_millis(200)).await;
        assert!(err.is_timeout() && !err.is_connect());
        assert_eq!(tries, 1);
        drop(listener);
    }

    /// A server that answers each connect with the same two-line tail and
    /// holds the stream open, handing over each accepted socket.
    async fn log_server() -> (String, tokio::sync::mpsc::UnboundedReceiver<tokio::net::TcpStream>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: one\n\ndata: two\n\n",
                    )
                    .await;
                let _ = sender.send(socket);
            }
        });
        (base, receiver)
    }

    async fn settled(remote: &Remote, want: &[&str]) {
        let want: Vec<String> = want.iter().map(|line| line.to_string()).collect();
        tokio::time::timeout(Duration::from_secs(2), async {
            while remote.pipeline_log() != want {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the log should settle");
    }

    #[tokio::test]
    async fn the_log_is_followed_only_while_shown() {
        use tokio::io::AsyncReadExt;
        let (base, mut sockets) = log_server().await;
        let remote = Remote::new(base, "token");

        remote.follow_pipeline_log();
        remote.follow_pipeline_log();
        let mut socket = sockets.recv().await.unwrap();
        settled(&remote, &["one", "two"]).await;

        // One of two views gone: still followed.
        remote.unfollow_pipeline_log();
        let mut byte = [0u8; 1];
        let open = tokio::time::timeout(Duration::from_millis(100), socket.read(&mut byte)).await;
        assert!(open.is_err(), "the socket should stay open");

        // The last one gone: the socket closes.
        remote.unfollow_pipeline_log();
        let read = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await;
        assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "the socket should close");

        // Reopened, the replayed tail replaces the one held.
        remote.follow_pipeline_log();
        let _socket = sockets.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        settled(&remote, &["one", "two"]).await;
        remote.unfollow_pipeline_log();
    }
}
