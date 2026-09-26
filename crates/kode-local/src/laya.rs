//! Laya multilingual decision model on ONNX Runtime (batch size 1).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;

use crate::device::{Device, DevicePref, build_session};
use crate::error::{LocalError, rt};
use crate::route::{
    RouteDecision, RouteInput, TaskRouter, resolve_route, route_questions, softmax_with_temperature,
};
use crate::sequence::{QType, QuestionDef, SeqTokenizer, Sequence, build_sequence, render_options};

#[derive(Debug, Clone, Deserialize)]
pub struct LayaConfig {
    pub max_len: usize,
    pub head_max_len: usize,
    pub temperature: Vec<f32>,
    #[serde(default)]
    pub temperature_by_options: BTreeMap<String, f32>,
    pub cls_id: u32,
    pub sep_id: u32,
    pub mask_id: u32,
    pub mask_token: String,
}

impl LayaConfig {
    pub fn load(dir: &Path) -> Result<Self, LocalError> {
        let path = dir.join("laya.json");
        let text = std::fs::read_to_string(&path).map_err(|_| LocalError::Missing(path.clone()))?;
        serde_json::from_str(&text)
            .map_err(|e| LocalError::Config(format!("{}: {e}", path.display())))
    }

    /// laya `temp_bucket`: per-cardinality temperature, else per question type.
    pub fn temperature_for(&self, qtype: QType, k: usize) -> f32 {
        let size = if k <= 2 {
            "2"
        } else if k <= 5 {
            "3-5"
        } else if k <= 10 {
            "6-10"
        } else {
            "11+"
        };
        self.temperature_by_options
            .get(&format!("{}:{size}", qtype.name()))
            .copied()
            .unwrap_or_else(|| self.temperature.get(qtype.index()).copied().unwrap_or(1.0))
    }
}

pub struct LayaTokenizer {
    inner: tokenizers::Tokenizer,
    cls: u32,
    sep: u32,
    mask: u32,
    mask_token: String,
}

impl LayaTokenizer {
    pub fn load(dir: &Path, cfg: &LayaConfig) -> Result<Self, LocalError> {
        let path = dir.join("tokenizer.json");
        let inner = tokenizers::Tokenizer::from_file(&path)
            .map_err(|e| LocalError::Tokenizer(e.to_string()))?;
        Ok(Self {
            inner,
            cls: cfg.cls_id,
            sep: cfg.sep_id,
            mask: cfg.mask_id,
            mask_token: cfg.mask_token.clone(),
        })
    }
}

impl SeqTokenizer for LayaTokenizer {
    fn encode(&self, text: &str) -> Vec<u32> {
        match self.inner.encode(text, false) {
            Ok(encoding) => encoding.get_ids().to_vec(),
            Err(e) => {
                tracing::warn!("laya tokenizer failed: {e}");
                Vec::new()
            }
        }
    }
    fn cls_id(&self) -> u32 {
        self.cls
    }
    fn sep_id(&self) -> u32 {
        self.sep
    }
    fn mask_id(&self) -> u32 {
        self.mask
    }
    fn mask_token(&self) -> &str {
        &self.mask_token
    }
}

pub struct LayaModel {
    session: Mutex<Session>,
    tok: LayaTokenizer,
    cfg: LayaConfig,
    device: Device,
}

fn i64s(v: &[u32]) -> Vec<i64> {
    v.iter().map(|&x| i64::from(x)).collect()
}

