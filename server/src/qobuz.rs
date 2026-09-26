//! The Qobuz client: browsing, stream URLs, playlists, and the bulk reads the
//! crawl and the analyser make. The one place credentials are held.
//!
//! The shapes it hands out are `two_khz::qobuz`'s, shared with the app.

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";

pub use two_khz::qobuz::{
    own_releases, RemoteAlbum, RemoteArtist, RemotePlaylist, RemoteTrack, SearchResults, FORMAT_MP3_320,
};

/// Requests per second, and how many may be spent at once. One budget for the
/// whole account, whoever is spending it.
pub const DEFAULT_RATE_PER_SEC: f64 = 2.0;
const BURST: f64 = 4.0;

/// Qobuz blocks programmatic user/login at their edge, so a token lifted from
/// a logged-in web player session is the only way in. See TOKEN_HELP.
pub const TOKEN_HELP: &str = "\
A user auth token is required. Get it from a logged-in browser session:
  1. Log in at https://play.qobuz.com/
  2. devtools -> Application -> Local Storage -> play.qobuz.com
  3. Copy the value of the `localuser.token` key
Then add to .env:  QOBUZ_USER_AUTH_TOKEN=<the token>";

#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub app_id: String,
    pub secrets: Vec<String>,
    pub email: Option<String>,
    pub password_md5: Option<String>,
    pub auth_token: Option<String>,
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_env(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.insert(
                key.trim().to_string(),
                value.trim().trim_matches(['\'', '"']).to_string(),
            );
        }
    }
    out
}

impl Credentials {
    /// The qobuz_stream .env supplies app_id/app_secret, the repo .env supplies
    /// the login, the environment wins.
    pub fn from_env(repo_root: &Path) -> Result<Self> {
        let mut merged = parse_env(&repo_root.join("..").join("qobuz_stream").join(".env"));
        merged.extend(parse_env(&repo_root.join(".env")));
        for key in [
            "QOBUZ_APP_ID",
            "QOBUZ_APP_SECRET",
            "QOBUZ_APP_SECRETS",
            "QOBUZ_EMAIL",
            "QOBUZ_PASSWORD",
            "QOBUZ_USER_AUTH_TOKEN",
        ] {
            if let Ok(value) = std::env::var(key) {
                if !value.is_empty() {
                    merged.insert(key.to_string(), value);
                }
            }
        }

        let Some(app_id) = merged.get("QOBUZ_APP_ID").cloned() else {
            bail!("QOBUZ_APP_ID not found in .env")
        };

        let mut secrets: Vec<String> = merged
            .get("QOBUZ_APP_SECRETS")
            .map(|s| s.split(',').map(|v| v.trim().to_string()).collect())
            .unwrap_or_default();
        if let Some(secret) = merged.get("QOBUZ_APP_SECRET") {
            secrets.push(secret.clone());
        }

        let password_md5 = merged.get("QOBUZ_PASSWORD").map(|pw| {
            let is_md5 = pw.len() == 32 && pw.chars().all(|c| c.is_ascii_hexdigit());
            if is_md5 {
                pw.to_lowercase()
            } else {
                md5_hex(pw)
            }
        });

        Ok(Self {
            app_id,
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
            email: merged.get("QOBUZ_EMAIL").cloned(),
            password_md5,
            auth_token: merged.get("QOBUZ_USER_AUTH_TOKEN").cloned(),
        })
    }
}

struct TokenBucket {
    tokens: f64,
    last: Instant,
}

/// The shared spend against one Qobuz account.
///
/// Separable from the client because there can be several clients and only one
/// account: a background crawl has its own, and the server answers several
/// devices. All of them draw from this bucket, or the budget becomes 2/s per
/// caller rather than per account.
#[derive(Clone)]
pub struct RateLimit(Arc<Mutex<TokenBucket>>);

impl RateLimit {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(TokenBucket {
            tokens: BURST,
            last: Instant::now(),
        })))
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::new()
    }
}

pub struct QobuzClient {
    credentials: Credentials,
    http: reqwest::Client,
    auth_token: Option<String>,
    working_secret: Option<String>,
    limiter: RateLimit,
    rate_per_sec: f64,
}

impl QobuzClient {
    pub fn new(credentials: Credentials) -> Self {
        Self::sharing(credentials, RateLimit::new())
    }

