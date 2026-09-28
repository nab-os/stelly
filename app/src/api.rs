//! Wire types shared by the app and the server.
//!
//! Space navigation is deliberately absent: neighbours, radio, paths and drift
//! stay client-side in both modes. Only `embed` crosses, because the CLAP
//! tower is 500MB and its answer is 512 floats.

use serde::{Deserialize, Serialize};

// --------------------------------------------------------------- the stages

/// One step of the pipeline. Kebab-case on the wire so it matches the CLI
/// subcommand the server runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Crawl,
    Analyse,
    BuildSpace,
    Layout,
}

impl Stage {
    pub const ALL: [Stage; 4] = [Stage::Crawl, Stage::Analyse, Stage::BuildSpace, Stage::Layout];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Crawl => "crawl",
            Stage::Analyse => "analyse",
            Stage::BuildSpace => "build space",
            Stage::Layout => "layout",
        }
    }

    /// The `two-khz-server` subcommand that runs the same thing by hand.
    pub fn command(self) -> &'static str {
        match self {
            Stage::Crawl => "crawl",
            Stage::Analyse => "analyse",
            Stage::BuildSpace => "build-space",
            Stage::Layout => "layout",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Stage::Crawl => "Follow similar artists outward from your favourites.",
            Stage::Analyse => "Download excerpts and extract features. The slow one.",
            Stage::BuildSpace => "Assemble the vectors. Seconds, no network.",
            Stage::Layout => "UMAP projection for the map.",
        }
    }

    /// Whether finishing this stage invalidates the client's space. Remotely
    /// that means re-syncing, not just reloading.
    pub fn rebuilds_space(self) -> bool {
        matches!(self, Stage::BuildSpace | Stage::Layout)
    }
}

/// The three that terminate on their own, in dependency order. Crawl runs
/// until the frontier empties, so it is not queued behind other work.
pub const FULL_RUN: [Stage; 3] = [Stage::Analyse, Stage::BuildSpace, Stage::Layout];

// ------------------------------------------------------------- the targets

/// Shorter tracks are not analysed, not worth the request. Here so a page
/// counting what it could add to the space leaves them out too.
pub const MIN_TRACK_SECONDS: i64 = 45;

/// Something asked for by name, to be added to the space ahead of the rest of
/// the catalogue: catalogued, analysed, then built and laid out with the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
pub enum Target {
    /// Just the one track, though its album's tracklist is catalogued too.
    Track(i64),
    Album(String),
    /// The artist's own releases, every track of each.
    Artist(i64),
}

impl Target {
    /// The `analyse` flag that asks for the same thing by hand.
    pub fn flag(&self) -> String {
        match self {
            Target::Track(id) => format!("--track {id}"),
            Target::Album(id) => format!("--album {id}"),
            Target::Artist(id) => format!("--artist {id}"),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Track(id) => write!(f, "track {id}"),
            Target::Album(id) => write!(f, "album {id}"),
            Target::Artist(id) => write!(f, "artist {id}"),
        }
    }
}

// --------------------------------------------------------------- the counts

/// What still needs doing, phrased as the stages themselves would phrase it.
///
/// Each field is asked the way its stage asks it. Deriving them arithmetically
/// is wrong: the populations differ over blocked artists.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Corpus {
    pub tracks: i64,
    /// Rows in `features`, whatever their artist.
    pub analysed: i64,
    /// Exactly what `analyse` would queue: unanalysed, not failed, not blocked.
    pub to_analyse: i64,
    pub failed: i64,
    /// Frontier entries waiting for a crawl.
    pub pending: i64,
    /// What `build-space` would include if run now.
    pub buildable: i64,
    /// Tracks in the space the client has loaded. Filled in by the client,
    /// the server cannot answer for someone else's stale sync.
    pub in_space: i64,
    /// Of those, the ones with coordinates, the points actually drawn.
    pub on_map: i64,
}

// ---------------------------------------------------------------- the crawl

/// How many similar-artist hops from a favourite the crawl follows by default.
pub const DEFAULT_MAX_DISTANCE: i64 = 2;

