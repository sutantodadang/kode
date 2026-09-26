//! Laya multilingual decision model on ONNX Runtime (batch size 1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
use crate::sequence::{QuestionDef, SeqTokenizer, Sequence, build_sequence, render_options};
use crate::temps::Temperatures;

#[derive(Debug, Clone, Deserialize)]
pub struct LayaConfig {
    pub max_len: usize,
    pub head_max_len: usize,
    #[serde(flatten)]
    pub temps: Temperatures,
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

    pub fn temperatures(&self) -> &Temperatures {
        &self.cfg.temps
    }

    pub fn logits(&self, state: &str, q: &QuestionDef) -> Result<Vec<f32>, LocalError> {
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
        let t = self.cfg.temps.get(q.qtype, logits.len());
        Ok(softmax_with_temperature(&logits, t))
    }
}

pub struct LayaRouter {
    pub model: Arc<LayaModel>,
    pub min_confidence: f32,
    /// Overrides the model's own `laya.json` temperatures (team calibration).
    pub temps: Option<Temperatures>,
    /// Which model this is, for the decision label: `pinned`, `pinned+cal`,
    /// `team@1a2b3c4`.
    pub label: String,
}

#[async_trait]
impl TaskRouter for LayaRouter {
    async fn route(&self, input: &RouteInput) -> RouteDecision {
        let started = Instant::now();
        let questions = route_questions();
        let defs: Vec<QuestionDef> = questions.iter().map(|q| q.def.clone()).collect();
        let model = self.model.clone();
        let temps = self
            .temps
            .clone()
            .unwrap_or_else(|| self.model.temperatures().clone());
        let state = input.state();
        let probs = tokio::task::spawn_blocking(move || {
            defs.iter()
                .map(|d| {
                    model
                        .logits(&state, d)
                        .map(|z| softmax_with_temperature(&z, temps.get(d.qtype, z.len())))
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("inference error: {e}"))
        })
        .await
        .unwrap_or_else(|e| Err(format!("inference task failed: {e}")));
        let mut decision = resolve_route(&questions, probs, self.min_confidence);
        decision.device = format!("laya {} · {}", self.label, self.model.device().label());
        decision.latency_ms = started.elapsed().as_millis() as u64;
        decision
    }
}

static LAYA: OnceLock<Mutex<HashMap<PathBuf, Arc<LayaModel>>>> = OnceLock::new();

/// Loads a Laya model once per directory per process. Failures are not
/// cached, so `kode setup` mid-session takes effect on the next task.
/// ponytail: two tasks racing on a cold directory may both load it; the
/// second insert wins, which is harmless.
pub fn shared_laya(dir: &Path, pref: DevicePref) -> Result<Arc<LayaModel>, LocalError> {
    let cache = LAYA.get_or_init(Default::default);
    if let Some(model) = cache.lock().map_err(|_| LocalError::Poisoned)?.get(dir) {
        return Ok(model.clone());
    }
    let model = Arc::new(LayaModel::load(dir, pref)?);
    cache
        .lock()
        .map_err(|_| LocalError::Poisoned)?
        .insert(dir.to_path_buf(), model.clone());
    Ok(model)
}

impl crate::calibrate::LogitSource for LayaModel {
    fn logits(&self, state: &str, q: &QuestionDef) -> Result<Vec<f32>, LocalError> {
        LayaModel::logits(self, state, q)
    }
    fn temperatures(&self) -> Temperatures {
        self.cfg.temps.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn laya_json_temperatures_flatten_into_config() {
        use crate::sequence::QType;
        let dir = std::env::temp_dir().join(format!("kode-laya-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("laya.json"),
            r#"{"max_len":1024,"head_max_len":256,"temperature":[1.2,1.0,1.0],"temperature_by_options":{},"cls_id":2,"sep_id":1,"mask_id":4,"mask_token":"<mask>"}"#,
        )
        .unwrap();
        let cfg = LayaConfig::load(&dir).unwrap();
        assert!((cfg.temps.get(QType::Choice, 3) - 1.2).abs() < 1e-6);
    }

    #[test]
    fn config_load_reports_missing_file() {
        let dir = std::env::temp_dir().join(format!("kode-laya-missing-{}", std::process::id()));
        assert!(LayaConfig::load(&dir).is_err());
    }
}
