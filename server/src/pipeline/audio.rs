//! Excerpts: the middle 90 seconds of a track, fetched and decoded in process.
//!
//! Intros, outros and fades skew tempo and timbre statistics badly, so only the
//! middle of a track is analysed. Qobuz serves MP3 320 at a constant bitrate,
//! which makes "the middle 90 seconds" a byte range: one request for the
//! header, one for the slice, and no need for ffmpeg to seek over HTTP.
//!
//! What lands in the cache is that slice of MP3, cut to a frame boundary. It is
//! smaller than decoded audio would be, and the cache is disposable anyway,
//! deleting it costs only the time to fetch again.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

pub const EXCERPT_SECONDS: f64 = 90.0;
/// Shorter tracks are not worth the request.
pub const MIN_TRACK_SECONDS: i64 = 45;
/// Below this an excerpt is noise; a seek past the end lands here too.
pub const MIN_EXCERPT_SECONDS: f64 = 20.0;
/// Extra audio fetched past the window, so a bitrate estimate that is slightly
/// off still leaves a full 90 seconds once the slice is decoded.
const SLACK_SECONDS: f64 = 3.0;

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

/// Centre the window in the track.
pub fn excerpt_start(duration: Option<i64>) -> f64 {
    match duration {
        Some(d) if d as f64 > EXCERPT_SECONDS => ((d as f64 - EXCERPT_SECONDS) / 2.0).max(0.0),
        _ => 0.0,
    }
}

// ------------------------------------------------------------------ fetching

/// Pull the middle of a track from a signed stream URL, as MP3 bytes that
/// begin on a frame boundary.
pub async fn fetch_excerpt(
    http: &reqwest::Client,
    url: &str,
    duration: Option<i64>,
) -> Result<Vec<u8>> {
    // The first ten bytes carry the ID3v2 header, if any, and the reply's
    // Content-Range carries the file size. Together they locate the audio.
    let head = http
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-9")
        .send()
        .await
        .context("requesting the stream header")?;
    let status = head.status();
    if !status.is_success() {
        bail!("stream header -> HTTP {status}");
    }

    let total = head
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse::<u64>().ok());

    // No range support: the whole file came back, so slice it here instead.
    let Some(total) = total.filter(|_| status == reqwest::StatusCode::PARTIAL_CONTENT) else {
        let body = head.bytes().await.context("reading the stream")?;
        let audio_start = id3_size(&body) as usize;
        let audio = body.get(audio_start..).unwrap_or_default();
        let (start, end) = window(audio.len() as u64, duration);
        let slice = &audio[start as usize..(end as usize).min(audio.len())];
        return Ok(frame_aligned(slice)?.to_vec());
    };

    let header = head.bytes().await.context("reading the stream header")?;
    let audio_start = id3_size(&header);
    let audio_len = total.saturating_sub(audio_start);
    let (start, end) = window(audio_len, duration);

    let body = http
        .get(url)
        .header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", audio_start + start, audio_start + end - 1),
        )
        .send()
        .await
        .context("requesting the excerpt")?
        .error_for_status()
        .context("requesting the excerpt")?
        .bytes()
        .await
        .context("reading the excerpt")?;

    Ok(frame_aligned(&body)?.to_vec())
}

/// The byte range, within the audio, that holds the excerpt plus slack.
fn window(audio_len: u64, duration: Option<i64>) -> (u64, u64) {
    let Some(seconds) = duration.filter(|&d| d > 0).map(|d| d as f64) else {
        return (0, audio_len);
    };
    let per_second = audio_len as f64 / seconds;
    let start = (excerpt_start(duration) * per_second) as u64;
    let length = ((EXCERPT_SECONDS + SLACK_SECONDS) * per_second) as u64;
    (start.min(audio_len), (start + length).min(audio_len))
}

/// Size of a leading ID3v2 tag, header and footer included; 0 without one.
fn id3_size(bytes: &[u8]) -> u64 {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return 0;
    }
    // Synchsafe: seven bits per byte.
    let size = bytes[6..10]
        .iter()
        .fold(0u64, |acc, &b| (acc << 7) | (b & 0x7f) as u64);
    let footer = if bytes[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

// ------------------------------------------------------------- frame sync

/// An MPEG audio layer III frame header, enough of it to find the next one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FrameHeader {
    version: u8,
    sample_rate: u32,
    length: usize,
}