impl LayaModel {
    /// Expects `model.onnx`, `tokenizer.json`, `laya.json` in `dir`; the
    /// caller verifies checksums first (`models::verify_model_dir`).
    pub fn load(dir: &Path, pref: DevicePref) -> Result<Self, LocalError> {
        let cfg = LayaConfig::load(dir)?;
        let tok = LayaTokenizer::load(dir, &cfg)?;
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

    pub fn sequence(&self, state: &str, q: &QuestionDef) -> Sequence {
        build_sequence(&self.tok, state, q, self.cfg.max_len, self.cfg.head_max_len)
    }

    fn logits(&self, state: &str, q: &QuestionDef) -> Result<Vec<f32>, LocalError> {
        let seq = self.sequence(state, q);
        let k = seq.markers.len();
        if k != render_options(q).len() {
            return Err(LocalError::OptionsDoNotFit);
        }
        let n = seq.ids.len();
        let markers: Vec<i64> = seq.markers.iter().map(|&m| m as i64).collect();
        let inputs = ort::inputs![
            "input_ids" => Tensor::from_array((vec![1usize, n], i64s(&seq.ids).into_boxed_slice())).map_err(rt)?,
            "attention_mask" => Tensor::from_array((vec![1usize, n], vec![1i64; n].into_boxed_slice())).map_err(rt)?,
            "marker_pos" => Tensor::from_array((vec![1usize, k], markers.into_boxed_slice())).map_err(rt)?,
            "marker_mask" => Tensor::from_array((vec![1usize, k], vec![1i64; k].into_boxed_slice())).map_err(rt)?,
            "qtype" => Tensor::from_array((vec![1usize], vec![q.qtype.index() as i64].into_boxed_slice())).map_err(rt)?,
        ];
        let mut session = self.session.lock().map_err(|_| LocalError::Poisoned)?;
        let outputs = session.run(inputs).map_err(rt)?;
        let (_, data) = outputs["logits"].try_extract_tensor::<f32>().map_err(rt)?;
        Ok(data[..k].to_vec())
    }

    pub fn probs(&self, state: &str, q: &QuestionDef) -> Result<Vec<f32>, LocalError> {
        let logits = self.logits(state, q)?;
        Ok(softmax_with_temperature(
            &logits,
            self.cfg.temperature_for(q.qtype, logits.len()),
        ))
    }
}

pub struct LayaRouter {
    pub model: Arc<LayaModel>,
    pub min_confidence: f32,
}

#[async_trait]
impl TaskRouter for LayaRouter {
    async fn route(&self, input: &RouteInput) -> RouteDecision {
        let started = Instant::now();
        let questions = route_questions();
        let defs: Vec<QuestionDef> = questions.iter().map(|q| q.def.clone()).collect();
        let model = self.model.clone();
        let state = input.state();
        let probs = tokio::task::spawn_blocking(move || {
            defs.iter()
                .map(|d| model.probs(&state, d))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("inference error: {e}"))
        })
        .await
        .unwrap_or_else(|e| Err(format!("inference task failed: {e}")));
        let mut decision = resolve_route(&questions, probs, self.min_confidence);
        decision.device = self.model.device().label();
        decision.latency_ms = started.elapsed().as_millis() as u64;
        decision
    }
}

static LAYA: OnceLock<Arc<LayaModel>> = OnceLock::new();

/// Loads Laya once per process and reuses it across tasks. Failures are not
/// cached, so running `kode setup` mid-session takes effect on the next task.
/// ponytail: the first successful device choice sticks; changing
/// `router.device` needs a restart.
pub fn shared_laya(dir: &Path, pref: DevicePref) -> Result<Arc<LayaModel>, LocalError> {
    if let Some(model) = LAYA.get() {
        return Ok(model.clone());
    }
    let model = Arc::new(LayaModel::load(dir, pref)?);
    Ok(LAYA.get_or_init(|| model).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(temps: &[(&str, f32)]) -> LayaConfig {
        LayaConfig {
            max_len: 1024,
            head_max_len: 256,
            temperature: vec![1.5, 2.0, 3.0],
            temperature_by_options: temps.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            cls_id: 1,
            sep_id: 2,
            mask_id: 3,
            mask_token: "<mask>".to_string(),
        }
    }

    #[test]
    fn temperature_prefers_cardinality_bucket_then_qtype() {
        let c = cfg(&[("choice:3-5", 0.7)]);
        assert!((c.temperature_for(QType::Choice, 3) - 0.7).abs() < 1e-6);
        assert!((c.temperature_for(QType::Choice, 2) - 1.5).abs() < 1e-6);
        assert!((c.temperature_for(QType::Score, 3) - 2.0).abs() < 1e-6);
        assert!((c.temperature_for(QType::Noul, 2) - 3.0).abs() < 1e-6);
        assert!(
            (cfg(&[("choice:11+", 0.5)]).temperature_for(QType::Choice, 14) - 0.5).abs() < 1e-6
        );
    }

    #[test]
    fn config_load_reports_missing_file() {
        let dir = std::env::temp_dir().join(format!("kode-laya-missing-{}", std::process::id()));
        assert!(LayaConfig::load(&dir).is_err());
    }
}
