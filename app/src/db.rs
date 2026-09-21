//! Track metadata, read from the slim catalogue the server hands out. The
//! schema lives in `schema.sql` at the repo root, and its typed mirror in
//! `schema`; only the server writes.

use crate::schema::{albums, artists, blocked_artists, features, layout, tracks};
use anyhow::{Context, Result};
use diesel::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize)]
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

pub struct Catalog {
    pub tracks: Vec<TrackMeta>,
    /// Raw CLAP audio embeddings, row-aligned with `tracks`. Raw rather than
    /// the PCA'd semantic block; see paths::text_target.
    pub clap: Option<Vec<f32>>,
    pub clap_dims: usize,
    /// Artists to pretend are not there. The space keeps their rows, so
    /// blocking is enforced on every read, which is what lets it take effect
    /// without a rebuild.
    pub blocked_artists: HashSet<i64>,
}

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

/// Load CLAP embeddings for the given track order.
fn load_clap(conn: &mut SqliteConnection, track_ids: &[i64]) -> Result<(Option<Vec<f32>>, usize)> {
    let stored: HashMap<i64, Vec<f32>> = features::table
        .filter(features::clap_f32.is_not_null())
        .select((features::track_id, features::clap_f32.assume_not_null()))
        .load::<(i64, Vec<u8>)>(conn)?
        .into_iter()
        .filter(|(_, blob)| blob.len() % 4 == 0)
        .map(|(id, blob)| (id, bytemuck::cast_slice::<u8, f32>(&blob).to_vec()))
        .collect();

    let Some(dims) = stored.values().map(|v| v.len()).next() else {
        return Ok((None, 0));
    };

    let mut matrix = vec![0.0f32; track_ids.len() * dims];
    for (i, id) in track_ids.iter().enumerate() {
        if let Some(vector) = stored.get(id) {
            if vector.len() == dims {
                matrix[i * dims..(i + 1) * dims].copy_from_slice(vector);
            }
        }
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
    descriptors: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    isrc: Option<String>,
}

impl From<Row> for TrackMeta {
    fn from(row: Row) -> Self {
        // BPM lives inside the descriptor blob; pull just that one field.
        let bpm = row.descriptors.as_deref().and_then(|json| {
            serde_json::from_str::<serde_json::Value>(json)
                .ok()
                .and_then(|v| v.get("bpm").and_then(|b| b.as_f64()))
                .map(|b| b as f32)
        });
        TrackMeta {
            track_id: row.id,
            title: row.title,
            artist_id: row.artist_id.unwrap_or(-1),
            album_id: row.album_id.unwrap_or_default(),
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

        let rows: Vec<Row> = tracks::table
            .left_join(artists::table)
            .left_join(albums::table)
            .left_join(features::table)
            .left_join(layout::table)
            .select((
                tracks::id,
                tracks::title,
                tracks::artist_id,
                tracks::album_id,
                tracks::seed_distance,
                artists::name.nullable(),
                albums::title.nullable(),
                albums::genre.nullable(),
                features::descriptors_json.nullable(),
                layout::x.nullable(),
                layout::y.nullable(),
                tracks::isrc,
            ))
            .load(conn)?;
        let mut by_id: HashMap<i64, TrackMeta> = rows
            .into_iter()
            .map(|row| (row.id, TrackMeta::from(row)))
            .collect();

        let tracks = track_ids
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

        Ok(Self {
            tracks,
            clap,
            clap_dims,
            blocked_artists,
        })
    }

    /// Whether row `i` belongs to a blocked artist.
    pub fn is_blocked(&self, i: usize) -> bool {
        self.tracks
            .get(i)
            .is_some_and(|t| self.blocked_artists.contains(&t.artist_id))
    }

    pub fn is_blocked_id(&self, track_id: i64) -> bool {
        self.tracks
            .iter()
            .find(|t| t.track_id == track_id)
            .is_some_and(|t| self.blocked_artists.contains(&t.artist_id))
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
}

/// Just the ids, for refreshing a loaded catalog after a block changes.
pub fn blocked_artist_ids(db_path: &Path) -> Result<HashSet<i64>> {
    load_blocked(&mut open(db_path)?)
}
