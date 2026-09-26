//! The layout stage: the space projected to 2D for the map, by UMAP.
//!
//! A port of the parts of umap-learn this needs, for one metric (cosine, over
//! the weighted space) and one purpose (a picture). Where umap-learn gives a
//! choice, this takes the default, except the initial layout: the first two
//! principal components rather than a spectral embedding, which is simpler
//! and, for a map nobody measures, as good.
//!
//! Deterministic: same space, same map.

use super::assemble::fit_pca;
use super::{Job, Paths};
use crate::db;
use crate::schema::layout;
use anyhow::{bail, Result};
use diesel::prelude::*;
use std::collections::HashMap;
use two_khz::space::Space;

#[derive(Debug, Clone)]
pub struct Options {
    pub neighbours: usize,
    pub min_dist: f64,
    /// 0 picks umap-learn's default: 200 epochs for large corpora, 500 else.
    pub epochs: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            neighbours: 15,
            min_dist: 0.1,
            epochs: 0,
        }
    }
}

const SPREAD: f64 = 1.0;
const NEGATIVE_SAMPLE_RATE: usize = 5;
const REPULSION: f64 = 1.0;
const SEED: u64 = 42;

/// Lay out the space as it stands, and store the coordinates. Returns how many
/// tracks were placed.
pub fn run(paths: &Paths, options: &Options, job: &Job) -> Result<usize> {
    let space = Space::load(&paths.data_dir)?;
    let weighted = space.weighted(&space.default_weights());
    let (n, d) = (weighted.n_tracks, weighted.n_dims);
    if n < 3 {
        bail!("{n} tracks is too few to lay out");
    }

    job.log(format!("laying out {n} tracks ({d} dims)"));
    let coords = umap(&weighted.unit, n, d, options, job)?;
    if job.cancelled() {
        bail!("cancelled");
    }

    let mut conn = db::open_for_write(&paths.db_path)?;
    // Rewritten wholesale rather than upserted: the layout is a function of
    // the space, so a track that has left it must not keep its coordinates.
    let stale = conn.transaction::<_, diesel::result::Error, _>(|conn| {
        let stale = diesel::delete(layout::table).execute(conn)?;
        for (id, &(x, y)) in space.manifest.track_ids.iter().zip(&coords) {
            diesel::insert_into(layout::table)
                .values((layout::track_id.eq(id), layout::x.eq(x), layout::y.eq(y)))
                .execute(conn)?;
        }
        Ok(stale)
    })?;

    if stale > n {
        job.log(format!("dropped {} stale layout rows", stale - n));
    }
    job.log(format!("laid out {n} tracks"));
    Ok(n)
}

/// 2D coordinates for `n` unit-length rows of `d` floats.
pub fn umap(data: &[f32], n: usize, d: usize, options: &Options, job: &Job) -> Result<Vec<(f64, f64)>> {
    // Tiny corpora cannot have fifteen neighbours.
    let k = options.neighbours.min(n - 1).max(2);

    job.log(format!("  nearest {k} neighbours…"));
    let knn = nearest(data, n, d, k);
    if job.cancelled() {
        bail!("cancelled");
    }
    let graph = fuzzy_graph(&knn, n, k);

    let (a, b) = fit_ab(options.min_dist, SPREAD);
    let epochs = match options.epochs {
        0 if n > 10_000 => 200,
        0 => 500,
        e => e,
    };
    job.log(format!("  optimising {} edges over {epochs} epochs…", graph.len()));

    let mut embedding = initial(data, n, d);
    optimise(&mut embedding, &graph, n, a, b, epochs, job)?;
    Ok(embedding)
}

// ---------------------------------------------------------------- neighbours