    /// A second client drawing on the same budget as an existing one.
    pub fn sharing(credentials: Credentials, limiter: RateLimit) -> Self {
        let auth_token = credentials.auth_token.clone();
        Self {
            credentials,
            http: reqwest::Client::new(),
            auth_token,
            working_secret: None,
            limiter,
            rate_per_sec: DEFAULT_RATE_PER_SEC,
        }
    }

    /// Hand out this client's budget so another can draw on it.
    pub fn rate_limit(&self) -> RateLimit {
        self.limiter.clone()
    }

    /// Crawling politely is the default; raise this only if you know the
    /// account can take it.
    pub fn with_rate(mut self, rate_per_sec: f64) -> Self {
        self.rate_per_sec = rate_per_sec.max(0.1);
        self
    }

    pub fn from_repo(repo_root: &Path) -> Result<Self> {
        Ok(Self::new(Credentials::from_env(repo_root)?))
    }

    /// md5(endpoint without slashes + sorted key/value pairs + timestamp + secret)
    fn sign(endpoint: &str, params: &BTreeMap<String, String>, secret: &str) -> (u64, String) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut to_hash = endpoint.replace('/', "");
        for (key, value) in params {
            if key == "app_id" || key == "user_auth_token" {
                continue;
            }
            to_hash.push_str(key);
            to_hash.push_str(value);
        }
        to_hash.push_str(&timestamp.to_string());
        to_hash.push_str(secret);

