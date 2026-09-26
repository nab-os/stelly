//! CLAP's audio tower: the semantic block of the space, and, through the
//! text tower it shares a space with, every mood and style score.
//!
//! The model is `laion/clap-htsat-unfused`, as exported to ONNX by Xenova. The
//! unfused general checkpoint on purpose: `larger_clap_music` has a text tower
//! whose embeddings are near-constant whatever the input, which would silently
//! flatten text steering and every zero-shot label while still "working".
//!
//! The front end below is a port of transformers' `ClapFeatureExtractor` for
//! that checkpoint's config, checked against it in the tests: slaney mel
//! filters, a reflect-padded periodic Hann STFT, power in dB.

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::TensorRef;
use realfft::{RealFftPlanner, RealToComplex};
use std::path::Path;
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 48_000;
/// The audio branch consumes 10s windows. A 90s excerpt is cut into nine and
/// the embeddings mean-pooled, so the result reflects the whole excerpt.
pub const WINDOW_SAMPLES: usize = 10 * SAMPLE_RATE as usize;
pub const EMBEDDING_DIMS: usize = 512;

const N_FFT: usize = 1024;
const HOP: usize = 480;
const N_MELS: usize = 64;
const F_MIN: f64 = 50.0;
const F_MAX: f64 = 14_000.0;
const N_BINS: usize = N_FFT / 2 + 1;
/// Frames per window: centred STFT, so one more than samples / hop.
pub const N_FRAMES: usize = WINDOW_SAMPLES / HOP + 1;

// ---------------------------------------------------------------- front end

/// Log-mel spectrogram, exactly as the model was trained to read it.
pub struct MelFrontEnd {
    /// [N_BINS x N_MELS], slaney-scaled and slaney-normalised triangles.
    filters: Vec<f64>,
    window: Vec<f64>,
    fft: Arc<dyn RealToComplex<f64>>,
}

impl Default for MelFrontEnd {
    fn default() -> Self {
        Self::new()
    }
}

impl MelFrontEnd {
    pub fn new() -> Self {
        // Periodic Hann: np.hanning(N + 1)[:-1].
        let window = (0..N_FFT)
            .map(|n| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64).cos())
            .collect();
        Self {
            filters: slaney_filters(),
            window,
            fft: RealFftPlanner::<f64>::new().plan_fft_forward(N_FFT),
        }
    }

    /// [N_FRAMES x N_MELS], row-major, for exactly one window of audio.
    pub fn log_mel(&self, window_samples: &[f32]) -> Vec<f32> {
        debug_assert_eq!(window_samples.len(), WINDOW_SAMPLES);
        let pad = N_FFT / 2;
        let n = window_samples.len();

        // Reflect padding, numpy's flavour: the edge sample is not repeated.
        let mut padded = Vec::with_capacity(n + 2 * pad);
        padded.extend((1..=pad).rev().map(|i| window_samples[i] as f64));
        padded.extend(window_samples.iter().map(|&v| v as f64));
        padded.extend((0..pad).map(|i| window_samples[n - 2 - i] as f64));

        let mut frame = self.fft.make_input_vec();
        let mut spectrum = self.fft.make_output_vec();
        let mut scratch = self.fft.make_scratch_vec();
        let mut power = vec![0f64; N_BINS];
        let mut out = Vec::with_capacity(N_FRAMES * N_MELS);

        for t in 0..N_FRAMES {
            let start = t * HOP;
            for (i, slot) in frame.iter_mut().enumerate() {
                *slot = padded[start + i] * self.window[i];
            }
            self.fft
                .process_with_scratch(&mut frame, &mut spectrum, &mut scratch)
                .expect("buffers come from the plan");
            for (p, c) in power.iter_mut().zip(&spectrum) {
                *p = c.norm_sqr();
            }
            for m in 0..N_MELS {
                let mut energy = 0f64;
                for (b, p) in power.iter().enumerate() {
                    energy += self.filters[b * N_MELS + m] * p;
                }
                out.push((10.0 * energy.max(1e-10).log10()) as f32);
            }
        }
        out
    }
}

