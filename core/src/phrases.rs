//! Each mix-analysed track's eight-bar phrases, as points of the space: where
//! each starts, and its sound as the space's semantic block would hold it.
//! One body, `/api/mix/phrases`, for stelly-explorer to draw a track as the
//! path its phrases take through the space.
//!
//! A phrase has only its sound of its own: its tempo, key and mood are its
//! track's. So it is the semantic block alone, and a client puts it in its
//! track's row in place of the track's own to have the phrase's.
//!
//! Laid out as:
//!
//! ```text
//!   MAGIC                    4 bytes
//!   head length              u32
//!   head                     JSON, `PhrasesHead`
//!   track ids                i64 x tracks
//!   phrase counts            u16 x tracks
//!   starts                   f32 x phrases, seconds into the track
//!   rows                     i8 x phrases·dims, see `vectors::quantise`
//! ```
//!
//! Little-endian. The rows are normalised as the space's own are, z-scored
//! by the block's column means and spreads and then to unit length, so they
//! sit between -1 and 1 like the rest.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 4] = *b"\0sp1";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PhrasesHead {
    pub stamp: String,
    /// The block the rows stand in for, and where it sits in a space row.
    pub block: String,
    pub start: usize,
    pub dims: usize,
    pub tracks: usize,
    pub phrases: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Phrases {
    pub head: PhrasesHead,
    pub ids: Vec<i64>,
    pub counts: Vec<u16>,
    pub starts: Vec<f32>,
    pub rows: Vec<i8>,
}

impl Phrases {
    /// Where each track's phrases begin in `starts` and `rows`, and the end.
    pub fn offsets(&self) -> Vec<usize> {
        let mut at = Vec::with_capacity(self.counts.len() + 1);
        at.push(0);
        for c in &self.counts {
            at.push(at.last().copied().unwrap_or(0) + *c as usize);
        }
        at
    }

    pub fn row(&self, phrase: usize) -> &[i8] {
        let d = self.head.dims;
        &self.rows[phrase * d..(phrase + 1) * d]
    }

    pub fn encode(&self) -> Vec<u8> {
        let head = serde_json::to_vec(&self.head).unwrap_or_default();
        let mut out = Vec::with_capacity(8 + head.len() + self.ids.len() * 10 + self.starts.len() * 4 + self.rows.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&(head.len() as u32).to_le_bytes());
        out.extend_from_slice(&head);
        self.ids.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        self.counts.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        self.starts.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        out.extend(self.rows.iter().map(|v| *v as u8));
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Phrases> {
        ensure!(bytes.starts_with(&MAGIC), "not a phrases body");
        let mut at = 4;
        let mut take = |len: usize| -> Result<&[u8]> {
            let slice = bytes.get(at..at + len).context("the phrases body ends early")?;
            at += len;
            Ok(slice)
        };
        let head_len = u32::from_le_bytes(take(4)?.try_into()?) as usize;
        let head: PhrasesHead = serde_json::from_slice(take(head_len)?).context("reading the phrases head")?;
        let (t, p, d) = (head.tracks, head.phrases, head.dims);
        let ids: Vec<i64> = take(8 * t)?.as_chunks::<8>().0.iter().map(|c| i64::from_le_bytes(*c)).collect();
        let counts: Vec<u16> = take(2 * t)?.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        ensure!(counts.iter().map(|c| *c as usize).sum::<usize>() == p, "the counts do not add up to the phrases");
        let starts = take(4 * p)?.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        let rows = take(p * d)?.iter().map(|v| *v as i8).collect();
        Ok(Phrases { head, ids, counts, starts, rows })
    }
}

/// A phrase's CLAP embedding as the space's semantic block would hold it:
/// through the block's PCA (`mean`, then `components`, one row of `input`
/// floats each), z-scored by the block's column `means` and `stds`, and
/// brought to unit length, as `build-space` does a track's.
pub fn semantic(embedding: &[f32], mean: &[f32], components: &[f32], means: &[f32], stds: &[f32]) -> Vec<f32> {
    let input = mean.len();
    let mut out: Vec<f32> = components
        .chunks_exact(input)
        .zip(means.iter().zip(stds))
        .map(|(component, (m, s))| {
            let p: f32 = component.iter().zip(embedding.iter().zip(mean)).map(|(w, (x, mu))| w * (x - mu)).sum();
            (p - m) / s.max(1e-9)
        })
        .collect();
    let norm = out.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    out.iter_mut().for_each(|x| *x /= norm);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectors::quantise;

    #[test]
    fn round_trips() {
        let phrases = Phrases {
            head: PhrasesHead { stamp: "s".into(), block: "semantic".into(), start: 41, dims: 2, tracks: 2, phrases: 3 },
            ids: vec![7, 1 << 40],
            counts: vec![2, 1],
            starts: vec![8.0, 24.5, 0.0],
            rows: vec![quantise(1.0), quantise(-0.5), 0, 3, -127, 127],
        };
        let back = Phrases::decode(&phrases.encode()).unwrap();
        assert_eq!(back, phrases);
        assert_eq!(back.offsets(), [0, 2, 3]);
        assert_eq!(back.row(2), [-127, 127]);
        let bytes = phrases.encode();
        assert!(Phrases::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn semantic_rows_are_what_build_space_makes() {
        // A 3-d embedding through a 2-component PCA.
        let mean = [0.1, 0.0, -0.1];
        let components = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        let (means, stds) = ([0.5, 0.0], [2.0, 0.5]);
        let row = semantic(&[0.9, 0.25, 0.3], &mean, &components, &means, &stds);
        // Projected (0.8, 0.25), z-scored (0.15, 0.5), then to unit length.
        let norm = (0.15f32 * 0.15 + 0.5 * 0.5).sqrt();
        assert!((row[0] - 0.15 / norm).abs() < 1e-6 && (row[1] - 0.5 / norm).abs() < 1e-6, "{row:?}");
    }
}
