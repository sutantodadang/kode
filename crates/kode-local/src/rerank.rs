//! Qwen3-Reranker-0.6B (slim export: `[b, 2]` no/yes logits) on ONNX
//! Runtime, one candidate per run.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;

use crate::device::{Device, DevicePref, build_session};
use crate::error::{LocalError, rt};

#[derive(Debug, Clone, Deserialize)]
pub struct RerankerConfig {
    pub prefix: String,
    pub suffix: String,
    pub instruction: String,
    pub max_len: usize,
    pub doc_char_cap: usize,
}

impl RerankerConfig {
    pub fn load(dir: &Path) -> Result<Self, LocalError> {
        let path = dir.join("reranker.json");
        let text = std::fs::read_to_string(&path).map_err(|_| LocalError::Missing(path.clone()))?;
        serde_json::from_str(&text)
            .map_err(|e| LocalError::Config(format!("{}: {e}", path.display())))
    }
}

pub struct QwenReranker {
    session: Mutex<Session>,
    tok: tokenizers::Tokenizer,
    cfg: RerankerConfig,
    device: Device,
}

/// `softmax([no, yes])[1]`, overflow-safe.
fn p_yes(no: f32, yes: f32) -> f32 {
    let m = no.max(yes);
    let (n, y) = ((no - m).exp(), (yes - m).exp());
    y / (n + y)
}

impl QwenReranker {
    /// Expects `model.onnx`, `model.onnx.data`, `tokenizer.json`,
    /// `reranker.json` in `dir`; the caller verifies checksums first.
    pub fn load(dir: &Path, pref: DevicePref) -> Result<Self, LocalError> {
        let cfg = RerankerConfig::load(dir)?;
        let tok = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| LocalError::Tokenizer(e.to_string()))?;
        let (session, device) = build_session(&dir.join("model.onnx"), pref)?;
        Ok(Self {
            session: Mutex::new(session),
            tok,
            cfg,
            device,
        })
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    fn encode(&self, text: &str) -> Vec<u32> {
        self.tok
            .encode(text, false)
            .map(|e| e.get_ids().to_vec())
            .unwrap_or_default()
    }

    /// `prefix + content[..room] + suffix`, as `build_reranker.py` builds it.
    pub fn input_ids(&self, query: &str, doc: &str) -> Vec<u32> {
        let doc: String = doc.chars().take(self.cfg.doc_char_cap).collect();
        let content = format!(
            "<Instruct>: {}\n<Query>: {query}\n<Document>: {doc}",
            self.cfg.instruction
        );
        let prefix = self.encode(&self.cfg.prefix);
        let suffix = self.encode(&self.cfg.suffix);
        let mut body = self.encode(&content);
        body.truncate(self.cfg.max_len.saturating_sub(prefix.len() + suffix.len()));
        [prefix, body, suffix].concat()
    }

    pub fn score(&self, query: &str, doc: &str) -> Result<f32, LocalError> {
        let ids: Vec<i64> = self
            .input_ids(query, doc)
            .iter()
            .map(|&x| i64::from(x))
            .collect();
        let n = ids.len();
        let inputs = ort::inputs![
            "input_ids" => Tensor::from_array((vec![1usize, n], ids.into_boxed_slice())).map_err(rt)?,
            "attention_mask" => Tensor::from_array((vec![1usize, n], vec![1i64; n].into_boxed_slice())).map_err(rt)?,
        ];
        let mut session = self.session.lock().map_err(|_| LocalError::Poisoned)?;
        let outputs = session.run(inputs).map_err(rt)?;
        let (_, data) = outputs["yes_no"].try_extract_tensor::<f32>().map_err(rt)?;
        Ok(p_yes(data[0], data[1]))
    }

    /// ponytail: one sequence per candidate; batch with left padding if
    /// rerank latency ever matters.
    pub fn score_all(&self, query: &str, docs: &[String]) -> Result<Vec<f32>, LocalError> {
        docs.iter().map(|d| self.score(query, d)).collect()
    }
}

static RERANKER: OnceLock<Arc<QwenReranker>> = OnceLock::new();

/// Process-wide reranker; failures are not cached (see `shared_laya`).
pub fn shared_reranker(dir: &Path, pref: DevicePref) -> Result<Arc<QwenReranker>, LocalError> {
    if let Some(model) = RERANKER.get() {
        return Ok(model.clone());
    }
    let model = Arc::new(QwenReranker::load(dir, pref)?);
    Ok(RERANKER.get_or_init(|| model).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_probability_is_softmax_of_no_yes_pair() {
        assert!((p_yes(0.0, 0.0) - 0.5).abs() < 1e-6);
        assert!(p_yes(-10.0, 10.0) > 0.999);
        assert!(p_yes(10.0, -10.0) < 0.001);
        assert!(
            (p_yes(1000.0, 1000.0) - 0.5).abs() < 1e-6,
            "must not overflow"
        );
    }
}
