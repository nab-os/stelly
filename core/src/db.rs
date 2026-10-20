//! Track metadata, read from the corpus when the space is loaded. The schema
//! lives in `schema.sql` at the repo root, and its typed mirror in `schema`.

use crate::schema::{albums, artists, blocked_artists, features, layout, tracks};
use anyhow::{Context, Result};
use diesel::prelude::*;
pub use crate::api::TrackMeta;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

#[derive(Default)]
pub struct Catalog {
    pub tracks: Vec<TrackMeta>,
    /// Raw CLAP audio embeddings, row-aligned with `tracks`. Raw rather than
    /// the PCA'd semantic block; see paths::text_target. Shared, so the
    /// describe baseline can scan it without holding the engine.
    pub clap: Option<Arc<Vec<f32>>>,
    pub clap_dims: usize,
    /// Artists to pretend are not there. The space keeps their rows, so
    /// blocking is enforced on every read, which is what lets it take effect
    /// without a rebuild. Set through `set_blocked`, which keeps `blocked`.
    blocked_artists: HashSet<i64>,
    /// Per row, whether its artist is blocked. Every scan of the space asks
    /// this of every row, a hash lookup each time was most of a scan.
    blocked: Vec<bool>,
    /// Per row, its artist numbered from 0, so a walk counts artists in a
    /// plain array rather than a map. `NO_ARTIST` for a track without one.
    artists: Vec<u32>,
    artist_count: usize,
    /// For each row that is not the only one of its recording, the rows that
    /// are the same song on other releases. See `recording`. Worked out once
    /// per load, since it depends on the catalogue alone.
    pub twins: HashMap<usize, Vec<usize>>,
}

/// Track ids bound into one query at load.
const LOAD_BATCH: usize = 10_000;

/// An artist number for a track without an artist.
pub const NO_ARTIST: u32 = u32::MAX;

fn open(db_path: &Path) -> Result<SqliteConnection> {
    let url = db_path
        .to_str()
        .with_context(|| format!("{} is not a UTF-8 path", db_path.display()))?;
    SqliteConnection::establish(url).with_context(|| format!("opening {}", db_path.display()))
}

/// Artists the user has blocked. The server honours the list everywhere; the
/// client filters what it has already loaded.
fn load_blocked(conn: &mut SqliteConnection) -> Result<HashSet<i64>> {
    Ok(blocked_artists::table
        .select(blocked_artists::artist_id)
        .load::<i64>(conn)?
        .into_iter()
        .collect())
}

/// Load CLAP embeddings for the given track order. Each row goes straight
/// into its place in the matrix as it is read, so the blobs are never all
/// held twice.
fn load_clap(conn: &mut SqliteConnection, track_ids: &[i64]) -> Result<(Option<Vec<f32>>, usize)> {
    let rows: HashMap<i64, usize> = track_ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut matrix: Vec<f32> = Vec::new();
    let mut dims = 0;
    let stored = features::table
        .filter(features::clap_f32.is_not_null())
        .select((features::track_id, features::clap_f32.assume_not_null()))
        .load_iter::<(i64, Vec<u8>), diesel::connection::DefaultLoadingMode>(conn)?;
    for row in stored {
        let (id, blob) = row?;
        if blob.len() % 4 != 0 {
            continue;
        }
        // The first vector read sets the width; one of another is left out.
        if dims == 0 {
            dims = blob.len() / 4;
            matrix = vec![0.0f32; track_ids.len() * dims];
        }
        let Some(&i) = rows.get(&id) else { continue };
        if blob.len() == dims * 4 {
            bytemuck::cast_slice_mut::<f32, u8>(&mut matrix[i * dims..(i + 1) * dims]).copy_from_slice(&blob);
        }
    }
    if dims == 0 {
        return Ok((None, 0));
    }
    Ok((Some(matrix), dims))
}

/// One track and everything joined onto it.
#[derive(Queryable)]
struct Row {
    id: i64,
    title: String,
    artist_id: Option<i64>,
    album_id: Option<String>,
    seed_distance: i64,
    artist: Option<String>,
    album: Option<String>,
    genre: Option<String>,
    image: Option<String>,
    descriptors: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    isrc: Option<String>,
}

/// The one field of the descriptors the space keeps.
#[derive(serde::Deserialize)]
struct Bpm {
    bpm: Option<f64>,
}

impl From<Row> for TrackMeta {
    fn from(row: Row) -> Self {
        // BPM lives inside the descriptor blob; pull just that one field,
        // without building the rest.
        let bpm = row
            .descriptors
            .as_deref()
            .and_then(|json| serde_json::from_str::<Bpm>(json).ok())
            .and_then(|d| d.bpm)
            .map(|b| b as f32);
        TrackMeta {
            track_id: row.id,
            title: row.title,
            artist_id: row.artist_id.unwrap_or(-1),
            album_id: row.album_id.unwrap_or_default(),
            image: row.image,
            seed_distance: row.seed_distance as i32,
            artist: row.artist.unwrap_or_default(),
            album: row.album.unwrap_or_default(),
            genre: row.genre.unwrap_or_default(),
            bpm,
            x: row.x.map(|v| v as f32),
            y: row.y.map(|v| v as f32),
            isrc: row.isrc,
        }
    }
}

