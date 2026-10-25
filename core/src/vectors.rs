//! Every track of the space with what a client needs to place and colour it
//! in a projection of its own: its row of the space, its place on the map,
//! its genre, tempo and key, and its names. One body, `/api/space/vectors`,
//! for stelly-explorer; the app's map makes do with `map`'s smaller ones.
//!
//! Laid out as:
//!
//! ```text
//!   MAGIC                    4 bytes
//!   head length              u32
//!   head                     JSON, `VectorsHead`
//!   ids                      i64 x n
//!   x, y                     f32 x 2n, NaN off the map
//!   genre                    u16 x n, into `head.genres`
//!   artist id                i64 x n
//!   rows                     i8 x n·dims, see `quantise`
//!   bpm                      f32 x n, NaN unknown
//!   key                      u8 x n, see `Key`
//!   key strength             u8 x n, 0 to 255 for 0 to 1
//!   names                    a line per track, "title␟artist␟album"
//! ```
//!
//! Little-endian throughout. The rows are the space's own, normalised per
//! block and unweighted, so each block's values lie within -1 and 1, and a
//! byte each keeps them to within 0.004: nothing a projection or a colour
//! can show, at a quarter of the size.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 4] = *b"\0sv1";

/// Separates a track's title, artist and album on its line.
const FIELD: char = '\u{1f}';

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VectorsHead {
    pub stamp: String,
    pub n: usize,
    pub dims: usize,
    pub blocks: Vec<VectorBlock>,
    pub genres: Vec<String>,
}

/// One block of the space's columns, `start..end` of each row.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VectorBlock {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub columns: Vec<String>,
    pub default_weight: f32,
}

/// A detected key: its tonic as a pitch class, C = 0 to B = 11, and its mode.
/// On the wire, `pitch + 12` for minor, 255 for none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub pitch: u8,
    pub minor: bool,
}

impl Key {
    pub const NONE: u8 = 255;

    /// The analyser's own names, as `features.descriptors_json` holds them.
    pub fn parse(tonic: &str, scale: Option<&str>) -> Option<Key> {
        const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
        let pitch = NAMES.iter().position(|n| *n == tonic)? as u8;
        Some(Key { pitch, minor: scale != Some("major") })
    }

    pub fn encode(key: Option<Key>) -> u8 {
        key.map_or(Self::NONE, |k| k.pitch + if k.minor { 12 } else { 0 })
    }

    pub fn decode(byte: u8) -> Option<Key> {
        (byte < 24).then_some(Key { pitch: byte % 12, minor: byte >= 12 })
    }

    /// Its place on the Camelot wheel: 1 to 12, B for major, A for minor.
    /// C major is 8B and A minor 8A; each step round is a fifth.
    pub fn camelot(self) -> (u8, char) {
        // Sevens of semitones walk the circle of fifths; C sits at 8.
        let from_c = (self.pitch as u32 * 7) % 12;
        let major_number = (from_c + 7) % 12 + 1;
        if self.minor {
            // A minor shares 8 with C major: its relative major is 3 up.
            let relative = Key { pitch: (self.pitch + 3) % 12, minor: false };
            (relative.camelot().0, 'A')
        } else {
            (major_number as u8, 'B')
        }
    }

    pub fn name(self) -> String {
        const NAMES: [&str; 12] = ["C", "C♯", "D", "E♭", "E", "F", "F♯", "G", "A♭", "A", "B♭", "B"];
        format!("{} {}", NAMES[self.pitch as usize], if self.minor { "minor" } else { "major" })
    }
}

/// Every track, column by column.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vectors {
    pub head: VectorsHead,
    pub ids: Vec<i64>,
    /// NaN for a track the layout has not placed.
    pub xy: Vec<[f32; 2]>,
    pub genre: Vec<u16>,
    pub artist: Vec<i64>,
    /// `n · dims`, row after row, see `quantise`.
    pub rows: Vec<i8>,
    /// NaN where the analyser found none.
    pub bpm: Vec<f32>,
    pub key: Vec<u8>,
    pub key_strength: Vec<u8>,
    pub titles: Vec<String>,
    pub artists: Vec<String>,
    pub albums: Vec<String>,
}

pub fn quantise(value: f32) -> i8 {
    (value.clamp(-1.0, 1.0) * 127.0).round() as i8
}

pub fn unquantise(value: i8) -> f32 {
    value as f32 / 127.0
}

impl Vectors {
    pub fn row(&self, i: usize) -> &[i8] {
        let d = self.head.dims;
        &self.rows[i * d..(i + 1) * d]
    }

    pub fn key(&self, i: usize) -> Option<Key> {
        Key::decode(self.key[i])
    }