/// (index, cosine distance) of each row's k nearest, self excluded, nearest
/// first. Brute force, spread over the cores: quadratic, but a 30k corpus is
/// seconds and the stage runs rarely.
fn nearest(data: &[f32], n: usize, d: usize, k: usize) -> Vec<Vec<(usize, f64)>> {
    let threads = std::thread::available_parallelism()
        .map(|t| t.get())
        .unwrap_or(4)
        .min(n);
    let chunk = n.div_ceil(threads);
    let mut out: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];

    std::thread::scope(|scope| {
        for (t, slots) in out.chunks_mut(chunk).enumerate() {
            scope.spawn(move || {
                let mut scores = vec![0f32; n];
                for (offset, slot) in slots.iter_mut().enumerate() {
                    let i = t * chunk + offset;
                    let row = &data[i * d..(i + 1) * d];
                    for (j, score) in scores.iter_mut().enumerate() {
                        *score = row.iter().zip(&data[j * d..(j + 1) * d]).map(|(a, b)| a * b).sum();
                    }
                    scores[i] = f32::NEG_INFINITY;
                    let order = two_khz::space::top_k(&scores, k);
                    *slot = order
                        .into_iter()
                        .map(|j| (j, (1.0 - scores[j] as f64).max(0.0)))
                        .collect();
                }
            });
        }
    });
    out
}

/// Each point's neighbourhood as fuzzy memberships, then the fuzzy union of
/// the directed graph with its transpose. Every undirected edge is returned
/// once per direction, as umap-learn samples them.
fn fuzzy_graph(knn: &[Vec<(usize, f64)>], n: usize, k: usize) -> Vec<(usize, usize, f64)> {
    // Self counts as a neighbour at distance zero in umap-learn, contributing
    // 1 to the sum it calibrates against log2(k + 1).
    let target = ((k + 1) as f64).log2() - 1.0;
    let mut directed: HashMap<(usize, usize), f64> = HashMap::with_capacity(n * k);

    for (i, neighbours) in knn.iter().enumerate() {
        let rho = neighbours
            .iter()
            .map(|&(_, dist)| dist)
            .find(|&dist| dist > 0.0)
            .unwrap_or(0.0);
        let mean_distance =
            neighbours.iter().map(|&(_, dist)| dist).sum::<f64>() / neighbours.len().max(1) as f64;

        // Binary search the bandwidth.
        let (mut lo, mut hi, mut sigma) = (0.0, f64::INFINITY, 1.0);
        for _ in 0..64 {
            let total: f64 = neighbours
                .iter()
                .map(|&(_, dist)| {
                    let gap = dist - rho;
                    if gap > 0.0 { (-gap / sigma).exp() } else { 1.0 }
                })
                .sum();
            if (total - target).abs() < 1e-5 {
                break;
            }
            if total > target {
                hi = sigma;
                sigma = (lo + hi) / 2.0;
            } else {
                lo = sigma;
                sigma = if hi.is_infinite() { sigma * 2.0 } else { (lo + hi) / 2.0 };
            }
        }
        sigma = sigma.max(1e-3 * mean_distance).max(1e-12);

        for &(j, dist) in neighbours {
            let weight = if dist - rho <= 0.0 { 1.0 } else { (-(dist - rho) / sigma).exp() };
            directed.insert((i, j), weight);
        }
    }

    let mut edges = Vec::with_capacity(directed.len() * 2);
    for (&(i, j), &w) in &directed {
        match directed.get(&(j, i)) {
            // Both directions exist: emit the union once from each side.
            Some(&v) => edges.push((i, j, w + v - w * v)),
            None => {
                edges.push((i, j, w));
                edges.push((j, i, w));
            }
        }
    }
    // HashMap order is not stable; the sampling below must be.
    edges.sort_by_key(|e| (e.0, e.1));
    edges
}

// ------------------------------------------------------------------- curve

