//! Audio in: decoding to mono and resampling, before either analysis reads it.
//!
//! MP3 is always there, it is what Stelly analyses. Other formats come with
//! the `all-codecs` feature, for local files.

use anyhow::{bail, Context, Result};
use std::path::Path;
use symphonia::core::io::MediaSource;


/// Mono samples in [-1, 1] and their rate.
#[derive(Debug, Clone)]
pub struct Pcm {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Pcm {
    pub fn seconds(&self) -> f64 {
        self.samples.len() as f64 / self.rate as f64
    }
}

// ----------------------------------------------------------------- decoding

/// Decode MP3 bytes to mono. Frames that fail, the first one usually does,
/// since its bit reservoir points into audio we did not fetch, are skipped.
pub fn decode_mp3(bytes: Vec<u8>) -> Result<Pcm> {
    decode(Box::new(std::io::Cursor::new(bytes)), Some("mp3")).context("not an MP3 stream")
}

/// Decode a whole file to mono, in any format the enabled codecs cover.
pub fn decode_file(path: &Path) -> Result<Pcm> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let extension = path.extension().and_then(|e| e.to_str());
    decode(Box::new(file), extension).with_context(|| format!("decoding {}", path.display()))
}

fn decode(source: Box<dyn MediaSource>, extension: Option<&str>) -> Result<Pcm> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let stream = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .context("unrecognised audio format")?;

    let track = format
        .default_track(TrackType::Audio)
        .context("no audio track")?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .context("no audio codec parameters")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .context("no decoder for this stream")?;

    let mut mono = Vec::new();
    let mut interleaved: Vec<f32> = Vec::new();
    let mut rate = 0u32;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            // A truncated final frame is the normal end of a byte range.
            Err(Error::IoError(_)) => break,
            Err(err) => return Err(err).context("demuxing"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            Err(Error::DecodeError(_)) | Err(Error::IoError(_)) => continue,
            Err(err) => return Err(err).context("decoding"),
        };

        rate = buffer.spec().rate();
        let channels = buffer.spec().channels().count().max(1);
        interleaved.resize(buffer.samples_interleaved(), 0.0);
        buffer.copy_to_slice_interleaved(&mut interleaved);
        let scale = 1.0 / channels as f32;
        mono.extend(
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() * scale),
        );
    }

    if mono.is_empty() || rate == 0 {
        bail!("decoded no audio");
    }
    Ok(Pcm {
        samples: mono,
        rate,
    })
}

// --------------------------------------------------------------- resampling

/// Windowed-sinc resampling, polyphase when the two rates share a small
/// period, 44.1k to 48k is 147 in, 160 out.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    const HALF_TAPS: i64 = 32;

    let divisor = gcd(from as u64, to as u64);
    let (up, down) = (to as u64 / divisor, from as u64 / divisor);
    // Low-pass at the lower of the two Nyquists, a little under it.
    let cutoff = (to as f64 / from as f64).min(1.0) * 0.95;
    let half = (HALF_TAPS as f64 / cutoff.min(1.0)).ceil() as i64;

    let kernel = |x: f64| -> f64 {
        if x.abs() >= half as f64 {
            return 0.0;
        }
        let sinc = if x == 0.0 {
            1.0
        } else {
            let px = std::f64::consts::PI * cutoff * x;
            px.sin() / px
        };
        // Blackman over the kernel's support.
        let n = (x / half as f64 + 1.0) / 2.0;
        let window = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * n).cos()
            + 0.08 * (4.0 * std::f64::consts::PI * n).cos();
        cutoff * sinc * window
    };

    let out_len = (input.len() as u64 * up / down) as usize;
    let taps = (2 * half) as usize;

    // One row of taps per phase, when there are few enough phases to tabulate.
    let table: Option<Vec<f32>> = (up <= 4096).then(|| {
        let mut table = vec![0f32; up as usize * taps];
        for phase in 0..up as usize {
            let frac = phase as f64 / up as f64;
            for (j, slot) in table[phase * taps..(phase + 1) * taps].iter_mut().enumerate() {
                let k = j as i64 - half + 1;
                *slot = kernel(k as f64 - frac) as f32;
            }
        }
        table
    });

    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len as u64 {
        let position = n * down;
        let base = (position / up) as i64;
        let phase = (position % up) as usize;
        let mut acc = 0f32;
        for j in 0..taps {
            let index = base + j as i64 - half + 1;
            if index < 0 || index >= input.len() as i64 {
                continue;
            }
            let weight = match &table {
                Some(table) => table[phase * taps + j],
                None => kernel((j as i64 - half + 1) as f64 - phase as f64 / up as f64) as f32,
            };
            acc += weight * input[index as usize];
        }
        out.push(acc);
    }
    out
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_a_tone_and_its_length() {
        let from = 44_100;
        let tone: Vec<f32> = (0..from)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / from as f32).sin())
            .collect();
        let out = resample(&tone, from, 48_000);
        assert_eq!(out.len(), 48_000);
        // Away from the edges it should still be a unit sine at 1kHz.
        for (n, v) in out.iter().enumerate().skip(1000).take(1000) {
            let expected = (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
            assert!((v - expected).abs() < 2e-3, "sample {n}: {v} vs {expected}");
        }
    }
}
