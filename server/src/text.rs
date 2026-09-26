//! CLAP text embeddings, via ONNX: text steering at query time, and the mood
//! and style prompts `build-space` scores every track against.
//!
//! Needs the text tower and tokenizer in the model directory; `two-khz models`
//! fetches them, and so does `build-space` on first run.

use crate::pipeline::models::{CLAP_TEXT, CLAP_TOKENIZER};
use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::TensorRef;
use std::path::Path;
use tokenizers::Tokenizer;

pub struct TextEncoder {
    session: Session,
    tokenizer: Tokenizer,
}

impl TextEncoder {
    /// Returns Ok(None) when the weights have not been fetched, so the app can
    /// hide text steering rather than refusing to start.
    pub fn load(model_dir: &Path) -> Result<Option<Self>> {
        let onnx_path = model_dir.join(CLAP_TEXT);
        let tokenizer_path = model_dir.join(CLAP_TOKENIZER);
        if !onnx_path.is_file() || !tokenizer_path.is_file() {
            return Ok(None);
        }

        let session = Session::builder()
            .context("creating ONNX session builder")?
            .commit_from_file(&onnx_path)
            .with_context(|| format!("loading {}", onnx_path.display()))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("loading {}: {e}", tokenizer_path.display()))?;

        Ok(Some(Self { session, tokenizer }))
    }

    /// L2-normalised 512-d embedding, in the same space as the stored audio
    /// embeddings. One phrase per call: the export takes no attention mask,
    /// so a padded batch would embed the padding too.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenising {text:?}: {e}"))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&v| v as i64).collect();
        let shape = [1_i64, ids.len() as i64];

        let outputs = self.session.run(ort::inputs![
            "input_ids" => TensorRef::from_array_view((shape, ids.as_slice()))?,
        ])?;

        let (_shape, data) = outputs["text_embeds"].try_extract_tensor::<f32>()?;
        let mut vector = data.to_vec();
        crate::pipeline::clap::normalise(&mut vector);
        Ok(vector)
    }
}