/// The a, b of 1 / (1 + a d^2b) that best match "1 inside min_dist, then an
/// exponential fall-off", by least squares. umap-learn uses scipy's
/// curve_fit; a shrinking grid search lands on the same values.
pub fn fit_ab(min_dist: f64, spread: f64) -> (f64, f64) {
    let xs: Vec<f64> = (0..300).map(|i| 3.0 * spread * i as f64 / 299.0).collect();
    let ys: Vec<f64> = xs
        .iter()
        .map(|&x| if x < min_dist { 1.0 } else { (-(x - min_dist) / spread).exp() })
        .collect();
    let error = |a: f64, b: f64| -> f64 {
        xs.iter()
            .zip(&ys)
            .map(|(&x, &y)| (1.0 / (1.0 + a * x.powf(2.0 * b)) - y).powi(2))
            .sum()
    };

    let (mut a, mut b) = (1.0, 1.0);
    let (mut a_step, mut b_step) = (1.0, 0.5);
    for _ in 0..60 {
        let mut best = (error(a, b), a, b);
        for da in [-1.0, 0.0, 1.0] {
            for db in [-1.0, 0.0, 1.0] {
                let (ca, cb) = (a + da * a_step, b + db * b_step);
                if ca <= 0.0 || cb <= 0.0 {
                    continue;
                }
                let e = error(ca, cb);
                if e < best.0 {
                    best = (e, ca, cb);
                }
            }
        }
        if best.1 == a && best.2 == b {
            a_step /= 2.0;
            b_step /= 2.0;
        }
        (a, b) = (best.1, best.2);
    }
    (a, b)
}

// -------------------------------------------------------------- optimisation

/// The first two principal components, scaled to [-10, 10] as umap-learn
/// scales its initialisation.
fn initial(data: &[f32], n: usize, d: usize) -> Vec<(f64, f64)> {
    let rows: Vec<Vec<f64>> = (0..n)
        .map(|i| data[i * d..(i + 1) * d].iter().map(|&v| v as f64).collect())
        .collect();
    let (mean, components) = fit_pca(&rows, 2);
    let axis = |row: &[f64], c: usize| -> f64 {
        components.get(c).map_or(0.0, |w| {
            w.iter().zip(row.iter().zip(&mean)).map(|(w, (v, m))| w * (v - m)).sum()
        })
    };
    let mut points: Vec<(f64, f64)> = rows.iter().map(|r| (axis(r, 0), axis(r, 1))).collect();

    // A little jitter, so coincident points can separate.
    let mut rng = Rng::new(SEED ^ 0x9e37_79b9);
    for axis in 0..2 {
        let values = points.iter().map(|p| if axis == 0 { p.0 } else { p.1 });
        let (lo, hi) = values.fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
        let span = (hi - lo).max(1e-12);
        for p in points.iter_mut() {
            let v = if axis == 0 { &mut p.0 } else { &mut p.1 };
            *v = 20.0 * (*v - lo) / span - 10.0 + 1e-4 * (rng.unit() - 0.5);
        }
    }
    points
}

fn clip(v: f64) -> f64 {
    v.clamp(-4.0, 4.0)
}