fn hz_to_mel(hz: f64) -> f64 {
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = 15.0;
    if hz >= MIN_LOG_HZ {
        MIN_LOG_MEL + (hz / MIN_LOG_HZ).ln() * (27.0 / 6.4f64.ln())
    } else {
        3.0 * hz / 200.0
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = 15.0;
    if mel >= MIN_LOG_MEL {
        MIN_LOG_HZ * ((6.4f64.ln() / 27.0) * (mel - MIN_LOG_MEL)).exp()
    } else {
        200.0 * mel / 3.0
    }
}

/// transformers' `mel_filter_bank(..., norm="slaney", mel_scale="slaney")`.
fn slaney_filters() -> Vec<f64> {
    let (low, high) = (hz_to_mel(F_MIN), hz_to_mel(F_MAX));
    let edges: Vec<f64> = (0..N_MELS + 2)
        .map(|i| mel_to_hz(low + (high - low) * i as f64 / (N_MELS + 1) as f64))
        .collect();
    let nyquist = (SAMPLE_RATE / 2) as f64;

    let mut filters = vec![0f64; N_BINS * N_MELS];
    for b in 0..N_BINS {
        let freq = nyquist * b as f64 / (N_BINS - 1) as f64;
        for m in 0..N_MELS {
            let down = (freq - edges[m]) / (edges[m + 1] - edges[m]);
            let up = (edges[m + 2] - freq) / (edges[m + 2] - edges[m + 1]);
            let weight = down.min(up).max(0.0);
            let area = 2.0 / (edges[m + 2] - edges[m]);
            filters[b * N_MELS + m] = weight * area;
        }
    }
    filters
}

/// Cut an excerpt into the windows the model reads: whole 10s windows, a
/// trailing stub kept only if at least half a window, and the processor's
/// "repeatpad" for anything short.
pub fn windows(audio: &[f32]) -> Vec<Vec<f32>> {
    let mut chunks: Vec<&[f32]> = audio
        .chunks(WINDOW_SAMPLES)
        .filter(|c| c.len() >= WINDOW_SAMPLES / 2)
        .collect();
    if chunks.is_empty() && !audio.is_empty() {
        chunks.push(audio);
    }
    chunks.into_iter().map(repeat_pad).collect()
}

fn repeat_pad(chunk: &[f32]) -> Vec<f32> {
    if chunk.len() >= WINDOW_SAMPLES {
        return chunk[..WINDOW_SAMPLES].to_vec();
    }
    let repeats = WINDOW_SAMPLES / chunk.len();
    let mut out = Vec::with_capacity(WINDOW_SAMPLES);
    for _ in 0..repeats {
        out.extend_from_slice(chunk);
    }
    out.resize(WINDOW_SAMPLES, 0.0);
    out
}

// -------------------------------------------------------------------- model

/// The audio tower, one per worker thread: a session is run through `&mut`.
pub struct AudioEncoder {
    session: Session,
    front: MelFrontEnd,
}

impl AudioEncoder {
    pub fn load(model_dir: &Path, threads: usize) -> Result<Self> {
        let path = model_dir.join(super::models::CLAP_AUDIO);
        let session = Session::builder()
            .context("creating an ONNX session")?
            .with_intra_threads(threads.max(1))
            .map_err(|e| anyhow::anyhow!("setting ONNX threads: {e}"))?
            .commit_from_file(&path)
            .with_context(|| format!("loading {}", path.display()))?;
        Ok(Self {
            session,
            front: MelFrontEnd::new(),
        })
    }

    /// Mean-pooled, L2-normalised 512-d embedding of 48kHz mono audio.
    pub fn embed(&mut self, audio: &[f32]) -> Result<Vec<f32>> {
        let windows = windows(audio);
        anyhow::ensure!(!windows.is_empty(), "no audio to embed");

        let mut features = Vec::with_capacity(windows.len() * N_FRAMES * N_MELS);
        for window in &windows {
            features.extend(self.front.log_mel(window));
        }
        let shape = [windows.len() as i64, 1, N_FRAMES as i64, N_MELS as i64];

        let outputs = self.session.run(ort::inputs![
            "input_features" => TensorRef::from_array_view((shape, features.as_slice()))?,
        ])?;
        let (_, data) = outputs["audio_embeds"].try_extract_tensor::<f32>()?;

        // Normalise per window before averaging, so a loud one cannot dominate.
        let mut pooled = vec![0f32; EMBEDDING_DIMS];
        for row in data.chunks_exact(EMBEDDING_DIMS) {
            let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
            for (p, v) in pooled.iter_mut().zip(row) {
                *p += v / norm;
            }
        }
        normalise(&mut pooled);
        Ok(pooled)
    }
}

pub fn normalise(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
    for v in vector.iter_mut() {
        *v /= norm;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slaney_scale_round_trips() {
        for hz in [50.0, 440.0, 999.0, 1000.0, 5000.0, 14_000.0] {
            assert!((mel_to_hz(hz_to_mel(hz)) - hz).abs() < 1e-6);
        }
    }

    #[test]
    fn repeat_pad_tiles_then_zero_fills() {
        let chunk: Vec<f32> = (0..300_000).map(|i| i as f32).collect();
        let padded = repeat_pad(&chunk);
        assert_eq!(padded.len(), WINDOW_SAMPLES);
        assert_eq!(padded[299_999], 299_999.0);
        assert_eq!(padded[300_000], 0.0);
        assert_eq!(padded[479_999], 0.0);

        let short: Vec<f32> = vec![1.0; 100_000];
        let padded = repeat_pad(&short);
        assert_eq!(padded[399_999], 1.0);
        assert_eq!(padded[400_000], 0.0);
    }

    #[test]
    fn windows_drop_a_short_stub() {
        assert_eq!(windows(&vec![0.1; WINDOW_SAMPLES * 2 + 1000]).len(), 2);
        assert_eq!(windows(&vec![0.1; WINDOW_SAMPLES * 2 + WINDOW_SAMPLES / 2]).len(), 3);
        assert_eq!(windows(&vec![0.1; 1000]).len(), 1);
    }

    /// Compare against transformers' own extractor. Needs reference files
    /// written by the export check; run with
    /// `TWO_KHZ_CLAP_REFERENCE=<dir> cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn front_end_matches_transformers() {
        let dir = std::path::PathBuf::from(std::env::var("TWO_KHZ_CLAP_REFERENCE").unwrap());
        let audio: Vec<f32> = bytemuck::cast_slice(&std::fs::read(dir.join("audio_in.f32")).unwrap()).to_vec();
        let reference: Vec<f32> =
            bytemuck::cast_slice(&std::fs::read(dir.join("feat_ref.f32")).unwrap()).to_vec();

        let front = MelFrontEnd::new();
        let first = front.log_mel(&audio[..WINDOW_SAMPLES]);
        let second = front.log_mel(&repeat_pad(&audio[WINDOW_SAMPLES..]));
        let ours: Vec<f32> = first.into_iter().chain(second).collect();
        assert_eq!(ours.len(), reference.len());

        let worst = ours
            .iter()
            .zip(&reference)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1e-2, "max dB difference {worst}");

        let model_dir = std::env::var("TWO_KHZ_MODEL_DIR").map(std::path::PathBuf::from);
        if let Ok(model_dir) = model_dir {
            let mut encoder = AudioEncoder::load(&model_dir, 4).unwrap();
            let expected: Vec<f32> =
                bytemuck::cast_slice(&std::fs::read(dir.join("audio_emb_ref.f32")).unwrap()).to_vec();
            let first = encoder.embed(&audio[..WINDOW_SAMPLES]).unwrap();
            let cosine: f32 = first.iter().zip(&expected[..EMBEDDING_DIMS]).map(|(a, b)| a * b).sum();
            assert!(cosine > 0.9999, "cosine to torch {cosine}");
        }
    }
}
