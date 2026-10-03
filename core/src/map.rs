//! Point-cloud payloads for the canvas map.
//!
//! Native Rust cannot touch canvas APIs in a webview, so: bulk data crosses as
//! binary over the asset protocol, rendering and pan/zoom happen in JS, and
//! only selections come back. 50k points through `eval` would freeze it.

use crate::db::Catalog;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct MapMeta {
    pub n: usize,
    /// Distinct genre names, indexed by the genre array in the binary payload.
    pub genres: Vec<String>,
    /// "Artist - Title" per point, in payload order. Sent with the metadata
    /// rather than fetched per hover, a tooltip that round-trips to Rust
    /// flickers. ~570KB for a 14k corpus, parsed once.
    pub labels: Vec<String>,
    pub bounds: Bounds,
    /// False when the layout step has not been run yet.
    pub has_layout: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct Bounds {
    pub min_x: f32,
    pub max_x: f32,
    pub min_y: f32,
    pub max_y: f32,
}

/// Tracks that have layout coordinates, in catalog order.
fn laid_out(catalog: &Catalog) -> Vec<usize> {
    catalog
        .visible()
        .filter(|&i| catalog.get(i).x.is_some() && catalog.get(i).y.is_some())
        .collect()
}

pub fn meta(catalog: &Catalog) -> MapMeta {
    let indices = laid_out(catalog);

    let mut genres: Vec<String> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for &i in &indices {
        let genre = catalog.get(i).genre.clone();
        seen.entry(genre.clone()).or_insert_with(|| {
            genres.push(genre);
            genres.len() - 1
        });
    }

    let labels: Vec<String> = indices
        .iter()
        .map(|&i| {
            let track = catalog.get(i);
            if track.artist.is_empty() {
                track.title.clone()
            } else {
                format!("{} - {}", track.artist, track.title)
            }
        })
        .collect();

    let mut bounds = Bounds {
        min_x: f32::INFINITY,
        max_x: f32::NEG_INFINITY,
        min_y: f32::INFINITY,
        max_y: f32::NEG_INFINITY,
    };
    for &i in &indices {
        let track = catalog.get(i);
        let (x, y) = (track.x.unwrap(), track.y.unwrap());
        bounds.min_x = bounds.min_x.min(x);
        bounds.max_x = bounds.max_x.max(x);
        bounds.min_y = bounds.min_y.min(y);
        bounds.max_y = bounds.max_y.max(y);
    }
    if indices.is_empty() {
        bounds = Bounds::default();
    }

    MapMeta {
        n: indices.len(),
        genres,
        labels,
        bounds,
        has_layout: !indices.is_empty(),
    }
}

/// One binary blob, laid out so JS can make typed-array views with no copying.
///
///   offset 0      : Float64Array(n)  track ids   (f64 keeps JS numbers exact)
///   offset 8n     : Float32Array(2n) x, y pairs
///   offset 16n    : Uint16Array(n)   genre index
pub fn payload(catalog: &Catalog) -> Vec<u8> {
    let indices = laid_out(catalog);
    let n = indices.len();

    let mut genre_index: HashMap<String, u16> = HashMap::new();
    let mut next = 0u16;
    for &i in &indices {
        genre_index.entry(catalog.get(i).genre.clone()).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
    }

    let mut bytes = Vec::with_capacity(n * 18);
    for &i in &indices {
        bytes.extend_from_slice(&(catalog.get(i).track_id as f64).to_le_bytes());
    }
    for &i in &indices {
        let track = catalog.get(i);
        bytes.extend_from_slice(&track.x.unwrap_or(0.0).to_le_bytes());
        bytes.extend_from_slice(&track.y.unwrap_or(0.0).to_le_bytes());
    }
    for &i in &indices {
        let g = genre_index
            .get(&catalog.get(i).genre)
            .copied()
            .unwrap_or(0);
        bytes.extend_from_slice(&g.to_le_bytes());
    }

    bytes
}
