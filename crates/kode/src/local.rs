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
use kode_local::rerank::shared_reranker;
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

    // Model verification/loading belongs inside the context rerank timeout,
    // rather than delaying every route before context candidates exist.
    let reranker = cfg.rerank.then(|| {
        Arc::new(LocalReranker {
            paths: Arc::new(paths),
            pref,
            allow_cpu: cfg.rerank_on_cpu,
        }) as Arc<dyn ContextReranker>
    });

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
    paths: Arc<LocalPaths>,
    pref: DevicePref,
    allow_cpu: bool,
}

#[async_trait::async_trait]
impl ContextReranker for LocalReranker {
    async fn rerank(&self, query: &str, docs: &[String]) -> RerankOutcome {
        if self.pref == DevicePref::Cpu && !self.allow_cpu {
            return RerankOutcome::Skipped(
                "no gpu (set router.rerank_on_cpu = true to rerank on cpu)".to_string(),
            );
        }
        if docs.is_empty() {
            return RerankOutcome::Scored(Vec::new());
        }
        let paths = self.paths.clone();
        let pref = self.pref;
        let allow_cpu = self.allow_cpu;
        let query = query.to_string();
        let docs = docs.to_vec();
        match tokio::task::spawn_blocking(move || {
            let run = || {
                let dir = verify_model_dir(&paths, MODELS_REVISION, MODEL_FILES, RERANKER_DIR)?;
                let model = shared_reranker(&dir, pref)?;
                let gpu = model.device().is_gpu();
                if !gpu && !allow_cpu {
                    return Ok(RerankOutcome::Skipped(
                        "no gpu (set router.rerank_on_cpu = true to rerank on cpu)".to_string(),
                    ));
                }
                let limit = if gpu {
                    docs.len()
                } else {
                    docs.len().min(CPU_RERANK_CAP)
                };
                let mut scores = model.score_all(&query, &docs[..limit])?;
                scores.resize(docs.len(), f32::NEG_INFINITY);
                Ok::<_, LocalError>(RerankOutcome::Scored(scores))
            };
            run().unwrap_or_else(|e| RerankOutcome::Failed(reason_of(&e)))
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => RerankOutcome::Failed(format!("rerank task failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "manual cold/warm route measurement; requires installed local models"]
    async fn local_stack_latency() {
        use kode_local::route::RouteInput;
        let cfg = RouterConfig {
            device: "cpu".to_string(),
            min_confidence: 0.0,
            ..Default::default()
        };
        let root = std::env::temp_dir();
        let started = std::time::Instant::now();
        let stack = load(&cfg, &root).await;
        println!("cold local stack: {}ms", started.elapsed().as_millis());
        let input = RouteInput {
            task: "explain the answer function".to_string(),
            project: "rust".to_string(),
            changed_files: 0,
        };
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let warm = load(&cfg, &root).await;
            let route = warm.router.route(&input).await;
            assert!(
                route
                    .answers
                    .iter()
                    .all(|a| a.source == kode_core::event::RouteSource::Laya)
            );
            println!(
                "warm local stack + route: {}ms",
                started.elapsed().as_millis()
            );
        }
        assert!(stack.reranker.is_some());
    }

    #[tokio::test]
    async fn disabled_cpu_rerank_skips_without_loading_missing_models() {
        let reranker = LocalReranker {
            paths: Arc::new(LocalPaths {
                root: std::path::PathBuf::from("missing-qa-models"),
            }),
            pref: DevicePref::Cpu,
            allow_cpu: false,
        };
        assert!(
            matches!(reranker.rerank("query", &["doc".to_string()]).await,
            RerankOutcome::Skipped(reason) if reason.contains("no gpu"))
        );
    }

    #[tokio::test]
    async fn empty_candidates_do_not_load_missing_models() {
        let reranker = LocalReranker {
            paths: Arc::new(LocalPaths {
                root: std::path::PathBuf::from("missing-qa-models"),
            }),
            pref: DevicePref::Cpu,
            allow_cpu: true,
        };
        assert!(matches!(reranker.rerank("query", &[]).await,
            RerankOutcome::Scored(scores) if scores.is_empty()));
        assert!(matches!(
            reranker.rerank("query", &["doc".to_string()]).await,
            RerankOutcome::Failed(_)
        ));
    }
}
