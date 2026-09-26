//! Model weights: CLAP's two towers and its tokenizer, fetched on first use.
//!
//! Xenova's ONNX export of `laion/clap-htsat-unfused`, pinned to one revision
//! and checked by hash, so what runs here is what was validated against the
//! torch model. The weights are shared across corpora, so they live in the
//! model directory rather than following TWO_KHZ_DATA_DIR.

use super::Job;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

pub const CLAP_AUDIO: &str = "clap-htsat-unfused-audio.onnx";
pub const CLAP_TEXT: &str = "clap-htsat-unfused-text.onnx";
pub const CLAP_TOKENIZER: &str = "clap-htsat-unfused-tokenizer.json";

const REPO: &str = "https://huggingface.co/Xenova/clap-htsat-unfused/resolve";
const REVISION: &str = "c28f2883575e590e04d3146ff0713c2448d691ba";

struct Weights {
    name: &'static str,
    remote: &'static str,
    bytes: u64,
    sha256: &'static str,
}

const WEIGHTS: [Weights; 3] = [
    Weights {
        name: CLAP_AUDIO,
        remote: "onnx/audio_model.onnx",
        bytes: 117_528_416,
        sha256: "a1c2b43c44f71e0fa841a4b86700886c199bf87699ea45632c4d831bc6c88957",
    },
    Weights {
        name: CLAP_TEXT,
        remote: "onnx/text_model.onnx",
        bytes: 501_513_769,
        sha256: "6df7c51f2d78e236a03f2b816e613d538c815639d13644fe7c2124f439da9648",
    },
    Weights {
        name: CLAP_TOKENIZER,
        remote: "tokenizer.json",
        bytes: 2_108_774,
        sha256: "dc239041d98de27ffc3975473a1a23e3db4c937b23c138c38bbc66588bd247e5",
    },
];

/// Whether a file is already in place. Size only: hashing 500MB on every
/// start would be slower than the thing it guards.
pub fn present(model_dir: &Path, name: &str) -> bool {
    let expected = WEIGHTS.iter().find(|w| w.name == name).map(|w| w.bytes);
    std::fs::metadata(model_dir.join(name))
        .map(|m| Some(m.len()) == expected)
        .unwrap_or(false)
}

/// Download whichever of `names` are missing.
pub async fn ensure(model_dir: &Path, names: &[&str], job: &Job) -> Result<()> {
    for weights in WEIGHTS.iter().filter(|w| names.contains(&w.name)) {
        if present(model_dir, weights.name) {
            continue;
        }
        if job.cancelled() {
            bail!("cancelled");
        }
        download(model_dir, weights, job)
            .await
            .with_context(|| format!("downloading {}", weights.name))?;
    }
    Ok(())
}

/// Everything, for `two-khz models`.
pub async fn ensure_all(model_dir: &Path, job: &Job) -> Result<()> {
    let names: Vec<&str> = WEIGHTS.iter().map(|w| w.name).collect();
    ensure(model_dir, &names, job).await
}

async fn download(model_dir: &Path, weights: &Weights, job: &Job) -> Result<()> {
    std::fs::create_dir_all(model_dir)
        .with_context(|| format!("creating {}", model_dir.display()))?;
    let url = format!("{REPO}/{REVISION}/{}", weights.remote);
    job.log(format!(
        "fetching {} ({:.0} MB) into {}",
        weights.name,
        weights.bytes as f64 / 1e6,
        model_dir.display()
    ));

    let response = reqwest::Client::new()
        .get(&url)
        .send()
        .await?
        .error_for_status()?;

    let target = model_dir.join(weights.name);
    let partial = target.with_extension("part");
    let mut file = std::fs::File::create(&partial)?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut next_report = 0.25;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if job.cancelled() {
            drop(file);
            let _ = std::fs::remove_file(&partial);
            bail!("cancelled");
        }
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        received += chunk.len() as u64;
        let fraction = received as f64 / weights.bytes as f64;
        if fraction >= next_report && fraction < 1.0 {
            job.log(format!("  {}: {:.0}%", weights.name, fraction * 100.0));
            next_report += 0.25;
        }
    }
    file.flush()?;
    drop(file);

    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if digest != weights.sha256 {
        let _ = std::fs::remove_file(&partial);
        bail!("{} arrived with sha256 {digest}, expected {}", weights.name, weights.sha256);
    }
    std::fs::rename(&partial, &target)?;
    Ok(())
}