    pub fn encode(&self) -> Vec<u8> {
        let n = self.head.n;
        let head = serde_json::to_vec(&self.head).unwrap_or_default();
        let mut out = Vec::with_capacity(8 + head.len() + n * (34 + self.head.dims + 48));
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&(head.len() as u32).to_le_bytes());
        out.extend_from_slice(&head);
        self.ids.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        self.xy.iter().flatten().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        self.genre.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        self.artist.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        out.extend(self.rows.iter().map(|v| *v as u8));
        self.bpm.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        out.extend_from_slice(&self.key);
        out.extend_from_slice(&self.key_strength);
        let clean = |text: &str| text.replace(['\n', FIELD], " ");
        for i in 0..n {
            if i > 0 {
                out.push(b'\n');
            }
            let line = format!("{}{FIELD}{}{FIELD}{}", clean(&self.titles[i]), clean(&self.artists[i]), clean(&self.albums[i]));
            out.extend_from_slice(line.as_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Vectors> {
        ensure!(bytes.starts_with(&MAGIC), "not a vectors body");
        let mut at = 4;
        let mut take = |len: usize| -> Result<&[u8]> {
            let slice = bytes.get(at..at + len).context("the vectors body ends early")?;
            at += len;
            Ok(slice)
        };
        let head_len = u32::from_le_bytes(take(4)?.try_into()?) as usize;
        let head: VectorsHead = serde_json::from_slice(take(head_len)?).context("reading the vectors head")?;
        let (n, d) = (head.n, head.dims);
        ensure!(head.blocks.iter().all(|b| b.start <= b.end && b.end <= d), "a block reaches past the row");

        let ids = numbers(take(8 * n)?, i64::from_le_bytes);
        let flat: Vec<f32> = numbers(take(8 * n)?, f32::from_le_bytes);
        let xy = flat.as_chunks::<2>().0.to_vec();
        let genre = numbers(take(2 * n)?, u16::from_le_bytes);
        let artist = numbers(take(8 * n)?, i64::from_le_bytes);
        let rows = take(n * d)?.iter().map(|v| *v as i8).collect();
        let bpm = numbers(take(4 * n)?, f32::from_le_bytes);
        let key = take(n)?.to_vec();
        let key_strength = take(n)?.to_vec();
        let text = std::str::from_utf8(&bytes[at..]).context("the names are not UTF-8")?;

        let (mut titles, mut artists, mut albums) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        if n > 0 {
            for line in text.split('\n') {
                let mut fields = line.split(FIELD);
                titles.push(fields.next().unwrap_or_default().to_string());
                artists.push(fields.next().unwrap_or_default().to_string());
                albums.push(fields.next().unwrap_or_default().to_string());
            }
        }
        if titles.len() != n {
            bail!("{} names for {n} tracks", titles.len());
        }
        Ok(Vectors { head, ids, xy, genre, artist, rows, bpm, key, key_strength, titles, artists, albums })
    }
}

fn numbers<const N: usize, T>(bytes: &[u8], read: fn([u8; N]) -> T) -> Vec<T> {
    bytes.as_chunks::<N>().0.iter().map(|chunk| read(*chunk)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two() -> Vectors {
        Vectors {
            head: VectorsHead {
                stamp: "s".into(),
                n: 2,
                dims: 3,
                blocks: vec![VectorBlock { name: "tempo".into(), start: 0, end: 3, columns: vec!["a".into(), "b".into(), "c".into()], default_weight: 1.0 }],
                genres: vec!["Techno".into(), "Jazz".into()],
            },
            ids: vec![1, 1 << 40],
            xy: vec![[0.5, -2.0], [f32::NAN, f32::NAN]],
            genre: vec![1, 0],
            artist: vec![7, 8],
            rows: vec![quantise(1.0), quantise(-0.5), 0, 3, -127, 127],
            bpm: vec![128.0, f32::NAN],
            key: vec![Key::encode(Key::parse("A", Some("minor"))), Key::NONE],
            key_strength: vec![200, 0],
            titles: vec!["Low\nTide".into(), "".into()],
            artists: vec!["Nova".into(), "Wren".into()],
            albums: vec!["Fields\u{1f}".into(), "Glass".into()],
        }
    }

    #[test]
    fn round_trips() {
        let v = two();
        let back = Vectors::decode(&v.encode()).unwrap();
        assert_eq!(back.head, v.head);
        assert_eq!(back.ids, v.ids);
        assert_eq!(back.xy[0], v.xy[0]);
        assert!(back.xy[1][0].is_nan() && back.bpm[1].is_nan());
        assert_eq!((&back.genre, &back.artist, &back.rows), (&v.genre, &v.artist, &v.rows));
        assert_eq!((&back.key, &back.key_strength), (&v.key, &v.key_strength));
        // Separators inside a name become spaces rather than shift the fields.
        assert_eq!(back.titles, ["Low Tide", ""]);
        assert_eq!(back.albums, ["Fields ", "Glass"]);
        assert_eq!(back.row(1), [3, -127, 127]);
    }

    #[test]
    fn a_short_body_is_an_error() {
        let bytes = two().encode();
        assert!(Vectors::decode(&bytes[..bytes.len() / 2]).is_err());
        assert!(Vectors::decode(b"{}").is_err());
    }

    #[test]
    fn camelot_numbers_follow_the_wheel() {
        let at = |tonic: &str, scale: &str| Key::parse(tonic, Some(scale)).unwrap().camelot();
        assert_eq!(at("C", "major"), (8, 'B'));
        assert_eq!(at("G", "major"), (9, 'B'));
        assert_eq!(at("F", "major"), (7, 'B'));
        assert_eq!(at("B", "major"), (1, 'B'));
        assert_eq!(at("A", "minor"), (8, 'A'));
        assert_eq!(at("E", "minor"), (9, 'A'));
        assert_eq!(at("G#", "minor"), (1, 'A'));
        assert_eq!(Key::parse("H", None), None);
    }

    #[test]
    fn quantising_stays_within_a_step() {
        for v in [-1.0, -0.33, 0.0, 0.004, 0.71, 1.0] {
            assert!((unquantise(quantise(v)) - v).abs() <= 0.5 / 127.0 + 1e-6);
        }
        assert_eq!(quantise(3.0), 127);
    }
}
