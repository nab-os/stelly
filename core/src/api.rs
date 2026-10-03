//! Wire types shared by the app and the server.
//!
//! Space navigation is among them: neighbours, radio, paths and drift are
//! answered by the server, which holds the one copy of the space.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

    /// The `stelly-server` subcommand that runs the same thing by hand.
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

    /// Whether finishing this stage changes the space, so the server loads it
    /// again and clients refetch what they cache of it.
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
    /// Tracks in the loaded space. Filled in by the client from `SpaceInfo`,
    /// what the map draws rather than what is on disk.
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
    /// Bumped when a space-rebuilding stage finishes, once the server has
    /// loaded the result. Clients ask for the space again when it moves.
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

// ---------------------------------------------------------------- the space

/// One track of the space, as navigation and the map know it. Not a Qobuz
/// track: no art, no duration, a genre and a position instead.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackMeta {
    pub track_id: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub artist_id: i64,
    pub album_id: String,
    pub bpm: Option<f32>,
    pub seed_distance: i32,
    /// Names the recording rather than the release; see
    /// `qobuz::RemoteTrack::identity`.
    pub isrc: Option<String>,
    /// UMAP coordinates for the map, if the layout step has been run.
    pub x: Option<f32>,
    pub y: Option<f32>,
}

/// One row of a generated sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    #[serde(flatten)]
    pub track: TrackMeta,
    pub similarity: Option<f32>,
}

/// What the server's space looks like, small enough to ask for on every start.
///
/// `stamp` changes whenever anything a client caches would: a rebuild, or a
/// change to the hidden artists. Unlike the pipeline's generation it survives
/// a server restart, so a client can keep its copy across one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpaceInfo {
    /// Empty when no space has been built yet.
    pub stamp: String,
    pub n_tracks: usize,
    pub blocks: Vec<BlockWeight>,
    /// Drift and mood: the corpus carries audio embeddings and the text tower
    /// is there to embed a phrase.
    pub can_steer: bool,
    /// Visible tracks, and how many of them have a place on the map.
    pub in_space: i64,
    pub on_map: i64,
}

/// A block of the space and the weight it gets unless a slider says otherwise.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockWeight {
    pub name: String,
    pub default_weight: f32,
}

/// Every visible track id, and the albums and artists they reach, for marking
/// a Qobuz row as one the space holds without asking per row.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpaceIndex {
    pub stamp: String,
    pub ids: Vec<i64>,
    pub albums: Vec<String>,
    pub artists: Vec<i64>,
}

/// The space's own tracks matching a phrase, and the albums and artists
/// behind them, for a search narrowed to the space.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpaceSearch {
    /// Every track that matched, of which `tracks` is the head.
    pub total: usize,
    /// Matched on the title alone: a track found through its artist's or
    /// album's name is that artist's or album's tile to show.
    pub tracks: Vec<TrackMeta>,
    /// Matched on any of artist, title or album, every word somewhere.
    pub albums: Vec<SpaceAlbum>,
    pub artists: Vec<SpaceArtist>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpaceAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpaceArtist {
    pub id: i64,
    pub name: String,
}

/// How a space search orders the tracks it hands back. The order matters on
/// the server: only the head of the list crosses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpaceSort {
    /// Best match first, then the space's own order.
    #[default]
    Rank,
    Title,
    Artist,
    Album,
}

/// What produced a sequence, and what to produce one from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Recipe {
    Neighbours { seed: i64 },
    Radio { seed: i64 },
    Path { a: i64, b: i64, even: bool },
    Drift { seed: i64, phrase: String },
    Mood { phrase: String },
}

/// Weights are sent with every request rather than held per device: the
/// sliders are the client's, and the server has nowhere to keep them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub recipe: Recipe,
    pub count: usize,
    /// Similarity subtracted per prior use of an artist, for radio and mood.
    pub penalty: f32,
    #[serde(default)]
    pub weights: HashMap<String, f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TracksRequest {
    pub ids: Vec<i64>,
}

/// The queue's upcoming tracks, to be put in an order that flows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderRequest {
    pub ids: Vec<i64>,
    pub from: Option<i64>,
    #[serde(default)]
    pub weights: HashMap<String, f32>,
}

// ----------------------------------------------------------------- the auth

/// What a device is allowed to do. `Play` is every device; `Pipeline` is hours
/// of CPU, the shared rate limit, the family's hidden artists, and
/// `block --purge`, which deletes rows.
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
    /// Whose device it is. Absent from servers older than users.
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub user: String,
}

/// One member of the family. Each has their own devices, play session and
/// likes; the Qobuz account and the space are everyone's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub created_at: String,
}

/// What an import of the Qobuz favourites added to someone's likes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Imported {
    pub tracks: usize,
    pub albums: usize,
    pub artists: usize,
}

/// The one time a token is ever transmitted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingGrant {
    pub device: Device,
    pub token: String,
}

// -------------------------------------------------------------- the errors

/// A failure, in the shape the client can turn back into an `anyhow::Error`.
/// The server sends the message it would have logged: a family on a private
/// network, and a real error beats a sanitised 500.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
}
