//! The build-space stage: stored features -> the vector space.
//!
//! A pure function of the database, no network, no audio, so rebuilding
//! after a weight or label change takes seconds.
//!
//! Output, read by `two_khz::space` on every client:
//!   space.bin          [n_tracks x n_dims] f32, per-block normalised and
//!                      UNWEIGHTED: weights are applied at query time, so the
//!                      sliders reshape distances live.
//!   space.json         track id order, block layout, default weights.
//!   semantic_pca.bin   mean + components, so a CLAP text vector can be
//!                      projected into the semantic block for steering.

use super::{labels, models, Job, Paths};
use crate::db::{self, not_blocked};
use crate::schema::{albums, features, tracks};
use crate::text::TextEncoder;
use anyhow::{bail, Context, Result};
use diesel::prelude::*;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use two_khz::space::SPACE_VERSION;

const BPM_FOLD_LOW: f64 = 70.0;
const BPM_FOLD_HIGH: f64 = 140.0;
const N_STYLE_COMPONENTS: usize = 20;
const N_SEMANTIC_COMPONENTS: usize = 40;

/// Fixed, so the client can rely on it.
pub const BLOCKS: [&str; 8] = [
    "tempo", "key", "dynamics", "timbre", "mood", "style", "era", "semantic",
];

pub fn default_weights() -> HashMap<String, f32> {
    [
        ("tempo", 1.0),
        ("key", 0.4),
        ("dynamics", 0.8),
        ("timbre", 0.6),
        ("mood", 1.2),
        ("style", 1.5),
        ("era", 0.3),
        ("semantic", 2.0),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

// --------------------------------------------------------------- primitives

/// log2 of the tempo folded into [70, 140) BPM, so a track detected at half
/// or double time lands in the same place.
pub fn fold_bpm(bpm: Option<f64>) -> Option<f64> {
    let mut value = bpm.filter(|b| b.is_finite() && *b > 0.0)?;
    while value < BPM_FOLD_LOW {
        value *= 2.0;
    }
    while value >= BPM_FOLD_HIGH {
        value /= 2.0;
    }
    Some(value.log2())
}

/// A key on the circle of fifths, so adjacent keys are adjacent points. The
/// radius is the detection strength: an ambiguous key sits near the origin
/// rather than confidently in the wrong place.
pub fn key_coordinates(key: Option<&str>, scale: Option<&str>, strength: Option<f64>) -> [f64; 3] {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let Some(index) = key.and_then(|k| NAMES.iter().position(|n| *n == k)) else {
        return [0.0; 3];
    };
    let radius = strength.filter(|s| s.is_finite()).unwrap_or(0.5);
    let position = (index * 7) % 12;
    let angle = 2.0 * std::f64::consts::PI * position as f64 / 12.0;
    let mode = if scale == Some("major") { 1.0 } else { -1.0 };
    [radius * angle.cos(), radius * angle.sin(), radius * mode]
}

/// PCA by eigendecomposition of the covariance. Returns (mean [d],
/// components [k x d]), each component's sign fixed so its largest
/// coefficient is positive, otherwise a rebuild could flip an axis for no
/// reason.
pub fn fit_pca(rows: &[Vec<f64>], k: usize) -> (Vec<f64>, Vec<Vec<f64>>) {
    let n = rows.len();
    let d = rows.first().map_or(0, |r| r.len());
    let mut mean = vec![0f64; d];
    for row in rows {
        for (m, v) in mean.iter_mut().zip(row) {
            *m += v / n as f64;
        }
    }

    let mut covariance = nalgebra::DMatrix::<f64>::zeros(d, d);
    let mut centred = vec![0f64; d];
    for row in rows {
        for (c, (v, m)) in centred.iter_mut().zip(row.iter().zip(&mean)) {
            *c = v - m;
        }
        for i in 0..d {
            let ci = centred[i];
            if ci == 0.0 {
                continue;
            }
            for j in i..d {
                covariance[(i, j)] += ci * centred[j];
            }
        }
    }
    for i in 0..d {
        for j in 0..i {
            covariance[(i, j)] = covariance[(j, i)];
        }
    }

    let eigen = covariance.symmetric_eigen();
    let mut order: Vec<usize> = (0..d).collect();
    order.sort_by(|&a, &b| eigen.eigenvalues[b].total_cmp(&eigen.eigenvalues[a]));

    let k = k.min(n).min(d);
    let components = order[..k]
        .iter()
        .map(|&c| {
            let mut v: Vec<f64> = eigen.eigenvectors.column(c).iter().copied().collect();
            let largest = v.iter().cloned().fold(0f64, |a, x| if x.abs() > a.abs() { x } else { a });
            if largest < 0.0 {
                v.iter_mut().for_each(|x| *x = -*x);
            }
            v
        })
        .collect();
    (mean, components)
}

fn project(rows: &[Vec<f64>], mean: &[f64], components: &[Vec<f64>]) -> Vec<Vec<f64>> {
    rows.iter()
        .map(|row| {
            components
                .iter()
                .map(|c| c.iter().zip(row.iter().zip(mean)).map(|(w, (v, m))| w * (v - m)).sum())
                .collect()
        })
        .collect()
}

/// Z-score each column, then L2-normalise each row. Column scaling stops a
/// wide-ranged descriptor swamping its neighbours; row normalisation makes
/// every block count equally before weights, so a 40-d block cannot quietly
/// outvote a 2-d one. Returns (block, column means, column stds).
pub fn normalise_block(rows: &[Vec<f64>]) -> (Vec<Vec<f32>>, Vec<f64>, Vec<f64>) {
    let n = rows.len() as f64;
    let d = rows.first().map_or(0, |r| r.len());
    let clean = |v: f64| if v.is_finite() { v } else { 0.0 };

    let mut mean = vec![0f64; d];
    for row in rows {
        for (m, v) in mean.iter_mut().zip(row) {
            *m += clean(*v) / n;
        }
    }
    let mut std = vec![0f64; d];
    for row in rows {
        for ((s, v), m) in std.iter_mut().zip(row).zip(&mean) {
            *s += (clean(*v) - m).powi(2) / n;
        }
    }
    for s in std.iter_mut() {
        *s = s.sqrt();
        if *s < 1e-9 {
            *s = 1.0;
        }
    }

    let block = rows
        .iter()
        .map(|row| {
            let scaled: Vec<f64> = row
                .iter()
                .zip(mean.iter().zip(&std))
                .map(|(v, (m, s))| (clean(*v) - m) / s)
                .collect();
            let norm = scaled.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-9);
            scaled.iter().map(|v| (v / norm) as f32).collect()
        })
        .collect();
    (block, mean, std)
}

/// Missing values become the column median; an all-missing column, zeros.
fn impute(values: Vec<Option<f64>>) -> Vec<f64> {
    let mut known: Vec<f64> = values.iter().flatten().copied().filter(|v| v.is_finite()).collect();
    if known.is_empty() {
        return vec![0.0; values.len()];
    }
    known.sort_by(f64::total_cmp);
    let mid = known.len() / 2;
    let median = if known.len().is_multiple_of(2) {
        (known[mid - 1] + known[mid]) / 2.0
    } else {
        known[mid]
    };
    values
        .into_iter()
        .map(|v| v.filter(|x| x.is_finite()).unwrap_or(median))
        .collect()
}

fn columns(columns: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    let n = columns.first().map_or(0, |c| c.len());
    (0..n).map(|i| columns.iter().map(|c| c[i]).collect()).collect()
}

// -------------------------------------------------------------------- build

struct Row {
    id: i64,
    release_date: Option<String>,
    descriptors: serde_json::Value,
    clap: Vec<f32>,
}

/// Every analysed track not by a blocked artist, in id order. The app also
/// filters blocks at load time, which is what lets one take effect before
/// the next rebuild.
fn load_rows(conn: &mut SqliteConnection) -> Result<Vec<Row>> {
    #[derive(Queryable)]
    struct Stored {
        id: i64,
        release_date: Option<String>,
        json: Option<String>,
        blob: Option<Vec<u8>>,
    }

    let stored: Vec<Stored> = tracks::table
        .inner_join(features::table)
        .left_join(albums::table)
        .filter(not_blocked())
        .filter(features::clap_f32.is_not_null())
        .order(tracks::id)
        .select((
            tracks::id,
            albums::release_date.nullable(),
            features::descriptors_json,
            features::clap_f32,
        ))
        .load(conn)?;
    Ok(stored
        .into_iter()
        .map(|Stored { id, release_date, json, blob }| Row {
            id,
            release_date,
            descriptors: json
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or(serde_json::Value::Null),
            clap: blob
                .unwrap_or_default()
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        })
        .filter(|r| r.clap.len() == super::clap::EMBEDDING_DIMS)
        .collect())
}

fn release_year(date: Option<&str>) -> Option<f64> {
    let year = date?.get(..4)?;
    year.chars().all(|c| c.is_ascii_digit()).then(|| year.parse().ok())?
}

/// Text-embedding directions, one per label.
type Directions = Vec<Vec<f32>>;

/// Embed the label phrases: (moods, styles). Each mood is one direction, one
/// end minus the other.
fn label_vectors(encoder: &mut TextEncoder) -> Result<(Directions, Directions)> {
    let mut moods = Vec::new();
    for (_, one, other) in labels::MOODS {
        let (a, b) = (encoder.embed(one)?, encoder.embed(other)?);
        moods.push(a.iter().zip(&b).map(|(x, y)| x - y).collect());
    }
    let mut styles = Vec::new();
    for style in labels::STYLES {
        styles.push(encoder.embed(&labels::style_prompt(style))?);
    }
    Ok((moods, styles))
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (*x as f64) * (*y as f64)).sum()
}

