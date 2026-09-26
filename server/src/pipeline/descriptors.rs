//! The descriptors CLAP does not give: tempo, key, loudness, spectral shape.
//!
//! Plain signal processing over the same 48kHz excerpt CLAP reads. None of it
//! tries to reproduce a particular library's numbers, the space z-scores
//! every column, so what matters is that a descriptor orders tracks sensibly
//! and consistently, not its absolute scale.
//!
//! BPM in particular is allowed to be off by an octave: the space folds tempo
//! into one octave before using it, so 86 and 172 land in the same place.

use anyhow::{Context, Result};
use realfft::RealFftPlanner;
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Descriptors {
    pub bpm: Option<f64>,
    /// Detected onsets per second.
    pub onset_rate: Option<f64>,
    /// Pitch class with sharps, "C" .. "B".
    pub key: Option<String>,
    /// "major" or "minor".
    pub scale: Option<String>,
    /// Correlation of the chroma with the winning key profile, 0..1.
    pub key_strength: Option<f64>,
    /// EBU R128, LUFS.
    pub loudness_integrated: Option<f64>,
    /// EBU R128 loudness range, LU.
    pub loudness_range: Option<f64>,
    /// Mean absolute deviation of 400ms loudness from its mean, dB. How much
    /// the level moves, where the range is about its extremes.
    pub dynamic_complexity: Option<f64>,
    /// Hz, mean over frames.
    pub spectral_centroid: Option<f64>,
    /// Hz below which 85% of a frame's energy sits, mean over frames.
    pub spectral_rolloff: Option<f64>,
    /// Geometric over arithmetic mean power, in dB: near 0 for noise, very
    /// negative for tones.
    pub spectral_flatness: Option<f64>,
    /// Sign changes per sample.
    pub zero_crossing_rate: Option<f64>,
}

const N_FFT: usize = 2048;
/// 10ms at 48kHz: fine enough for onsets, and a round number of frames/s.
const HOP: usize = 480;
const ROLLOFF: f64 = 0.85;
const ONSET_BANDS: usize = 40;

/// Everything, from mono audio at `rate`.
pub fn extract(audio: &[f32], rate: u32) -> Result<Descriptors> {
    anyhow::ensure!(audio.len() > N_FFT * 4, "too little audio for descriptors");

    let (loudness_integrated, loudness_range) = loudness(audio, rate)?;
    let spectral = spectral(audio, rate);
    let envelope = onset_envelope(&spectral.band_energy, spectral.frames);
    let fps = rate as f64 / HOP as f64;
    let (key, scale, key_strength) = match key(audio, rate) {
        Some((key, scale, strength)) => (Some(key), Some(scale), Some(strength)),
        None => (None, None, None),
    };

    Ok(Descriptors {
        bpm: tempo(&envelope, fps),
        onset_rate: Some(onset_rate(&envelope, fps)),
        key,
        scale,
        key_strength,
        loudness_integrated,
        loudness_range,
        dynamic_complexity: dynamic_complexity(audio, rate),
        spectral_centroid: finite(spectral.centroid),
        spectral_rolloff: finite(spectral.rolloff),
        spectral_flatness: finite(spectral.flatness_db),
        zero_crossing_rate: finite(zero_crossing_rate(audio)),
    })
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

// ----------------------------------------------------------------- loudness

fn loudness(audio: &[f32], rate: u32) -> Result<(Option<f64>, Option<f64>)> {
    use ebur128::{EbuR128, Mode};
    let mut meter = EbuR128::new(1, rate, Mode::I | Mode::LRA).context("loudness meter")?;
    meter.add_frames_f32(audio).context("loudness meter")?;
    // Silence gates everything out and reports -inf; that is "unknown" here.
    Ok((
        meter.loudness_global().ok().and_then(finite),
        meter.loudness_range().ok().and_then(finite),
    ))
}

fn dynamic_complexity(audio: &[f32], rate: u32) -> Option<f64> {
    let block = (rate as usize * 2) / 5; // 400ms
    let levels: Vec<f64> = audio
        .chunks_exact(block)
        .map(|b| {
            let power = b.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / block as f64;
            10.0 * power.max(1e-12).log10()
        })
        // Gate out near-silence, as loudness measures do: a gap between
        // movements says nothing about how dynamic the music is.
        .filter(|&db| db > -70.0)
        .collect();
    if levels.len() < 2 {
        return None;
    }
    let mean = levels.iter().sum::<f64>() / levels.len() as f64;
    Some(levels.iter().map(|l| (l - mean).abs()).sum::<f64>() / levels.len() as f64)
}

fn zero_crossing_rate(audio: &[f32]) -> f64 {
    let crossings = audio
        .windows(2)
        .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
        .count();
    crossings as f64 / audio.len().max(1) as f64
}

// ----------------------------------------------------------------- spectrum

struct Spectral {
    centroid: f64,
    rolloff: f64,
    flatness_db: f64,
    /// [frames x ONSET_BANDS] log-compressed band energies, for onsets.
    band_energy: Vec<f64>,
    frames: usize,
}

fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos())
        .collect()
}