impl Catalog {
    /// Load metadata for the given track ids, in that exact order, so indices
    /// line up with the rows of space.bin.
    pub fn load(db_path: &Path, track_ids: &[i64]) -> Result<Self> {
        let conn = &mut open(db_path)?;
        // Only the space's tracks, a slice of the catalogue once the crawl has
        // gone wide, in batches under SQLite's limit on bound values.
        let mut by_id: HashMap<i64, TrackMeta> = HashMap::with_capacity(track_ids.len());
        for ids in track_ids.chunks(LOAD_BATCH) {
            let rows: Vec<Row> = tracks::table
                .left_join(artists::table)
                .left_join(albums::table)
                .left_join(features::table)
                .left_join(layout::table)
                .filter(tracks::id.eq_any(ids))
                .select((
                    tracks::id,
                    tracks::title,
                    tracks::artist_id,
                    tracks::album_id,
                    tracks::seed_distance,
                    artists::name.nullable(),
                    albums::title.nullable(),
                    albums::genre.nullable(),
                    albums::image.nullable(),
                    features::descriptors_json.nullable(),
                    layout::x.nullable(),
                    layout::y.nullable(),
                    tracks::isrc,
                ))
                .load(conn)?;
            by_id.extend(rows.into_iter().map(|row| (row.id, TrackMeta::from(row))));
        }

        let tracks: Vec<TrackMeta> = track_ids
            .iter()
            .map(|id| {
                by_id.remove(id).unwrap_or(TrackMeta {
                    track_id: *id,
                    title: format!("<missing {id}>"),
                    ..Default::default()
                })
            })
            .collect();

        let (clap, clap_dims) = load_clap(conn, track_ids)?;
        let blocked_artists = load_blocked(conn)?;
        Ok(Self::new(tracks, clap.map(Arc::new), clap_dims, blocked_artists))
    }

    /// A catalogue over `tracks`, with what is worked out from them once.
    pub fn new(
        tracks: Vec<TrackMeta>,
        clap: Option<Arc<Vec<f32>>>,
        clap_dims: usize,
        blocked_artists: HashSet<i64>,
    ) -> Self {
        let mut numbers: HashMap<i64, u32> = HashMap::new();
        let artists = tracks
            .iter()
            .map(|t| match t.artist_id {
                -1 => NO_ARTIST,
                id => {
                    let next = numbers.len() as u32;
                    *numbers.entry(id).or_insert(next)
                }
            })
            .collect();
        let mut catalog = Self {
            twins: Self::twins_of(&tracks),
            tracks,
            clap,
            clap_dims,
            blocked_artists: HashSet::new(),
            blocked: Vec::new(),
            artists,
            artist_count: numbers.len(),
        };
        catalog.set_blocked(blocked_artists);
        catalog
    }

    /// Apply a block list. Blocking only filters, so nothing needs rebuilding.
    pub fn set_blocked(&mut self, blocked_artists: HashSet<i64>) {
        self.blocked = self
            .tracks
            .iter()
            .map(|t| blocked_artists.contains(&t.artist_id))
            .collect();
        self.blocked_artists = blocked_artists;
    }

    /// The artists blocked.
    pub fn blocked_artists(&self) -> &HashSet<i64> {
        &self.blocked_artists
    }

    /// The `twins` map for a set of rows.
    pub fn twins_of(tracks: &[TrackMeta]) -> HashMap<usize, Vec<usize>> {
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for (row, group) in crate::recording::recordings(tracks).into_iter().enumerate() {
            groups.entry(group).or_default().push(row);
        }
        let mut twins = HashMap::new();
        for rows in groups.into_values().filter(|rows| rows.len() > 1) {
            for &row in &rows {
                twins.insert(row, rows.iter().copied().filter(|&other| other != row).collect());
            }
        }
        twins
    }

    /// The other rows of row `i`'s recording, none for most.
    pub fn twins(&self, i: usize) -> &[usize] {
        self.twins.get(&i).map_or(&[], Vec::as_slice)
    }

    /// Whether row `i` belongs to a blocked artist.
    pub fn is_blocked(&self, i: usize) -> bool {
        self.blocked.get(i).copied().unwrap_or(false)
    }

    /// Per row, whether it is blocked, for a scan over every row.
    pub fn blocked_rows(&self) -> &[bool] {
        &self.blocked
    }

    /// Row `i`'s artist in `artists`' numbering, `None` for a track without
    /// one.
    pub fn artist(&self, i: usize) -> Option<u32> {
        Some(self.artists[i]).filter(|&a| a != NO_ARTIST)
    }

    /// Per row, its artist's number, `NO_ARTIST` for none, for a scan over
    /// every row.
    pub fn artist_rows(&self) -> &[u32] {
        &self.artists
    }

    /// How many artists `artists` numbers.
    pub fn artist_count(&self) -> usize {
        self.artist_count
    }

    /// Row indices that are visible, in catalog order.
    pub fn visible(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.tracks.len()).filter(|&i| !self.is_blocked(i))
    }

    pub fn get(&self, i: usize) -> &TrackMeta {
        &self.tracks[i]
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Ids present in the space, for checking whether a Qobuz result is
    /// navigable. Blocked tracks are left out.
    pub fn id_set(&self) -> HashSet<i64> {
        self.visible().map(|i| self.tracks[i].track_id).collect()
    }

    /// Albums and artists with at least one visible track in the space, for
    /// marking a Qobuz album or artist as one the map already reaches.
    pub fn reach(&self) -> (HashSet<String>, HashSet<i64>) {
        let mut albums = HashSet::new();
        let mut artists = HashSet::new();
        for i in self.visible() {
            let track = &self.tracks[i];
            if !track.album_id.is_empty() {
                albums.insert(track.album_id.clone());
            }
            if track.artist_id >= 0 {
                artists.insert(track.artist_id);
            }
        }
        (albums, artists)
    }
}

/// Just the ids, for refreshing a loaded catalog after a block changes.
pub fn blocked_artist_ids(db_path: &Path) -> Result<HashSet<i64>> {
    load_blocked(&mut open(db_path)?)
}