/// Build `space.bin`, `space.json` and `semantic_pca.bin` from what is stored.
pub async fn run(paths: &Paths, weights: Option<HashMap<String, f32>>, job: &Job) -> Result<()> {
    let rows = load_rows(&mut db::open_for_write(&paths.db_path)?)?;
    if rows.is_empty() {
        bail!("no analysed tracks; run analyse first");
    }

    models::ensure(&paths.model_dir, &[models::CLAP_TEXT, models::CLAP_TOKENIZER], job).await?;
    let mut encoder = TextEncoder::load(&paths.model_dir)?
        .context("the CLAP text tower is missing after fetching it")?;

    let mut weights_all = default_weights();
    weights_all.extend(weights.unwrap_or_default());
    job.log(format!("building space from {} tracks", rows.len()));

    let (mood_directions, style_directions) = label_vectors(&mut encoder)?;
    build(&paths.data_dir, &rows, &mood_directions, &style_directions, &weights_all, job)
}

fn build(
    data_dir: &Path,
    rows: &[Row],
    mood_directions: &[Vec<f32>],
    style_directions: &[Vec<f32>],
    weights: &HashMap<String, f32>,
    job: &Job,
) -> Result<()> {
    let n = rows.len();
    let number = |row: &Row, key: &str| row.descriptors.get(key).and_then(|v| v.as_f64());
    let text = |row: &Row, key: &str| {
        row.descriptors
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let column = |key: &str| impute(rows.iter().map(|r| number(r, key)).collect());

    // (name, rows, column names), in the order they are laid out.
    type Block<'a> = (&'a str, Vec<Vec<f64>>, Vec<String>);
    let mut blocks: Vec<Block> = Vec::new();

    blocks.push((
        "tempo",
        columns(vec![
            impute(rows.iter().map(|r| fold_bpm(number(r, "bpm"))).collect()),
            column("onset_rate"),
        ]),
        vec!["log_bpm_folded".into(), "onset_rate".into()],
    ));

    blocks.push((
        "key",
        rows.iter()
            .map(|r| {
                key_coordinates(
                    text(r, "key").as_deref(),
                    text(r, "scale").as_deref(),
                    number(r, "key_strength"),
                )
                .to_vec()
            })
            .collect(),
        vec!["fifths_cos".into(), "fifths_sin".into(), "mode".into()],
    ));

    let named = |names: &[&str]| -> (Vec<Vec<f64>>, Vec<String>) {
        (
            columns(names.iter().map(|n| column(n)).collect()),
            names.iter().map(|n| n.to_string()).collect(),
        )
    };
    let (dynamics, dynamics_names) =
        named(&["loudness_integrated", "dynamic_complexity", "loudness_range"]);
    blocks.push(("dynamics", dynamics, dynamics_names));
    let (timbre, timbre_names) = named(&[
        "spectral_centroid",
        "spectral_rolloff",
        "spectral_flatness",
        "zero_crossing_rate",
    ]);
    blocks.push(("timbre", timbre, timbre_names));

    // mood: one contrast per axis.
    blocks.push((
        "mood",
        rows.iter()
            .map(|r| mood_directions.iter().map(|d| dot(&r.clap, d)).collect())
            .collect(),
        labels::MOODS.iter().map(|m| m.0.to_string()).collect(),
    ));

    // style: similarity to every style phrase, then PCA.
    let style_scores: Vec<Vec<f64>> = rows
        .iter()
        .map(|r| style_directions.iter().map(|d| dot(&r.clap, d)).collect())
        .collect();
    let (style_mean, style_components) = fit_pca(&style_scores, N_STYLE_COMPONENTS);
    let style = project(&style_scores, &style_mean, &style_components);
    let style_names = (0..style_components.len()).map(|i| format!("style_pc{i}")).collect();
    blocks.push(("style", style, style_names));

    blocks.push((
        "era",
        impute(rows.iter().map(|r| release_year(r.release_date.as_deref())).collect())
            .into_iter()
            .map(|v| vec![v])
            .collect(),
        vec!["release_year".into()],
    ));

    // semantic: CLAP 512 -> PCA.
    let clap: Vec<Vec<f64>> = rows
        .iter()
        .map(|r| r.clap.iter().map(|&v| v as f64).collect())
        .collect();
    let (semantic_mean, semantic_components) = fit_pca(&clap, N_SEMANTIC_COMPONENTS);
    let semantic = project(&clap, &semantic_mean, &semantic_components);
    let semantic_names = (0..semantic_components.len()).map(|i| format!("clap_pc{i}")).collect();
    blocks.push(("semantic", semantic, semantic_names));

    // Normalise and lay out, in the fixed order.
    debug_assert!(blocks.iter().map(|b| b.0).eq(BLOCKS));
    let mut matrix = vec![Vec::<f32>::new(); n];
    let mut layout = Vec::new();
    let mut cursor = 0;
    for (name, block, names) in &blocks {
        let (normalised, mean, std) = normalise_block(block);
        let width = normalised.first().map_or(0, |r| r.len());
        for (row, part) in matrix.iter_mut().zip(normalised) {
            row.extend(part);
        }
        layout.push(json!({
            "name": name,
            "start": cursor,
            "end": cursor + width,
            "columns": names,
            "default_weight": weights[*name],
            "mean": mean,
            "std": std,
        }));
        job.log(format!(
            "  {name:<10} dims {cursor:>3}..{:<3} weight {}",
            cursor + width,
            weights[*name]
        ));
        cursor += width;
    }
    let n_dims = cursor;

    std::fs::create_dir_all(data_dir)?;
    let flat: Vec<f32> = matrix.into_iter().flatten().collect();
    write_atomic(&data_dir.join("space.bin"), bytemuck::cast_slice(&flat))?;

    // Semantic PCA params, so a CLAP text vector can be projected into the
    // semantic block and used as a steering direction.
    let mut pca: Vec<f32> = semantic_mean.iter().map(|&v| v as f32).collect();
    for component in &semantic_components {
        pca.extend(component.iter().map(|&v| v as f32));
    }
    write_atomic(&data_dir.join("semantic_pca.bin"), bytemuck::cast_slice(&pca))?;

    let manifest = json!({
        "version": SPACE_VERSION,
        "built_at": db::utc_now(),
        "n_tracks": n,
        "n_dims": n_dims,
        "track_ids": rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        "blocks": layout,
        "weights": weights,
        "semantic_pca": {
            "file": "semantic_pca.bin",
            "input_dim": super::clap::EMBEDDING_DIMS,
            "n_components": semantic_components.len(),
        },
    });
    // Last, so a client that sees the new manifest also finds the new vectors.
    write_atomic(
        &data_dir.join("space.json"),
        serde_json::to_string_pretty(&manifest)?.as_bytes(),
    )?;

    job.log(format!("space: {n} tracks x {n_dims} dims"));
    Ok(())
}