/// umap-learn's `optimize_layout_euclidean`: each edge is sampled in
/// proportion to its weight, pulled together, and a few random points pushed
/// away from its head.
fn optimise(
    embedding: &mut [(f64, f64)],
    edges: &[(usize, usize, f64)],
    n: usize,
    a: f64,
    b: f64,
    epochs: usize,
    job: &Job,
) -> Result<()> {
    let max_weight = edges.iter().map(|e| e.2).fold(0.0, f64::max);
    // Edges too weak to be sampled even once are dropped, as umap-learn does.
    let edges: Vec<&(usize, usize, f64)> = edges
        .iter()
        .filter(|e| e.2 >= max_weight / epochs as f64)
        .collect();
    let per_sample: Vec<f64> = edges.iter().map(|e| max_weight / e.2).collect();
    let per_negative: Vec<f64> = per_sample
        .iter()
        .map(|s| s / NEGATIVE_SAMPLE_RATE as f64)
        .collect();
    let mut next_sample = per_sample.clone();
    let mut next_negative = per_negative.clone();
    let mut rng = Rng::new(SEED);

    for epoch in 0..epochs {
        if epoch % 25 == 0 && job.cancelled() {
            bail!("cancelled");
        }
        let alpha = 1.0 - epoch as f64 / epochs as f64;
        let now = epoch as f64;

        for (e, &&(i, j, _)) in edges.iter().enumerate() {
            if next_sample[e] > now {
                continue;
            }

            let (dx, dy) = (embedding[i].0 - embedding[j].0, embedding[i].1 - embedding[j].1);
            let dist_sq = dx * dx + dy * dy;
            if dist_sq > 0.0 {
                let coefficient =
                    -2.0 * a * b * dist_sq.powf(b - 1.0) / (a * dist_sq.powf(b) + 1.0);
                let (gx, gy) = (clip(coefficient * dx) * alpha, clip(coefficient * dy) * alpha);
                embedding[i].0 += gx;
                embedding[i].1 += gy;
                embedding[j].0 -= gx;
                embedding[j].1 -= gy;
            }
            next_sample[e] += per_sample[e];

            let negatives = ((now - next_negative[e]) / per_negative[e]).floor().max(0.0) as usize;
            for _ in 0..negatives {
                let other = rng.below(n);
                if other == i {
                    continue;
                }
                let (dx, dy) = (
                    embedding[i].0 - embedding[other].0,
                    embedding[i].1 - embedding[other].1,
                );
                let dist_sq = dx * dx + dy * dy;
                let (gx, gy) = if dist_sq > 0.0 {
                    let coefficient =
                        2.0 * REPULSION * b / ((0.001 + dist_sq) * (a * dist_sq.powf(b) + 1.0));
                    (clip(coefficient * dx), clip(coefficient * dy))
                } else {
                    (4.0, 4.0)
                };
                embedding[i].0 += gx * alpha;
                embedding[i].1 += gy * alpha;
            }
            next_negative[e] += negatives as f64 * per_negative[e];
        }
    }
    Ok(())
}

/// xorshift64*: small, seedable, and good enough to pick negative samples.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_matches_umap_learn_for_the_defaults() {
        // umap-learn's find_ab_params(1.0, 0.1).
        let (a, b) = fit_ab(0.1, 1.0);
        assert!((a - 1.577).abs() < 0.02, "a = {a}");
        assert!((b - 0.895).abs() < 0.01, "b = {b}");
    }

    #[test]
    fn clusters_stay_apart_on_the_map() {
        // Three tight clusters around orthogonal directions in 6 dims.
        let (n, d) = (90, 6);
        let mut rng = Rng::new(7);
        let mut data = Vec::with_capacity(n * d);
        for i in 0..n {
            let cluster = i / 30;
            let mut row: Vec<f32> = (0..d).map(|_| (rng.unit() as f32 - 0.5) * 0.2).collect();
            row[cluster * 2] += 1.0;
            let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt();
            data.extend(row.iter().map(|v| v / norm));
        }

        let coords = umap(&data, n, d, &Options::default(), &Job::stderr()).unwrap();
        let centre = |c: usize| {
            let pts = &coords[c * 30..(c + 1) * 30];
            let (x, y) = pts.iter().fold((0.0, 0.0), |acc, p| (acc.0 + p.0, acc.1 + p.1));
            (x / 30.0, y / 30.0)
        };
        let spread = |c: usize| {
            let (cx, cy) = centre(c);
            coords[c * 30..(c + 1) * 30]
                .iter()
                .map(|p| ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt())
                .fold(0.0, f64::max)
        };
        for c in 0..3 {
            for other in (c + 1)..3 {
                let ((ax, ay), (bx, by)) = (centre(c), centre(other));
                let gap = ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt();
                assert!(gap > spread(c) + spread(other), "clusters {c} and {other} overlap");
            }
        }
    }
}
