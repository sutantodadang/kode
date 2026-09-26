//! Loads the local router stack (Laya + Qwen3 reranker) from verified
//! artefacts under `~/.kode`, degrading to `StaticRouter` with a reason.

use std::path::Path;
use std::sync::Arc;

use kode_context::{ContextReranker, RerankOutcome};
use kode_core::config::RouterConfig;
use kode_local::LocalError;
use kode_local::device::{DevicePref, init_runtime};
use kode_local::laya::{LayaModel, LayaRouter, shared_laya};
use kode_local::manifest::{ModelChoice, choose};
use kode_local::models::{LAYA_DIR, LocalPaths, RERANKER_DIR, installed_runtime, verify_model_dir};
use kode_local::pins::{MODEL_FILES, MODELS_REVISION, ORT_VERSION};
use kode_local::rerank::{QwenReranker, shared_reranker};
use kode_local::route::{StaticRouter, TaskRouter, questions_version};

/// ponytail: CPU reranks only the first 10 candidates; the rest sort last.
const CPU_RERANK_CAP: usize = 10;

/// Laya runs one short sequence per question: on an RTX 4070 SUPER it took
/// ~190 ms for three questions on CPU vs ~280 ms on DirectML (GPU launch
/// overhead dominates). `router.device` therefore applies to the reranker.
const LAYA_DEVICE: DevicePref = DevicePref::Cpu;

pub struct LocalStack {
    pub router: Arc<dyn TaskRouter>,
    pub reranker: Option<Arc<dyn ContextReranker>>,
    pub notes: Vec<String>,
}

fn static_stack(reason: impl Into<String>, notes: Vec<String>) -> LocalStack {
    LocalStack {
        router: Arc::new(StaticRouter {
            reason: reason.into(),
        }),
        reranker: None,
        notes,
    }
}

fn reason_of(e: &LocalError) -> String {
    match e {
        LocalError::Missing(_) | LocalError::NotPinned => {
            "models not installed — run `kode setup`".to_string()
        }
        LocalError::ChecksumMismatch(p) => format!("checksum mismatch: {}", p.display()),
        other => other.to_string(),
    }
}

pub async fn load(cfg: &RouterConfig, root: &Path) -> LocalStack {
    let mut notes = Vec::new();
    let pref = DevicePref::parse(&cfg.device).unwrap_or_else(|| {
        notes.push(format!(
            "unknown router.device '{}' — using auto",
            cfg.device
        ));
        DevicePref::Auto
    });
    let Some(paths) = LocalPaths::from_home() else {
        return static_stack("no home directory", notes);
    };
    let Some((_, dylib)) = installed_runtime(&paths, ORT_VERSION) else {
        return static_stack("onnx runtime not installed — run `kode setup`", notes);
    };
    if let Err(e) = init_runtime(&dylib) {
        return static_stack(format!("onnx runtime failed to load: {e}"), notes);
    }

    let (choice, note) = choose(root, &paths, MODELS_REVISION, &questions_version());
    notes.extend(note);
    let (laya, temps, label) = match choice {
        ModelChoice::Team { dir, temps, label } => match load_laya(dir).await {
            Ok(model) => (model, Some(temps), label),
            Err(e) => {
                notes.push(format!(
                    "router: team model failed to load ({e}) — using pinned"
                ));
                match load_pinned(&paths).await {
                    Ok(model) => (model, None, "pinned".to_string()),
                    Err(reason) => return static_stack(reason, notes),
                }
            }
        },
        ModelChoice::Pinned { temps, label } => match load_pinned(&paths).await {
            Ok(model) => (model, temps, label.to_string()),
            Err(reason) => return static_stack(reason, notes),
        },
    };
    let router: Arc<dyn TaskRouter> = Arc::new(LayaRouter {
        model: laya,
        min_confidence: cfg.min_confidence,
        temps,
        label,
    });

    let reranker = if cfg.rerank {
        match verify_model_dir(&paths, MODELS_REVISION, MODEL_FILES, RERANKER_DIR) {
            Ok(dir) => match tokio::task::spawn_blocking(move || shared_reranker(&dir, pref)).await
            {
                Ok(Ok(model)) => Some(Arc::new(LocalReranker {
                    model,
                    allow_cpu: cfg.rerank_on_cpu,
                }) as Arc<dyn ContextReranker>),
                Ok(Err(e)) => {
                    notes.push(format!("reranker unavailable: {}", reason_of(&e)));
                    None
                }
                Err(e) => {
                    notes.push(format!("reranker unavailable: load task failed: {e}"));
                    None
                }
            },
            Err(e) => {
                notes.push(format!("reranker unavailable: {}", reason_of(&e)));
                None
            }
        }
    } else {
        None
    };

    LocalStack {
        router,
        reranker,
        notes,
    }
}

async fn load_laya(dir: std::path::PathBuf) -> Result<Arc<LayaModel>, String> {
    match tokio::task::spawn_blocking(move || shared_laya(&dir, LAYA_DEVICE)).await {
        Ok(Ok(model)) => Ok(model),
        Ok(Err(e)) => Err(reason_of(&e)),
        Err(e) => Err(format!("laya load task failed: {e}")),
    }
}

async fn load_pinned(paths: &LocalPaths) -> Result<Arc<LayaModel>, String> {
    let dir = verify_model_dir(paths, MODELS_REVISION, MODEL_FILES, LAYA_DIR)
        .map_err(|e| reason_of(&e))?;
    load_laya(dir).await
}

pub struct LocalReranker {
    model: Arc<QwenReranker>,
    allow_cpu: bool,
}

#[async_trait::async_trait]
impl ContextReranker for LocalReranker {
    async fn rerank(&self, query: &str, docs: &[String]) -> RerankOutcome {
        let gpu = self.model.device().is_gpu();
        if !gpu && !self.allow_cpu {
            return RerankOutcome::Skipped(
                "no gpu (set router.rerank_on_cpu = true to rerank on cpu)".to_string(),
            );
        }
        let limit = if gpu {
            docs.len()
        } else {
            docs.len().min(CPU_RERANK_CAP)
        };
        let model = self.model.clone();
        let query = query.to_string();
        let head: Vec<String> = docs[..limit].to_vec();
        let total = docs.len();
        match tokio::task::spawn_blocking(move || model.score_all(&query, &head)).await {
            Ok(Ok(mut scores)) => {
                scores.resize(total, f32::NEG_INFINITY);
                RerankOutcome::Scored(scores)
            }
            Ok(Err(e)) => RerankOutcome::Failed(e.to_string()),
            Err(e) => RerankOutcome::Failed(format!("rerank task failed: {e}")),
        }
    }
}
