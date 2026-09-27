//! Qwen3-Reranker-0.6B (slim export: `[b, 2]` no/yes logits) on ONNX
//! Runtime, candidates batched with left padding.

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
        let ids = self.input_ids(query, doc);
        Ok(self.run(&[&ids])?[0])
    }

    /// One forward pass over left-padded sequences; P(yes) per row. The
    /// graph scores the last position, which left padding keeps real, and
    /// RoPE is shift-invariant, so padded rows match unpadded scores
    /// (checked in `tests/reranker_golden.rs`).
    fn run(&self, seqs: &[&[u32]]) -> Result<Vec<f32>, LocalError> {
        let (ids, mask, width) = left_pad(seqs);
        let rows = seqs.len();
        let inputs = ort::inputs![
            "input_ids" => Tensor::from_array((vec![rows, width], ids.into_boxed_slice())).map_err(rt)?,
            "attention_mask" => Tensor::from_array((vec![rows, width], mask.into_boxed_slice())).map_err(rt)?,
        ];
        let mut session = self.session.lock().map_err(|_| LocalError::Poisoned)?;
        let outputs = session.run(inputs).map_err(rt)?;
        let (_, data) = outputs["yes_no"].try_extract_tensor::<f32>().map_err(rt)?;
        Ok(data
            .chunks(2)
            .take(rows)
            .map(|z| p_yes(z[0], z[1]))
            .collect())
    }

    /// Scores in batches of similar-length candidates (little padding).
    pub fn score_all(&self, query: &str, docs: &[String]) -> Result<Vec<f32>, LocalError> {
        let seqs: Vec<Vec<u32>> = docs.iter().map(|d| self.input_ids(query, d)).collect();
        let lens: Vec<usize> = seqs.iter().map(Vec::len).collect();
        let mut scores = vec![0.0; docs.len()];
        for batch in length_batches(&lens, BATCH_SIZE) {
            let rows: Vec<&[u32]> = batch.iter().map(|&i| seqs[i].as_slice()).collect();
            for (&i, s) in batch.iter().zip(self.run(&rows)?) {
                scores[i] = s;
            }
        }
        Ok(scores)
    }
}

/// Candidates per forward pass. ponytail: fixed; tune if GPU memory or
/// latency says otherwise.
const BATCH_SIZE: usize = 8;

/// Left-pads to the longest sequence. Returns `(ids, mask, width)`, row
/// major. The pad id is irrelevant: padded positions are masked out.
fn left_pad(seqs: &[&[u32]]) -> (Vec<i64>, Vec<i64>, usize) {
    let width = seqs.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut ids = Vec::with_capacity(seqs.len() * width);
    let mut mask = Vec::with_capacity(seqs.len() * width);
    for s in seqs {
        let pad = width - s.len();
        ids.extend(std::iter::repeat_n(0i64, pad));
        ids.extend(s.iter().map(|&x| i64::from(x)));
        mask.extend(std::iter::repeat_n(0i64, pad));
        mask.extend(std::iter::repeat_n(1i64, s.len()));
    }
    (ids, mask, width)
}

/// Indices grouped into batches of similar length, shortest first.
fn length_batches(lens: &[usize], batch: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..lens.len()).collect();
    order.sort_by_key(|&i| lens[i]);
    order.chunks(batch.max(1)).map(<[usize]>::to_vec).collect()
}

static RERANKER: OnceLock<Arc<QwenReranker>> = OnceLock::new();
static RERANKER_LOAD: Mutex<()> = Mutex::new(());

/// Process-wide reranker; failures are not cached (see `shared_laya`).
pub fn shared_reranker(dir: &Path, pref: DevicePref) -> Result<Arc<QwenReranker>, LocalError> {
    if let Some(model) = RERANKER.get() {
        return Ok(model.clone());
    }
    // A timed-out cold load may still run when the next task starts.
    // Serialize initialization so it cannot allocate a second model.
    let _loading = RERANKER_LOAD.lock().map_err(|_| LocalError::Poisoned)?;
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

    #[test]
    fn left_pad_right_aligns_every_sequence() {
        let (ids, mask, width) = left_pad(&[&[5, 6], &[7]]);
        assert_eq!(width, 2);
        assert_eq!(ids, vec![5, 6, 0, 7]);
        assert_eq!(mask, vec![1, 1, 0, 1]);
    }

    #[test]
    fn length_batches_group_similar_lengths_shortest_first() {
        assert_eq!(
            length_batches(&[5, 1, 3, 2], 2),
            vec![vec![1, 3], vec![2, 0]]
        );
        assert_eq!(length_batches(&[4, 4, 4], 8), vec![vec![0, 1, 2]]);
        assert!(length_batches(&[], 8).is_empty());
    }
}
