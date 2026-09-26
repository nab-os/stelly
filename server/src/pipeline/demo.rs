//! A synthetic corpus, for trying the app without a Qobuz crawl.
//!
//! Eight fake albums with deliberately distinct character, beatless drones,
//! 174 BPM breaks, a bright pop pulse, run through the real analyser, the
//! real space and the real layout. Nothing here touches the network except
//! fetching model weights, so it doubles as the end-to-end smoke test.

use super::audio::Pcm;
use super::clap::AudioEncoder;
use super::{analyse, assemble, layout, models, Job, Paths};
use crate::{crawl, db};
use anyhow::Result;
use serde_json::json;

const RATE: u32 = 44_100;
const SECONDS: usize = 25;
pub const TRACKS_PER_ALBUM: usize = 4;

/// Name, BPM (0 = beatless), base frequency, noise level, harmonics.
pub const ALBUMS: [(&str, u32, f64, f64, usize); 8] = [
    ("Ambient Drift", 0, 110.0, 0.01, 2),
    ("Techno Nights", 140, 55.0, 0.12, 3),
    ("Rock Garage", 110, 98.0, 0.30, 6),
    ("Jazz Corner", 92, 147.0, 0.04, 8),
    ("Drum and Bass", 174, 45.0, 0.18, 4),
    ("Downtempo Haze", 75, 82.0, 0.05, 3),
    ("Bright Pop", 128, 220.0, 0.08, 5),
    ("Dark Drone", 0, 38.0, 0.02, 2),
];

/// Deterministic noise, so the demo is the same every time.
struct Noise(u64);

impl Noise {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Roughly normal, by summing uniforms.
    fn gaussian(&mut self) -> f64 {
        (0..6).map(|_| self.unit()).sum::<f64>() - 3.0
    }
}

