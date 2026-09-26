//! Loading the vector space the server's `build-space` writes.
//!
//! `space.bin` holds per-block normalised but UNWEIGHTED vectors. Weights are
//! applied here, at query time, so the sliders reshape distances live.

use anyhow::{Context, Result};
use memmap2::Mmap;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

/// The space format this build understands, and the one the server writes.
///
/// Bump it whenever the block layout changes: a reordered block keeps the same
/// byte count, so the length check cannot see it.
pub const SPACE_VERSION: &str = "space-2";

#[derive(Debug, Clone, Deserialize)]
pub struct Block {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub columns: Vec<String>,
    pub default_weight: f32,
    /// Per-column z-score parameters, needed to project an external point
    /// (a CLAP text embedding) into this block.
    pub mean: Vec<f32>,
    pub std: Vec<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SemanticPca {
    pub file: String,
    pub input_dim: usize,
    pub n_components: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub built_at: String,
    pub n_tracks: usize,
    pub n_dims: usize,
    pub track_ids: Vec<i64>,
    pub blocks: Vec<Block>,
    pub weights: HashMap<String, f32>,
    pub semantic_pca: SemanticPca,
}

/// The raw space, memory-mapped. Cheap to hold, cheap to re-weight.
pub struct Space {
    pub manifest: Manifest,
    mmap: Mmap,
    pub index_of: HashMap<i64, usize>,
}

impl Space {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let manifest_path = data_dir.join("space.json");
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(&manifest_path)
                .with_context(|| format!("reading {}", manifest_path.display()))?,
        )
        .context("parsing space.json")?;

        anyhow::ensure!(
            manifest.version == SPACE_VERSION,
            "space.json is format {}, this build reads {}, rebuild it on the server:\n  \
             two-khz-server build-space && two-khz-server layout",
            manifest.version,
            SPACE_VERSION
        );

        // Belt and braces: catches a stale space.bin next to a fresh manifest.
        let blocks_end = manifest.blocks.iter().map(|b| b.end).max().unwrap_or(0);
        anyhow::ensure!(
            blocks_end == manifest.n_dims,
            "space.json blocks cover {} dims but n_dims is {}, rebuild the space",
            blocks_end,
            manifest.n_dims
        );

        let bin_path = data_dir.join("space.bin");
        let file = File::open(&bin_path)
            .with_context(|| format!("opening {}", bin_path.display()))?;
        // Safety: the file is written once by the pipeline and only read here.
        let mmap = unsafe { Mmap::map(&file)? };

        let expected = manifest.n_tracks * manifest.n_dims * std::mem::size_of::<f32>();
        anyhow::ensure!(
            mmap.len() == expected,
            "space.bin is {} bytes, manifest implies {} ({}x{} f32), rebuild the space",
            mmap.len(),
            expected,
            manifest.n_tracks,
            manifest.n_dims
        );

        let index_of = manifest
            .track_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, i))
            .collect();

        Ok(Self {
            manifest,
            mmap,
            index_of,
        })
    }

    pub fn raw(&self) -> &[f32] {
        bytemuck::cast_slice(&self.mmap)
    }

    pub fn row(&self, i: usize) -> &[f32] {
        let d = self.manifest.n_dims;
        &self.raw()[i * d..(i + 1) * d]
    }

    pub fn default_weights(&self) -> HashMap<String, f32> {
        self.manifest.weights.clone()
    }

    /// Apply block weights and pre-normalise every row, so a similarity query is
    /// one dot product. ~50k x 81 floats, so this is a few milliseconds.
    pub fn weighted(&self, weights: &HashMap<String, f32>) -> WeightedSpace {
        let d = self.manifest.n_dims;
        let mut scale = vec![1.0f32; d];
        for block in &self.manifest.blocks {
            let w = weights
                .get(&block.name)
                .copied()
                .unwrap_or(block.default_weight);
            for s in scale.iter_mut().take(block.end).skip(block.start) {
                *s = w;
            }
        }

        let raw = self.raw();
        let mut unit = vec![0.0f32; raw.len()];
        for i in 0..self.manifest.n_tracks {
            let src = &raw[i * d..(i + 1) * d];
            let dst = &mut unit[i * d..(i + 1) * d];
            let mut norm_sq = 0.0f32;
            for k in 0..d {
                let v = src[k] * scale[k];
                dst[k] = v;
                norm_sq += v * v;
            }
            let norm = norm_sq.sqrt().max(1e-9);
            for v in dst.iter_mut() {
                *v /= norm;
            }
        }

        WeightedSpace {
            unit,
            n_tracks: self.manifest.n_tracks,
            n_dims: d,
            scale,
        }
    }

    /// PCA parameters for projecting a CLAP text embedding into the semantic block.
    pub fn semantic_pca(&self, data_dir: &Path) -> Result<(Vec<f32>, Vec<f32>)> {
        let pca = &self.manifest.semantic_pca;
        let path = data_dir.join(&pca.file);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let floats: &[f32] = bytemuck::cast_slice(&bytes);

        let expected = pca.input_dim + pca.n_components * pca.input_dim;
        anyhow::ensure!(
            floats.len() == expected,
            "{} has {} floats, expected {}",
            path.display(),
            floats.len(),
            expected
        );

        let mean = floats[..pca.input_dim].to_vec();
        let components = floats[pca.input_dim..].to_vec();
        Ok((mean, components))
    }
}

/// Weighted, row-normalised vectors ready for cosine queries.
pub struct WeightedSpace {
    pub unit: Vec<f32>,
    pub n_tracks: usize,
    pub n_dims: usize,
    /// Per-dimension weight actually applied, kept so external points can be
    /// scaled the same way.
    pub scale: Vec<f32>,
}

impl WeightedSpace {
    pub fn row(&self, i: usize) -> &[f32] {
        &self.unit[i * self.n_dims..(i + 1) * self.n_dims]
    }

    /// Cosine similarity of every track against a query vector.
    pub fn similarities(&self, query: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; self.n_tracks];
        self.similarities_into(query, &mut out);
        out
    }

    /// The same, into a caller-owned buffer, the kNN build runs this once
    /// per track and a fresh vector each time is pure churn. The arithmetic is
    /// deliberately identical to `similarities`: a different summation order
    /// can reorder near-ties between the two.
    pub fn similarities_into(&self, query: &[f32], out: &mut [f32]) {
        let norm = query
            .iter()
            .map(|v| v * v)
            .sum::<f32>()
            .sqrt()
            .max(1e-9);

        for (i, slot) in out.iter_mut().enumerate().take(self.n_tracks) {
            let row = self.row(i);
            *slot = row.iter().zip(query).map(|(a, b)| a * b).sum::<f32>() / norm;
        }
    }
}

/// Indices of the `k` highest scores, ordered as `argsort_desc` would.
///
/// Selecting beats sorting when k is small, the graph build wants 16 of
/// several thousand. The comparator is a total order, so the result is exactly
/// the prefix a full sort would give.
pub fn top_k(scores: &[f32], k: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    let cmp = |&a: &usize, &b: &usize| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    };

    if k >= order.len() {
        order.sort_by(cmp);
        return order;
    }

    order.select_nth_unstable_by(k, cmp);
    order.truncate(k);
    order.sort_by(cmp);
    order
}

/// Sort indices by descending score, breaking ties by index. The tie-break is
/// what keeps a ranking stable from one run to the next.
pub fn argsort_desc(scores: &[f32]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    order
}