/// Written beside the target and renamed, so a client syncing mid-build
/// never reads half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let staging = path.with_extension("partial");
    std::fs::write(&staging, bytes).with_context(|| format!("writing {}", staging.display()))?;
    std::fs::rename(&staging, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bpm_folds_into_one_octave() {
        assert_eq!(fold_bpm(Some(172.0)), fold_bpm(Some(86.0)));
        assert_eq!(fold_bpm(Some(60.0)), fold_bpm(Some(120.0)));
        assert_eq!(fold_bpm(Some(0.0)), None);
        assert_eq!(fold_bpm(None), None);
    }

    #[test]
    fn keys_a_fifth_apart_are_neighbours() {
        let c = key_coordinates(Some("C"), Some("major"), Some(1.0));
        let g = key_coordinates(Some("G"), Some("major"), Some(1.0));
        let f_sharp = key_coordinates(Some("F#"), Some("major"), Some(1.0));
        let distance = |a: [f64; 3], b: [f64; 3]| {
            a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>().sqrt()
        };
        assert!(distance(c, g) < distance(c, f_sharp));
        assert_eq!(key_coordinates(None, None, None), [0.0; 3]);
    }

    #[test]
    fn pca_finds_the_long_axis() {
        // Points spread along (1, 1) with a little noise across it.
        let rows: Vec<Vec<f64>> = (0..200)
            .map(|i| {
                let t = i as f64 / 10.0 - 10.0;
                let noise = ((i * 37) % 11) as f64 / 100.0 - 0.05;
                vec![t + noise, t - noise, 3.0]
            })
            .collect();
        let (mean, components) = fit_pca(&rows, 2);
        assert!((mean[2] - 3.0).abs() < 1e-12);
        let first = &components[0];
        let s = std::f64::consts::FRAC_1_SQRT_2;
        assert!((first[0] - s).abs() < 1e-3 && (first[1] - s).abs() < 1e-3, "{first:?}");
    }

    #[test]
    fn normalised_rows_are_unit_length_and_columns_centred() {
        let rows = vec![vec![1.0, 100.0], vec![2.0, 300.0], vec![3.0, f64::NAN]];
        let (block, mean, _) = normalise_block(&rows);
        for row in &block {
            let norm: f32 = row.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5 || norm < 1e-5);
        }
        assert!((mean[0] - 2.0).abs() < 1e-12);
    }

    #[test]
    fn missing_values_take_the_median() {
        assert_eq!(impute(vec![Some(1.0), None, Some(5.0), Some(3.0)]), vec![1.0, 3.0, 5.0, 3.0]);
        assert_eq!(impute(vec![None, None]), vec![0.0, 0.0]);
    }
}