fn parse_header(bytes: &[u8]) -> Option<FrameHeader> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (bytes[1] >> 3) & 0b11; // 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5
    let layer = (bytes[1] >> 1) & 0b11; // 1 = layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let bitrate_index = (bytes[2] >> 4) as usize;
    let rate_index = ((bytes[2] >> 2) & 0b11) as usize;
    if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let padding = ((bytes[2] >> 1) & 1) as usize;

    const MPEG1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const MPEG2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let (kbps, rates, factor) = match version {
        3 => (MPEG1[bitrate_index], [44_100, 48_000, 32_000], 144),
        2 => (MPEG2[bitrate_index], [22_050, 24_000, 16_000], 72),
        _ => (MPEG2[bitrate_index], [11_025, 12_000, 8_000], 72),
    };
    let sample_rate = rates[rate_index];
    let length = (factor * kbps as usize * 1000) / sample_rate as usize + padding;
    Some(FrameHeader {
        version,
        sample_rate,
        length,
    })
}

/// The slice from the first frame that is followed by two more like it.
///
/// A byte range starts mid-frame, and 0xFFE turns up inside audio data too, so
/// one plausible header proves nothing; three in a row, agreeing on version
/// and rate, is what a real frame boundary looks like.
fn frame_aligned(bytes: &[u8]) -> Result<&[u8]> {
    const CHAIN: usize = 3;
    'candidates: for start in 0..bytes.len().saturating_sub(4) {
        let Some(first) = parse_header(&bytes[start..]) else {
            continue;
        };
        let mut at = start;
        let mut header = first;
        for _ in 1..CHAIN {
            at += header.length;
            match bytes.get(at..).and_then(parse_header) {
                Some(next) if next.version == first.version && next.sample_rate == first.sample_rate => {
                    header = next
                }
                _ => continue 'candidates,
            }
        }
        return Ok(&bytes[start..]);
    }
    bail!("no MP3 frames in {} bytes of stream", bytes.len())
}

// ----------------------------------------------------------------- decoding

/// Decode MP3 bytes to mono. Frames that fail, the first one usually does,
/// since its bit reservoir points into audio we did not fetch, are skipped.
pub fn decode_mp3(bytes: Vec<u8>) -> Result<Pcm> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let stream = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .context("not an MP3 stream")?;

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