pub fn synth(bpm: u32, base: f64, noise: f64, harmonics: usize, seed: u64) -> Pcm {
    let mut rng = Noise(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let n = RATE as usize * SECONDS;
    let tau = 2.0 * std::f64::consts::PI;
    let mut audio = vec![0f64; n];

    // A harmonic stack, slightly detuned per track so albums vary inside.
    let detune = 1.0 + (rng.unit() - 0.5) * 0.06;
    for h in 1..=harmonics {
        let phase = rng.unit() * tau;
        let freq = base * detune * h as f64;
        for (i, a) in audio.iter_mut().enumerate() {
            *a += (0.6 / h as f64) * (tau * freq * i as f64 / RATE as f64 + phase).sin();
        }
    }

    let wobble = 0.05 + rng.unit() * 0.15;
    for (i, a) in audio.iter_mut().enumerate() {
        *a *= 0.5 + 0.5 * (tau * wobble * i as f64 / RATE as f64).sin();
        *a += noise * rng.gaussian();
    }

    if bpm > 0 {
        let period = 60.0 / bpm as f64;
        let click = (0.04 * RATE as f64) as usize;
        let mut k = 0;
        loop {
            let at = (k as f64 * period * RATE as f64) as usize;
            if at + click >= n {
                break;
            }
            for i in 0..click {
                let envelope = (-14.0 * i as f64 / click as f64).exp();
                audio[at + i] += 1.4 * envelope * (tau * 55.0 * i as f64 / RATE as f64).sin();
            }
            k += 1;
        }
    }

    let peak = audio.iter().fold(0f64, |m, v| m.max(v.abs())).max(1e-9);
    Pcm {
        samples: audio.iter().map(|v| (v / peak * 0.9) as f32).collect(),
        rate: RATE,
    }
}

/// (track id, album index), in the order they were written.
pub fn populate(conn: &mut diesel::SqliteConnection) -> Result<Vec<(i64, usize)>> {
    let mut tracks = Vec::new();
    let mut track_id = 1000;
    for (index, (title, ..)) in ALBUMS.iter().enumerate() {
        let artist = json!({"id": 100 + index, "name": format!("Artist {index}")});
        let album_id = format!("album-{index}");
        crawl::upsert_album(
            conn,
            &json!({
                "id": album_id,
                "title": title,
                "artist": artist,
                "release_date_original": format!("{}-01-01", 1990 + index * 4),
                "genre": {"name": title.split(' ').next()},
            }),
        )?;
        for n in 0..TRACKS_PER_ALBUM {
            track_id += 1;
            crawl::upsert_track(
                conn,
                &json!({"id": track_id, "title": format!("{title} {}", n + 1), "duration": SECONDS}),
                Some(&album_id),
                Some(100 + index as i64),
                0,
            )?;
            tracks.push((track_id, index));
        }
    }
    Ok(tracks)
}

/// Build the whole demo under `root`: database, space and layout.
pub async fn build(root: &std::path::Path, job: &Job) -> Result<Paths> {
    let paths = Paths::under(root);
    let mut conn = db::open_for_write(&paths.db_path)?;

    job.log(format!(
        "synthesising {} tracks…",
        ALBUMS.len() * TRACKS_PER_ALBUM
    ));
    let tracks = populate(&mut conn)?;

    models::ensure(&paths.model_dir, &[models::CLAP_AUDIO], job).await?;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut encoder = AudioEncoder::load(&paths.model_dir, threads)?;

    job.log("extracting features…");
    for (n, &(track_id, album)) in tracks.iter().enumerate() {
        let (_, bpm, base, noise, harmonics) = ALBUMS[album];
        let pcm = synth(bpm, base, noise, harmonics, track_id as u64);
        let (descriptors, embedding) = analyse::analyse_pcm(&mut encoder, &pcm)?;
        analyse::store(&mut conn, track_id, &descriptors, &embedding)?;
        if (n + 1) % 8 == 0 {
            job.log(format!("  {}/{}", n + 1, tracks.len()));
        }
    }
    drop(conn);

    job.log("building space…");
    assemble::run(&paths, None, job).await?;
    job.log("laying out…");
    layout::run(&paths, &layout::Options::default(), job)?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole pipeline on the synthetic corpus: do tracks from one album
    /// land near each other, and does tempo come out where it was put?
    ///
    /// Needs the CLAP weights (fetched on first run, ~620MB), so it is not
    /// part of the default run: `cargo test -- --ignored smoke`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn smoke() {
        let root = std::env::temp_dir().join(format!("two-khz-smoke-{}", std::process::id()));
        let paths = build(&root, &Job::stderr()).await.unwrap();

        use crate::schema::{features, layout};
        use diesel::prelude::*;

        let conn = &mut db::open_for_write(&paths.db_path).unwrap();
        let mut bpm_of = |track: i64| -> Option<f64> {
            let json: Option<String> = features::table
                .find(track)
                .select(features::descriptors_json)
                .first(conn)
                .unwrap();
            serde_json::from_str::<serde_json::Value>(&json.unwrap()).unwrap()["bpm"].as_f64()
        };
        // Drum and Bass is album 4: tracks 1017..1020, at 174 BPM (or 87).
        let bpm = assemble::fold_bpm(bpm_of(1017)).unwrap().exp2();
        assert!((bpm - 87.0).abs() < 3.0, "174 BPM folded to {bpm}");

        let space = two_khz::space::Space::load(&paths.data_dir).unwrap();
        let weighted = space.weighted(&space.default_weights());
        let album_of = |row: usize| (space.manifest.track_ids[row] - 1001) as usize / TRACKS_PER_ALBUM;
        let mut top1 = 0;
        for row in 0..weighted.n_tracks {
            let mut scores = weighted.similarities(weighted.row(row));
            scores[row] = f32::NEG_INFINITY;
            let nearest = two_khz::space::top_k(&scores, 1)[0];
            if album_of(nearest) == album_of(row) {
                top1 += 1;
            }
        }
        let share = top1 as f64 / weighted.n_tracks as f64;
        eprintln!("same-album top-1: {top1}/{} ({:.0}%)", weighted.n_tracks, share * 100.0);
        assert!(share > 0.75);

        let laid_out: i64 = layout::table.count().get_result(conn).unwrap();
        assert_eq!(laid_out as usize, weighted.n_tracks);
        let _ = std::fs::remove_dir_all(&root);
    }
}
