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
pub use stelly_audio::decode::{decode_mp3, resample, Pcm};

pub const EXCERPT_SECONDS: f64 = 90.0;
pub use stelly_core::api::MIN_TRACK_SECONDS;
/// Below this an excerpt is noise; a seek past the end lands here too.
pub const MIN_EXCERPT_SECONDS: f64 = 20.0;
/// Extra audio fetched past the window, so a bitrate estimate that is slightly
/// off still leaves a full 90 seconds once the slice is decoded.
const SLACK_SECONDS: f64 = 3.0;

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

/// Decode, cut to the window, and check there is enough of it. The bytes
/// are a slice of MP3, or an excerpt `excerpt_of_whole` cut and packed.
pub fn excerpt_pcm(bytes: Vec<u8>) -> Result<Pcm> {
    let mut pcm = match unpack(&bytes) {
        Some(pcm) => pcm,
        None => decode_mp3(bytes)?,
    };
    pcm.samples
        .truncate((EXCERPT_SECONDS * pcm.rate as f64) as usize);
    let seconds = pcm.seconds();
    if seconds < MIN_EXCERPT_SECONDS {
        bail!(
            "excerpt decoded to only {seconds:.1}s of audio \
             (empty stream, or the track duration the provider reported was wrong)"
        );
    }
    Ok(pcm)
}

// ------------------------------------------------------------- whole files
//
// A stream that is not constant-bitrate MP3 cannot be cut by byte range: a
// FLAC's or an Opus's bytes do not map evenly onto time. Those are fetched
// whole, decoded, and the excerpt is cut from the samples, then packed as
// 16-bit PCM, for the cache and the extractors to take like any other.

/// What a packed excerpt starts with; no MP3 frame or ID3 tag does.
const PACKED: &[u8; 8] = b"STLYPCM1";

/// A whole track's bytes, from a URL that needs no credentials.
pub async fn fetch_whole(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = http.get(url).send().await.context("requesting the stream")?;
    let response = response.error_for_status().context("requesting the stream")?;
    Ok(response.bytes().await.context("reading the stream")?.to_vec())
}

/// A whole track's bytes decoded to mono, whatever its format: MIME types
/// name it, and an unnamed one is taken for MP3, as streams mostly are.
pub fn decode_any(bytes: Vec<u8>, mime_type: Option<&str>) -> Result<Pcm> {
    let extension = match mime_type.map(|m| m.split(';').next().unwrap_or(m).trim().to_ascii_lowercase()) {
        None => return decode_mp3(bytes),
        Some(mime) => match mime.as_str() {
            "audio/mpeg" | "audio/mp3" => return decode_mp3(bytes),
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/ogg" | "application/ogg" | "audio/vorbis" => "ogg",
            "audio/opus" => "opus",
            "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "audio/aac" => "m4a",
            "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
            "audio/aiff" | "audio/x-aiff" => "aiff",
            _ => "audio",
        },
    };
    // The decoder reads files, where the extension says what is in them.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("stelly-decode-{}-{n}.{extension}", std::process::id()));
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    let decoded = stelly_audio::decode::decode_file(&path);
    let _ = std::fs::remove_file(&path);
    decoded
}

/// The excerpt of a whole track, centred as `fetch_excerpt` centres it,
/// packed for the cache.
pub fn excerpt_of_whole(bytes: Vec<u8>, mime_type: Option<&str>) -> Result<Vec<u8>> {
    Ok(pack(&excerpt_of(&decode_any(bytes, mime_type)?)))
}

/// The excerpt of a whole track's samples, centred as `fetch_excerpt`
/// centres it.
pub fn excerpt_of(pcm: &Pcm) -> Pcm {
    let seconds = pcm.seconds().round() as i64;
    let start = (excerpt_start(Some(seconds)) * pcm.rate as f64) as usize;
    let end = (start + (EXCERPT_SECONDS * pcm.rate as f64) as usize).min(pcm.samples.len());
    Pcm { samples: pcm.samples[start.min(end)..end].to_vec(), rate: pcm.rate }
}

fn pack(pcm: &Pcm) -> Vec<u8> {
    let mut out = Vec::with_capacity(PACKED.len() + 4 + pcm.samples.len() * 2);
    out.extend_from_slice(PACKED);
    out.extend_from_slice(&pcm.rate.to_le_bytes());
    for sample in &pcm.samples {
        out.extend_from_slice(&((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

fn unpack(bytes: &[u8]) -> Option<Pcm> {
    let rest = bytes.strip_prefix(PACKED)?;
    let rate = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
    let samples = rest[4..]
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / i16::MAX as f32)
        .collect();
    Some(Pcm { samples, rate })
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
    /// `STELLY_MP3_SAMPLE=track.mp3 cargo test -- --ignored excerpt`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn excerpt_from_the_middle_of_a_served_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let path = std::env::var("STELLY_MP3_SAMPLE").unwrap();
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
    fn a_packed_excerpt_comes_back_as_it_went() {
        let pcm = Pcm { samples: vec![0.0, 0.5, -0.5, 1.0, -1.0], rate: 22_050 };
        let back = unpack(&pack(&pcm)).unwrap();
        assert_eq!(back.rate, 22_050);
        for (a, b) in pcm.samples.iter().zip(&back.samples) {
            assert!((a - b).abs() < 1e-4, "{a} came back as {b}");
        }
        assert!(unpack(b"ID3\x04rest").is_none());
    }
}