/// Band edges spaced evenly on a log-frequency axis from 30Hz to 16kHz, as
/// bin indices. Close enough to mel for onset detection.
fn log_bands(rate: u32, n_fft: usize, bands: usize) -> Vec<usize> {
    let (low, high) = (30f64.ln(), 16_000f64.min(rate as f64 / 2.0).ln());
    (0..=bands)
        .map(|i| {
            let hz = (low + (high - low) * i as f64 / bands as f64).exp();
            ((hz * n_fft as f64 / rate as f64).round() as usize).min(n_fft / 2)
        })
        .collect()
}

fn spectral(audio: &[f32], rate: u32) -> Spectral {
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(N_FFT);
    let window = hann(N_FFT);
    let mut frame = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();
    let bin_hz = rate as f64 / N_FFT as f64;
    let edges = log_bands(rate, N_FFT, ONSET_BANDS);

    let frames = (audio.len() - N_FFT) / HOP + 1;
    let mut band_energy = Vec::with_capacity(frames * ONSET_BANDS);
    let (mut centroid_sum, mut rolloff_sum, mut flatness_sum, mut voiced) = (0.0, 0.0, 0.0, 0usize);
    let mut power = vec![0f64; N_FFT / 2 + 1];

    for t in 0..frames {
        let start = t * HOP;
        for (i, slot) in frame.iter_mut().enumerate() {
            *slot = audio[start + i] as f64 * window[i];
        }
        fft.process_with_scratch(&mut frame, &mut spectrum, &mut scratch)
            .expect("buffers come from the plan");
        for (p, c) in power.iter_mut().zip(&spectrum) {
            *p = c.norm_sqr();
        }

        for band in 0..ONSET_BANDS {
            let (lo, hi) = (edges[band], edges[band + 1].max(edges[band] + 1));
            let energy: f64 = power[lo..hi.min(power.len())].iter().sum();
            band_energy.push((1.0 + 1000.0 * energy / N_FFT as f64).ln());
        }

        // Skip DC; ignore silent frames, whose shape is meaningless.
        let total: f64 = power[1..].iter().sum();
        if total < 1e-9 {
            continue;
        }
        voiced += 1;
        centroid_sum += power[1..]
            .iter()
            .enumerate()
            .map(|(i, p)| (i + 1) as f64 * bin_hz * p)
            .sum::<f64>()
            / total;

        let mut running = 0.0;
        let mut rolloff_bin = power.len() - 1;
        for (i, p) in power.iter().enumerate().skip(1) {
            running += p;
            if running >= ROLLOFF * total {
                rolloff_bin = i;
                break;
            }
        }
        rolloff_sum += rolloff_bin as f64 * bin_hz;

        let n = (power.len() - 1) as f64;
        let log_mean = power[1..].iter().map(|p| (p + 1e-12).ln()).sum::<f64>() / n;
        let mean = total / n;
        flatness_sum += 10.0 * (log_mean.exp() / mean).max(1e-12).log10();
    }

    let per = |sum: f64| if voiced > 0 { sum / voiced as f64 } else { f64::NAN };
    Spectral {
        centroid: per(centroid_sum),
        rolloff: per(rolloff_sum),
        flatness_db: per(flatness_sum),
        band_energy,
        frames,
    }
}

// ------------------------------------------------------------------- rhythm

/// Spectral flux: how much each band's log energy rose since the last frame,
/// summed, with the local average taken off so a crescendo is not an onset.
fn onset_envelope(bands: &[f64], frames: usize) -> Vec<f64> {
    let mut flux = vec![0f64; frames];
    for t in 1..frames {
        let (now, before) = (
            &bands[t * ONSET_BANDS..(t + 1) * ONSET_BANDS],
            &bands[(t - 1) * ONSET_BANDS..t * ONSET_BANDS],
        );
        flux[t] = now
            .iter()
            .zip(before)
            .map(|(a, b)| (a - b).max(0.0))
            .sum();
    }

    // Subtract a centred moving average over ~0.5s, and rectify.
    let half = 25;
    let mut prefix = vec![0f64; frames + 1];
    for (i, v) in flux.iter().enumerate() {
        prefix[i + 1] = prefix[i] + v;
    }
    (0..frames)
        .map(|t| {
            let (lo, hi) = (t.saturating_sub(half), (t + half + 1).min(frames));
            let local = (prefix[hi] - prefix[lo]) / (hi - lo) as f64;
            (flux[t] - local).max(0.0)
        })
        .collect()
}