/// A running crawl, as the pipeline view needs to show it. Polled, not pushed,
/// a server cannot write Dioxus signals.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CrawlStatus {
    pub running: bool,
    pub artists_expanded: usize,
    pub albums_expanded: usize,
    pub tracks_added: i64,
    pub errors: usize,
    pub blocked_skipped: usize,
    /// Total rows in `tracks`, so the view can show the corpus growing.
    pub tracks: i64,
    /// Frontier entries still pending.
    pub pending: i64,
    /// What the last step did, for the one-line ticker.
    pub last: Option<String>,
}

// ------------------------------------------------------------- the pipeline

/// Which stage is running, and whether the space underneath has changed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineStatus {
    pub running: Option<Stage>,
    /// Stages queued behind the running one, in the order they will run.
    pub queued: Vec<Stage>,
    /// Bumped when a space-rebuilding stage finishes. The client re-reads
    /// (local) or re-syncs (remote) when it differs from what it loaded.
    pub generation: u64,
    /// What the running analyse is scoped to, when it is not the backlog.
    #[serde(default)]
    pub target: Option<Target>,
    /// Everything asked for by name that is not on the map yet: waiting, being
    /// analysed, or waiting for the space and the layout to be rebuilt.
    #[serde(default)]
    pub adding: Vec<Target>,
    /// Targets not started yet, in the order they will run.
    #[serde(default)]
    pub waiting: Vec<Target>,
    /// How far the running stage has got, when it can tell.
    #[serde(default)]
    pub progress: Option<Progress>,
}

/// How far through one step of a stage: `catalogue`, `analyse`,
/// `build-space` or `layout`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub step: String,
    pub done: u64,
    pub total: u64,
    /// Since the step started, for an estimate of what is left.
    pub seconds: f64,
}

impl Progress {
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
        }
    }

    /// Seconds left at the rate so far, once there is a rate to go on.
    pub fn remaining(&self) -> Option<f64> {
        (self.done > 0 && self.done < self.total)
            .then(|| self.seconds / self.done as f64 * (self.total - self.done) as f64)
    }
}

/// A slice of a stage's output. Cursor-based, because locally the lines come
/// off a subprocess reader and remotely off SSE, and the view should not care.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LogSlice {
    pub lines: Vec<String>,
    /// Pass this back as `since` on the next call.
    pub cursor: u64,
}

// --------------------------------------------------------------- the client

/// An artist the user has hidden.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockedArtist {
    pub artist_id: i64,
    pub name: String,
    pub reason: Option<String>,
}

// ----------------------------------------------------------------- the sync

/// What the server holds, so a client can tell whether its copy is stale. The
/// digests cover the case where a restart has reset `generation`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncManifest {
    pub generation: u64,
    pub files: Vec<SyncFile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncFile {
    /// Name within the data directory: `space.bin`, `space.json`, `catalog.db`.
    pub name: String,
    pub bytes: u64,
    /// Hex md5 of the contents. md5 because it is already a dependency for
    /// request signing and this is a cache key, not a security boundary.
    pub digest: String,
}

/// Hex md5, the shape both halves compare sync files by. Spelled out because
/// `finalize` returns a `GenericArray`, which has no `LowerHex`.
pub fn digest(bytes: &[u8]) -> String {
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ----------------------------------------------------------------- the auth

/// What a device is allowed to do. `Play` is every device; `Pipeline` is hours
/// of CPU, the shared rate limit, and `block --purge`, which deletes rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Play,
    Pipeline,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Play => "play",
            Scope::Pipeline => "pipeline",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "play" => Some(Scope::Play),
            "pipeline" => Some(Scope::Pipeline),
            _ => None,
        }
    }

    /// `pipeline` implies `play`: a device trusted to start `analyse` is
    /// already trusted to search. The reverse is not true.
    pub fn covers(self, needed: Scope) -> bool {
        self == needed || (self == Scope::Pipeline && needed == Scope::Play)
    }
}

/// A paired device, as the pipeline view lists them. The token itself is never
/// in here, it is shown once, at pairing, and only its hash is stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub scope: Scope,
    pub created_at: String,
    pub last_seen: Option<String>,
}

/// The one time a token is ever transmitted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingGrant {
    pub device: Device,
    pub token: String,
}

// -------------------------------------------------------------- the errors

/// A failure, in the shape the client can turn back into an `anyhow::Error`.
/// The server sends the message it would have logged: single-user, private
/// network, and a real error beats a sanitised 500.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
}