        (timestamp, md5_hex(&to_hash))
    }

    /// Wait for a request slot.
    /// The deficit is claimed under the lock and slept for outside it, so
    /// concurrent callers queue rather than race.
    async fn acquire(&self) {
        let wait = {
            let mut bucket = self.limiter.0.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let elapsed = now.duration_since(bucket.last).as_secs_f64();
            bucket.tokens = (bucket.tokens + elapsed * self.rate_per_sec).min(BURST);
            bucket.last = now;

            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                None
            } else {
                let deficit = (1.0 - bucket.tokens) / self.rate_per_sec;
                bucket.tokens = 0.0;
                bucket.last = now + Duration::from_secs_f64(deficit);
                Some(Duration::from_secs_f64(deficit))
            }
        };

        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }

    async fn request(
        &self,
        endpoint: &str,
        params: &BTreeMap<String, String>,
        secret: Option<&str>,
    ) -> Result<Value> {
        const MAX_ATTEMPTS: u32 = 4;
        let mut last_error = String::new();

        for attempt in 0..MAX_ATTEMPTS {
            let mut query: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            query.push(("app_id".into(), self.credentials.app_id.clone()));

            if let Some(secret) = secret {
                // Re-signed per attempt: the signature carries a timestamp.
                let (timestamp, signature) = Self::sign(endpoint, params, secret);
                query.push(("request_ts".into(), timestamp.to_string()));
                query.push(("request_sig".into(), signature));
            }

            let mut builder = self
                .http
                .get(format!("{API_BASE}/{endpoint}"))
                .query(&query)
                .header("X-App-Id", &self.credentials.app_id);
            if let Some(token) = &self.auth_token {
                builder = builder.header("X-User-Auth-Token", token);
            }

            self.acquire().await;

            let response = match builder.send().await {
                Ok(response) => response,
                Err(err) => {
                    last_error = err.to_string();
                    if attempt + 1 == MAX_ATTEMPTS {
                        return Err(err).with_context(|| format!("requesting {endpoint}"));
                    }
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    continue;
                }
            };

            let status = response.status();
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let body = response.text().await.unwrap_or_default();

            if status.is_success() {
                return serde_json::from_str(&body)
                    .with_context(|| format!("parsing {endpoint} response"));
            }

            // Rate limited or a transient server fault: back off and retry.
            // Anything else is the caller's problem and fails immediately.
            let transient = status.as_u16() == 429 || status.is_server_error();
            last_error = format!("HTTP {status}: {}", &body[..body.len().min(200)]);
            if !transient || attempt + 1 == MAX_ATTEMPTS {
                bail!("{endpoint} -> {last_error}");
            }
            let backoff = retry_after.unwrap_or(1u64 << (attempt + 1));
            tokio::time::sleep(Duration::from_secs(backoff)).await;
        }

        bail!("{endpoint} -> exhausted retries: {last_error}")
    }

    pub async fn login(&mut self) -> Result<Value> {
        if self.auth_token.is_some() {
            return Ok(Value::Null);
        }
        let (Some(email), Some(password)) = (
            self.credentials.email.clone(),
            self.credentials.password_md5.clone(),
        ) else {
            bail!("{TOKEN_HELP}")
        };

        let params = BTreeMap::from([
            ("email".to_string(), email),
            ("password".to_string(), password),
        ]);
        let data = self.request("user/login", &params, None).await?;

        let token = data
            .get("user_auth_token")
            .and_then(|v| v.as_str())
            .context("login response had no user_auth_token")?;
        self.auth_token = Some(token.to_string());
        Ok(data.get("user").cloned().unwrap_or(Value::Null))
    }

    /// The signed-in account, which is also the check that the token works.
    pub async fn user(&mut self) -> Result<Value> {
        self.login().await?;
        let data = self
            .request("user/get", &BTreeMap::new(), None)
            .await
            .context("the token was rejected; tokens end with the web player session")?;
        Ok(data
            .get("user")
            .filter(|u| u.is_object())
            .cloned()
            .unwrap_or(data))
    }

    // ------------------------------------------------------------- browsing
    //
    // Unsigned, token-authenticated reads. Only `track/getFileUrl` needs the
    // app secret.

    /// Walk a paginated list endpoint until `cap` items or the reported total.
    async fn paginate(
        &self,
        endpoint: &str,
        params: &BTreeMap<String, String>,
        container: &str,
        cap: usize,
    ) -> Result<Vec<Value>> {
        const PAGE: usize = 100;
        let mut out: Vec<Value> = Vec::new();
        let mut offset = 0usize;

        while out.len() < cap {
            let mut page_params = params.clone();
            page_params.insert("limit".into(), PAGE.min(cap - out.len()).to_string());
            page_params.insert("offset".into(), offset.to_string());

            let data = self.request(endpoint, &page_params, None).await?;
            let block = data.get(container).cloned().unwrap_or(Value::Null);
            let items = block
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let total = block.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let received = items.len();
            out.extend(items);
            offset += received;

            // A short page means the end regardless of what `total` claims.
            if received == 0 || offset >= total {
                break;
            }
        }

        out.truncate(cap);
        Ok(out)
    }

    /// Full-catalogue search: tracks, albums and artists in one request.
    pub async fn search(&mut self, query: &str, limit: usize) -> Result<SearchResults> {
        self.login().await?;

        let params = BTreeMap::from([
            ("query".to_string(), query.to_string()),
            ("limit".to_string(), limit.to_string()),
        ]);
        let data = self.request("catalog/search", &params, None).await?;

        let items = |key: &str| -> Vec<Value> {
            data.get(key)
                .and_then(|block| block.get("items"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        };

        Ok(SearchResults {
            tracks: items("tracks")
                .iter()
                .filter_map(|v| RemoteTrack::parse(v, None))
                .collect(),
            albums: items("albums")
                .iter()
                .filter_map(RemoteAlbum::parse)
                .collect(),
            artists: items("artists")
                .iter()
                .filter_map(RemoteArtist::parse)
                .collect(),
        })
    }

    /// Raw favourites, exactly as Qobuz returned them. `kind` is 'tracks',
    /// 'albums' or 'artists'. The crawler needs the untouched objects for
    /// `qobuz_json`; the typed helpers below are the same call, parsed.
    pub async fn favorites_raw(&mut self, kind: &str, cap: usize) -> Result<Vec<Value>> {
        self.login().await?;
        let params = BTreeMap::from([("type".to_string(), kind.to_string())]);
        self.paginate("favorite/getUserFavorites", &params, kind, cap)
            .await
    }

    /// The signed-in user's favourited tracks.
    pub async fn favorite_tracks(&mut self, cap: usize) -> Result<Vec<RemoteTrack>> {
        Ok(self
            .favorites_raw("tracks", cap)
            .await?
            .iter()
            .filter_map(|v| RemoteTrack::parse(v, None))
            .collect())
    }

    pub async fn favorite_albums(&mut self, cap: usize) -> Result<Vec<RemoteAlbum>> {
        Ok(self
            .favorites_raw("albums", cap)
            .await?
            .iter()
            .filter_map(RemoteAlbum::parse)
            .collect())
    }

    pub async fn favorite_artists(&mut self, cap: usize) -> Result<Vec<RemoteArtist>> {
        Ok(self
            .favorites_raw("artists", cap)
            .await?
            .iter()
            .filter_map(RemoteArtist::parse)
            .collect())
    }

    /// Add or remove a track, album or artist from the account's Qobuz
    /// favourites. `kind` is "track", "album" or "artist", singular,
    /// matching the app's `request_analysis` convention, unlike
    /// `favorites_raw`'s plural "tracks"/"albums"/"artists".
    ///
    /// `favorite/create` and `favorite/delete` are not exercised anywhere
    /// else in this codebase, unlike every other endpoint here. They are the
    /// standard paired Qobuz calls, every third-party client that reads
    /// `favorite/getUserFavorites` writes through these the same way, but
    /// that is inference from the read side and from how this API's other
    /// pairs are shaped, not something confirmed against a live account.
    async fn favorite_write(&mut self, endpoint: &str, kind: &str, id: &str) -> Result<()> {
        self.login().await?;
        let param = match kind {
            "track" => "track_ids",
            "album" => "album_ids",
            "artist" => "artist_ids",
            other => bail!("unknown favourite kind {other:?}"),
        };
        self.request(
            endpoint,
            &BTreeMap::from([(param.to_string(), id.to_string())]),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn favorite_add(&mut self, kind: &str, id: &str) -> Result<()> {
        self.favorite_write("favorite/create", kind, id).await
    }

    pub async fn favorite_remove(&mut self, kind: &str, id: &str) -> Result<()> {
        self.favorite_write("favorite/delete", kind, id).await
    }

    pub async fn user_playlists(&mut self, cap: usize) -> Result<Vec<RemotePlaylist>> {
        self.login().await?;
        Ok(self
            .paginate(
                "playlist/getUserPlaylists",
                &BTreeMap::new(),
                "playlists",
                cap,
            )
            .await?
            .iter()
            .filter_map(RemotePlaylist::parse)
            .collect())
    }

    /// A playlist's tracklist. These carry their own album/performer, so no
    /// parent context is needed.
    pub async fn playlist_tracks(&mut self, playlist_id: i64, cap: usize) -> Result<Vec<RemoteTrack>> {
        self.login().await?;

        let mut out = Vec::new();
        let mut offset = 0usize;
        while out.len() < cap {
            let params = BTreeMap::from([
                ("playlist_id".to_string(), playlist_id.to_string()),
                ("extra".to_string(), "tracks".to_string()),
                ("limit".to_string(), 100.min(cap - out.len()).to_string()),
                ("offset".to_string(), offset.to_string()),
            ]);
            let data = self.request("playlist/get", &params, None).await?;
            let block = data.get("tracks").cloned().unwrap_or(Value::Null);
            let items = block
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let total = block.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let received = items.len();
            out.extend(items.iter().filter_map(|v| RemoteTrack::parse(v, None)));
            offset += received;
            if received == 0 || offset >= total {
                break;
            }
        }
        Ok(out)
    }

    /// The whole `album/get` payload, tracklist included.
    pub async fn album_raw(&mut self, album_id: &str) -> Result<Value> {
        self.login().await?;
        let params = BTreeMap::from([("album_id".to_string(), album_id.to_string())]);
        self.request("album/get", &params, None).await
    }

    /// An album and its tracklist, in one request.
    pub async fn album_tracks(&mut self, album_id: &str) -> Result<(RemoteAlbum, Vec<RemoteTrack>)> {
        let data = self.album_raw(album_id).await?;

        let album = RemoteAlbum::parse(&data)
            .with_context(|| format!("album {album_id} had no usable metadata"))?;
        let tracks = data
            .get("tracks")
            .and_then(|block| block.get("items"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| RemoteTrack::parse(v, Some(&album)))
            .collect();

        Ok((album, tracks))
    }

    /// Raw album objects for an artist, paginated through `artist/get`.
    pub async fn artist_albums_raw(&mut self, artist_id: i64, cap: usize) -> Result<Vec<Value>> {
        Ok(self.artist_albums_inner(artist_id, cap).await?.1)
    }

    /// An artist's discography, newest first.
    pub async fn artist_albums(
        &mut self,
        artist_id: i64,
        cap: usize,
    ) -> Result<(RemoteArtist, Vec<RemoteAlbum>)> {
        let (artist, raw) = self.artist_albums_inner(artist_id, cap).await?;
        let mut out = own_releases(artist_id, raw.iter().filter_map(RemoteAlbum::parse).collect());
        out.sort_by(|a, b| b.year().cmp(&a.year()));
        Ok((artist, out))
    }

    async fn artist_albums_inner(
        &mut self,
        artist_id: i64,
        cap: usize,
    ) -> Result<(RemoteArtist, Vec<Value>)> {
        self.login().await?;

        let mut artist = RemoteArtist {
            id: artist_id,
            ..Default::default()
        };
        let mut out: Vec<Value> = Vec::new();
        let mut offset = 0usize;

        while out.len() < cap {
            let params = BTreeMap::from([
                ("artist_id".to_string(), artist_id.to_string()),
                ("extra".to_string(), "albums".to_string()),
                ("limit".to_string(), 100.min(cap - out.len()).to_string()),
                ("offset".to_string(), offset.to_string()),
            ]);
            let data = self.request("artist/get", &params, None).await?;

            if artist.name.is_empty() {
                if let Some(parsed) = RemoteArtist::parse(&data) {
                    artist = parsed;
                }
            }

            let block = data.get("albums").cloned().unwrap_or(Value::Null);
            let items = block
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let total = block.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let received = items.len();
            out.extend(items);
            offset += received;
            if received == 0 || offset >= total {
                break;
            }
        }

        Ok((artist, out))
    }

    /// Artists Qobuz considers close to this one. The same endpoint the
    /// crawler expands the frontier with.
    pub async fn similar_artists_raw(
        &mut self,
        artist_id: i64,
        limit: usize,
    ) -> Result<Vec<Value>> {
        self.login().await?;

        let params = BTreeMap::from([
            ("artist_id".to_string(), artist_id.to_string()),
            ("limit".to_string(), limit.to_string()),
        ]);
        let data = self
            .request("artist/getSimilarArtists", &params, None)
            .await?;

        Ok(data
            .get("artists")
            .and_then(|block| block.get("items"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    pub async fn similar_artists(&mut self, artist_id: i64, limit: usize) -> Result<Vec<RemoteArtist>> {
        Ok(self
            .similar_artists_raw(artist_id, limit)
            .await?
            .iter()
            .filter_map(RemoteArtist::parse)
            .collect())
    }

    /// Signed stream URL. Tries each configured secret until one is accepted.
    pub async fn file_url(&mut self, track_id: i64, format_id: u32) -> Result<String> {
        self.login().await?;

        let params = BTreeMap::from([
            ("track_id".to_string(), track_id.to_string()),
            ("format_id".to_string(), format_id.to_string()),
            ("intent".to_string(), "stream".to_string()),
        ]);

        let candidates: Vec<String> = match &self.working_secret {
            Some(secret) => vec![secret.clone()],
            None => self.credentials.secrets.clone(),
        };
        if candidates.is_empty() {
            bail!("no QOBUZ_APP_SECRET configured for request signing");
        }

        let had_working_secret = self.working_secret.is_some();
        let mut last_error = String::new();
        for secret in candidates {
            match self.request("track/getFileUrl", &params, Some(&secret)).await {
                Ok(data) => {
                    if let Some(url) = data.get("url").and_then(|v| v.as_str()) {
                        self.working_secret = Some(secret);
                        return Ok(url.to_string());
                    }
                    last_error = format!("no url in response: {data}");
                }
                Err(err) => last_error = err.to_string(),
            }
        }

        // Qobuz reports delisted tracks as a signature error too, so once a
        // secret is known to work, blame the track rather than the credentials.
        if had_working_secret {
            bail!("track {track_id} is not streamable (likely delisted): {last_error}")
        }

        bail!(
            "no configured app secret produced a valid signature (it may have rotated). \
             Run `two-khz-server refresh-credentials --write`. Last error: {last_error}"
        )
    }

    /// Write a path out as a real Qobuz playlist. Returns the playlist id.
    pub async fn export_playlist(&mut self, name: &str, track_ids: &[i64]) -> Result<i64> {
        self.login().await?;

        let created = self
            .request(
                "playlist/create",
                &BTreeMap::from([
                    ("name".to_string(), name.to_string()),
                    ("is_public".to_string(), "false".to_string()),
                    ("is_collaborative".to_string(), "false".to_string()),
                ]),
                None,
            )
            .await?;

        let playlist_id = created
            .get("id")
            .and_then(|v| v.as_i64())
            .context("playlist/create returned no id")?;

        let ids = track_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");

        self.request(
            "playlist/addTracks",
            &BTreeMap::from([
                ("playlist_id".to_string(), playlist_id.to_string()),
                ("track_ids".to_string(), ids),
            ]),
            None,
        )
        .await?;

        Ok(playlist_id)
    }
}

/// Repo root, inferred from the crate location.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}