/// Peaks of the envelope that stand clear of it, at least 50ms apart.
fn onset_rate(envelope: &[f64], fps: f64) -> f64 {
    let n = envelope.len();
    if n < 3 {
        return 0.0;
    }
    let mean = envelope.iter().sum::<f64>() / n as f64;
    let std = (envelope.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let threshold = mean + std;
    let neighbourhood = 3;
    let min_gap = (0.05 * fps).ceil() as usize;

    let mut onsets = 0;
    let mut last: Option<usize> = None;
    for t in 0..n {
        let v = envelope[t];
        if v <= threshold {
            continue;
        }
        let (lo, hi) = (t.saturating_sub(neighbourhood), (t + neighbourhood + 1).min(n));
        if envelope[lo..hi].iter().any(|&u| u > v) {
            continue;
        }
        if last.is_some_and(|l| t - l < min_gap) {
            continue;
        }
        onsets += 1;
        last = Some(t);
    }
    onsets as f64 / (n as f64 / fps)
}

/// Tempo from the autocorrelation of the onset envelope, weighted towards
/// 120 BPM so that, of two octave-related candidates, the likelier one wins.
fn tempo(envelope: &[f64], fps: f64) -> Option<f64> {
    let n = envelope.len();
    let mean = envelope.iter().sum::<f64>() / n.max(1) as f64;
    let centred: Vec<f64> = envelope.iter().map(|v| v - mean).collect();
    let energy: f64 = centred.iter().map(|v| v * v).sum();
    if energy < 1e-12 {
        return None;
    }

    let (min_bpm, max_bpm) = (40.0, 220.0);
    let min_lag = (60.0 * fps / max_bpm).floor() as usize;
    let max_lag = ((60.0 * fps / min_bpm).ceil() as usize).min(n / 2);
    if max_lag <= min_lag + 2 {
        return None;
    }

    let autocorrelation: Vec<f64> = (0..=max_lag + 1)
        .map(|lag| {
            centred[..n - lag]
                .iter()
                .zip(&centred[lag..])
                .map(|(a, b)| a * b)
                .sum::<f64>()
                / energy
        })
        .collect();

    let prior = |lag: f64| {
        let bpm = 60.0 * fps / lag;
        (-0.5 * (bpm / 120.0).log2().powi(2)).exp()
    };
    let (best, score) = (min_lag..=max_lag)
        .map(|lag| (lag, autocorrelation[lag] * prior(lag as f64)))
        .fold((0, f64::MIN), |acc, x| if x.1 > acc.1 { x } else { acc });
    if score <= 0.0 {
        return None;
    }

    // Parabolic interpolation around the peak, for a sub-frame lag.
    let (a, b, c) = (
        autocorrelation[best - 1],
        autocorrelation[best],
        autocorrelation[best + 1],
    );
    let denominator = a - 2.0 * b + c;
    let shift = if denominator.abs() > 1e-12 {
        (0.5 * (a - c) / denominator).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    Some(60.0 * fps / (best as f64 + shift))
}

// ---------------------------------------------------------------------- key

const PITCH_CLASSES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
/// Krumhansl-Kessler probe-tone profiles, tonic first.
const MAJOR: [f64; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const MINOR: [f64; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

/// The best-correlating of the 24 major and minor keys, with its correlation.
fn key(audio: &[f32], rate: u32) -> Option<(String, String, f64)> {
    let chroma = chroma(audio, rate)?;
    let mut best = (0usize, "major", f64::MIN);
    for tonic in 0..12 {
        for (scale, profile) in [("major", &MAJOR), ("minor", &MINOR)] {
            let rotated: Vec<f64> = (0..12).map(|pc| profile[(pc + 12 - tonic) % 12]).collect();
            let r = pearson(&chroma, &rotated);
            if r > best.2 {
                best = (tonic, scale, r);
            }
        }
    }
    Some((
        PITCH_CLASSES[best.0].to_string(),
        best.1.to_string(),
        best.2.clamp(0.0, 1.0),
    ))
}

/// Mean chroma over the excerpt, from a long FFT: at 48kHz, 8192 points gives
/// 5.9Hz bins, enough to tell semitones apart down to about 100Hz.
fn chroma(audio: &[f32], rate: u32) -> Option<[f64; 12]> {
    const N: usize = 8192;
    const STEP: usize = 4096;
    if audio.len() < N {
        return None;
    }
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(N);
    let window = hann(N);
    let mut frame = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();

    // Pitch class per bin, for bins between 100Hz and 5kHz.
    let bin_hz = rate as f64 / N as f64;
    let classes: Vec<Option<usize>> = (0..=N / 2)
        .map(|b| {
            let hz = b as f64 * bin_hz;
            (100.0..5000.0).contains(&hz).then(|| {
                let midi = 69.0 + 12.0 * (hz / 440.0).log2();
                (midi.round() as i64).rem_euclid(12) as usize
            })
        })
        .collect();

    let mut total = [0f64; 12];
    let mut frames = 0;
    for start in (0..=audio.len() - N).step_by(STEP) {
        for (i, slot) in frame.iter_mut().enumerate() {
            *slot = audio[start + i] as f64 * window[i];
        }
        fft.process_with_scratch(&mut frame, &mut spectrum, &mut scratch)
            .expect("buffers come from the plan");
        let mut bins = [0f64; 12];
        for (c, class) in spectrum.iter().zip(&classes) {
            if let Some(pc) = class {
                bins[*pc] += c.norm();
            }
        }
        // Each frame counts equally, so loud passages do not decide the key.
        let peak = bins.iter().cloned().fold(0.0, f64::max);
        if peak > 1e-9 {
            for (t, b) in total.iter_mut().zip(bins) {
                *t += b / peak;
            }
            frames += 1;
        }
    }
    (frames > 0).then_some(total)
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        num += (x - ma) * (y - mb);
        da += (x - ma).powi(2);
        db += (y - mb).powi(2);
    }
    if da < 1e-12 || db < 1e-12 {
        0.0
    } else {
        num / (da * db).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sine(hz: f64, amplitude: f64, seconds: f64) -> Vec<f32> {
        (0..(seconds * RATE as f64) as usize)
            .map(|i| (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / RATE as f64).sin()) as f32)
            .collect()
    }

    fn fold(bpm: f64) -> f64 {
        let mut v = bpm;
        while v < 70.0 {
            v *= 2.0;
        }
        while v >= 140.0 {
            v /= 2.0;
        }
        v
    }

    #[test]
    fn loudness_of_a_quiet_sine() {
        // A full-scale 1kHz sine is -3.01 LUFS; a tenth of it is 20dB lower.
        let d = extract(&sine(1000.0, 0.1, 20.0), RATE).unwrap();
        let lufs = d.loudness_integrated.unwrap();
        assert!((lufs + 23.0).abs() < 0.3, "{lufs}");
        assert!(d.dynamic_complexity.unwrap() < 0.5);
        // A pure tone is anything but flat, and its centroid sits on it.
        assert!(d.spectral_flatness.unwrap() < -20.0);
        assert!((d.spectral_centroid.unwrap() - 1000.0).abs() < 60.0);
    }

    #[test]
    fn tempo_of_a_click_track() {
        for bpm in [90.0, 128.0, 174.0] {
            let mut audio = vec![0f32; RATE as usize * 30];
            let period = 60.0 / bpm;
            let mut k = 0;
            while ((k as f64 * period) * RATE as f64) < (audio.len() - 2000) as f64 {
                let at = (k as f64 * period * RATE as f64) as usize;
                for i in 0..960 {
                    let decay = (-(i as f64) / 150.0).exp();
                    audio[at + i] += (decay * (2.0 * std::f64::consts::PI * 60.0 * i as f64 / RATE as f64).sin()) as f32 * 0.8;
                    audio[at + i] += (decay * ((i * 7919 % 13) as f64 / 13.0 - 0.5)) as f32 * 0.4;
                }
                k += 1;
            }
            let d = extract(&audio, RATE).unwrap();
            let found = d.bpm.unwrap();
            assert!((fold(found) - fold(bpm)).abs() < 2.0, "{bpm} detected as {found}");
            let rate = d.onset_rate.unwrap();
            assert!((rate - bpm / 60.0).abs() < 0.5, "{bpm}: {rate} onsets/s");
        }
    }

    #[test]
    fn key_of_a_triad() {
        // A minor: A, C, E.
        let mut audio = vec![0f32; RATE as usize * 10];
        for hz in [220.0, 261.63, 329.63, 440.0] {
            for (a, s) in audio.iter_mut().zip(sine(hz, 0.2, 10.0)) {
                *a += s;
            }
        }
        let d = extract(&audio, RATE).unwrap();
        assert_eq!((d.key.as_deref(), d.scale.as_deref()), (Some("A"), Some("minor")));

        // G major: G, B, D.
        let mut audio = vec![0f32; RATE as usize * 10];
        for hz in [196.0, 246.94, 293.66, 392.0] {
            for (a, s) in audio.iter_mut().zip(sine(hz, 0.2, 10.0)) {
                *a += s;
            }
        }
        let d = extract(&audio, RATE).unwrap();
        assert_eq!((d.key.as_deref(), d.scale.as_deref()), (Some("G"), Some("major")));
    }
}