/// Decode, cut to the window, and check there is enough of it.
pub fn excerpt_pcm(bytes: Vec<u8>) -> Result<Pcm> {
    let mut pcm = decode_mp3(bytes)?;
    pcm.samples
        .truncate((EXCERPT_SECONDS * pcm.rate as f64) as usize);
    let seconds = pcm.seconds();
    if seconds < MIN_EXCERPT_SECONDS {
        bail!(
            "excerpt decoded to only {seconds:.1}s of audio \
             (empty stream, or the track duration Qobuz reported was wrong)"
        );
    }
    Ok(pcm)
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

// -------------------------------------------------------------------- cache

/// How much new audio to accept before measuring the cache again. Statting
/// every file after every download is O(cache) per track.
const RESCAN_BYTES: u64 = 512 * 1024 * 1024;

/// Size-capped LRU of fetched excerpts, keyed by track id. Shared by the
/// fetch tasks; every stat tolerates the file having gone meanwhile.
pub struct AudioCache {
    root: PathBuf,
    max_bytes: u64,
    unscanned: AtomicU64,
}

impl AudioCache {
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self {
            root,
            max_bytes,
            unscanned: AtomicU64::new(0),
        }
    }

    fn path_for(&self, track_id: i64) -> PathBuf {
        // Shard by the last two digits to keep directories small.
        self.root
            .join(format!("{:02}", track_id.rem_euclid(100)))
            .join(format!("{track_id}.mp3"))
    }

    pub fn get(&self, track_id: i64) -> Option<Vec<u8>> {
        let path = self.path_for(track_id);
        let bytes = std::fs::read(&path).ok().filter(|b| !b.is_empty())?;
        // Mark as recently used.
        if let Ok(file) = std::fs::File::options().append(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    /// Store an excerpt, then evict if enough has landed to be worth checking.
    /// Returns how many excerpts were evicted.
    pub fn put(&self, track_id: i64, bytes: &[u8]) -> Result<usize> {
        let path = self.path_for(track_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A unique temporary, renamed into place, so a concurrent reader never
        // sees half a file and two writers never share one.
        let tmp = path.with_extension(format!(
            "{}.{:?}.part",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;

        let seen = self.unscanned.fetch_add(bytes.len() as u64, Ordering::SeqCst) + bytes.len() as u64;
        if seen < RESCAN_BYTES {
            return Ok(0);
        }
        self.unscanned.store(0, Ordering::SeqCst);
        Ok(self.evict())
    }

    /// Delete least-recently-used excerpts until under the cap.
    fn evict(&self) -> usize {
        let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        collect(&self.root, &mut files);
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.2);

        let mut removed = 0;
        for (path, size, _) in files {
            if total <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
                removed += 1;
            }
        }
        removed
    }
}

/// Finished excerpts under `dir`, never the in-flight temporaries.
fn collect(dir: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "mp3") {
            out.push((path, meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id3_size_counts_header_and_synchsafe_body() {
        let mut header = *b"ID3\x04\x00\x00\x00\x00\x02\x01";
        assert_eq!(id3_size(&header), 10 + 257);
        header[5] = 0x10;
        assert_eq!(id3_size(&header), 20 + 257);
        assert_eq!(id3_size(b"\xff\xfb\x90\x00abcdef"), 0);
    }

    #[test]
    fn frame_sync_skips_a_false_header() {
        // 320kbps, 44.1kHz, no padding: 1044-byte frames.
        let header = [0xff, 0xfb, 0xe0, 0x00];
        assert_eq!(parse_header(&header).unwrap().length, 1044);

        let mut stream = vec![0u8; 7];
        stream.extend_from_slice(&[0xff, 0xfb, 0xe0, 0x00]); // lone false sync
        stream.extend_from_slice(&[0u8; 50]);
        let real = stream.len();
        for _ in 0..3 {
            let mut frame = vec![0u8; 1044];
            frame[..4].copy_from_slice(&header);
            stream.extend(frame);
        }
        let aligned = frame_aligned(&stream).unwrap();
        assert_eq!(aligned.len(), stream.len() - real);
    }

    /// The whole fetch against a server that honours byte ranges, as the CDN
    /// does. Needs a real MP3 of a few minutes, 320kbps like Qobuz's:
    /// `TWO_KHZ_MP3_SAMPLE=track.mp3 cargo test -- --ignored excerpt`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn excerpt_from_the_middle_of_a_served_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let path = std::env::var("TWO_KHZ_MP3_SAMPLE").unwrap();
        let file = std::sync::Arc::new(std::fs::read(&path).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let served = file.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let mut request = vec![0u8; 4096];
                let n = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_lowercase();
                let range = request
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| r.trim().split_once('-'))
                    .map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()))
                    .unwrap();
                let end = range.1.min(served.len() - 1);
                let body = &served[range.0..=end];
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                     Content-Range: bytes {}-{end}/{}\r\nConnection: close\r\n\r\n",
                    body.len(),
                    range.0,
                    served.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        // Decode the whole file once to know its true length and content.
        let whole = decode_mp3(file.to_vec()).unwrap();
        let duration = whole.seconds().round() as i64;

        let url = format!("http://{address}/track.mp3");
        let bytes = fetch_excerpt(&reqwest::Client::new(), &url, Some(duration)).await.unwrap();
        assert!(bytes.len() < file.len() / 2, "fetched {} of {} bytes", bytes.len(), file.len());

        let pcm = excerpt_pcm(bytes).unwrap();
        assert_eq!(pcm.rate, whole.rate);
        assert!((pcm.seconds() - EXCERPT_SECONDS).abs() < 0.1, "{}s", pcm.seconds());
    }

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
