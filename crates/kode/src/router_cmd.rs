//! `kode router …`: team router training commands. Status/correct are
//! synchronous so the TUI can call them directly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use kode_core::config::KodeConfig;
use kode_local::calibrate::{
    Calibration, Evaluation, LogitSource, MIN_CALIBRATE, MIN_EVAL_DECISIONS, MIN_TRAIN,
    build_items, calibrate, evaluate,
};
use kode_local::dataset::{self, Correction, Labeled, Line, Split, split_of};
use kode_local::device::{DevicePref, init_runtime};
use kode_local::laya::{LayaModel, shared_laya};
use kode_local::manifest::{
    FileEntry, ManifestKind, Source, TeamModel, manifest_path, read_manifest, write_manifest,
};
use kode_local::models::{LAYA_DIR, LocalPaths, installed_runtime, sha256_file, verify_model_dir};
use kode_local::pins::{MODEL_FILES, MODELS_REVISION, ORT_VERSION};
use kode_local::route::{questions_version, route_questions};
use kode_local::temps::Temperatures;
use serde::{Deserialize, Serialize};

use crate::trainer::{HfJobsRunner, TRAIN_SCRIPT, TrainJob, TrainRunner, UvRunner};

pub fn candidates_dir(root: &Path) -> PathBuf {
    root.join(".kode").join("router").join("candidates")
}

pub fn parse_assignments(args: &[String]) -> Result<BTreeMap<String, String>, String> {
    if args.is_empty() {
        return Err("give at least one key=value (tier, effort, plan)".to_string());
    }
    let questions = route_questions();
    let mut out = BTreeMap::new();
    for arg in args {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("`{arg}` is not key=value"))?;
        let q = questions
            .iter()
            .find(|q| q.key == key)
            .ok_or_else(|| format!("unknown key `{key}` (tier, effort, plan)"))?;
        if !q.def.options.iter().any(|(o, _)| o == value) {
            let options: Vec<&str> = q.def.options.iter().map(|(o, _)| o.as_str()).collect();
            return Err(format!(
                "`{value}` is not a {key} option ({})",
                options.join(", ")
            ));
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

pub fn git_user_name_sync(root: &Path) -> String {
    // Only inside a work tree: outside one, `git config user.name` still
    // reports the machine-global name, which would wrongly filter `last`.
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true");
    if !inside {
        return String::new();
    }
    std::process::Command::new("git")
        .args(["config", "user.name"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Appends a correction for `target` (a record id, or `last` = the newest
/// record by the current git user). Returns a confirmation line.
pub fn correct(root: &Path, target: &str, args: &[String]) -> Result<String, String> {
    let set = parse_assignments(args)?;
    let path = dataset::dataset_path(root);
    let ds = dataset::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let author = git_user_name_sync(root);
    let record = if target == "last" {
        dataset::last_record(&ds, (!author.is_empty()).then_some(author.as_str()))
            .ok_or_else(|| "no router training record yet".to_string())?
    } else {
        ds.records
            .iter()
            .find(|r| r.id == target)
            .ok_or_else(|| format!("no record `{target}`"))?
    };
    let id = record.id.clone();
    dataset::append(
        &path,
        &Line::Correction(Correction {
            v: 1,
            id: dataset::new_id(),
            target: id.clone(),
            ts: dataset::now_secs(),
            author,
            set: set.clone(),
        }),
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let pairs: Vec<String> = set.iter().map(|(k, v)| format!("{k}={v}")).collect();
    Ok(format!("corrected {id}: {}", pairs.join(" ")))
}

fn argmax_option(key: &str, probs: &[f32]) -> Option<String> {
    let q = route_questions().into_iter().find(|q| q.key == key)?;
    let mut best = 0;
    for (i, p) in probs.iter().enumerate() {
        if *p > probs[best] {
            best = i;
        }
    }
    q.def.options.get(best).map(|(k, _)| format!("{key}={k}"))
}

pub fn describe_last(root: &Path) -> Vec<String> {
    let Ok(ds) = dataset::read(&dataset::dataset_path(root)) else {
        return vec!["router dataset unreadable".to_string()];
    };
    let Some(record) = dataset::last_record(&ds, None) else {
        return vec!["no router training records yet".to_string()];
    };
    let mut lines = vec![format!(
        "last router record {} — {}",
        record.id,
        record.state.lines().next().unwrap_or("")
    )];
    if let Some(t) = &record.teacher {
        let answers: Vec<String> = ["tier", "effort", "plan"]
            .iter()
            .filter_map(|k| t.probs.get(*k).and_then(|p| argmax_option(k, p)))
            .collect();
        lines.push(format!("teacher ({}): {}", t.model, answers.join(" · ")));
    }
    for c in ds.corrections.iter().filter(|c| c.target == record.id) {
        let pairs: Vec<String> = c.set.iter().map(|(k, v)| format!("{k}={v}")).collect();
        lines.push(format!("corrected by {}: {}", c.author, pairs.join(" ")));
    }
    lines.push("correct with: /router tier=… effort=… plan=…".to_string());
    lines
}

pub fn status_lines(root: &Path) -> Vec<String> {
    let path = dataset::dataset_path(root);
    let ds = match dataset::read(&path) {
        Ok(ds) => ds,
        Err(e) => return vec![format!("{}: {e}", path.display())],
    };
    let labels = dataset::labeled(&ds, &questions_version());
    let corrected = labels.examples.iter().filter(|e| e.corrected).count();
    let count = |s: Split| {
        labels
            .examples
            .iter()
            .filter(|e| split_of(&e.id) == s)
            .count()
    };
    let n = labels.examples.len();
    let team = match read_manifest(root) {
        Ok(None) => "none (pinned model)".to_string(),
        Ok(Some(m)) => match (m.kind, &m.source) {
            (ManifestKind::Calibration, _) => format!(
                "calibration of pinned {}",
                &m.base_revision[..m.base_revision.len().min(7)]
            ),
            (ManifestKind::Checkpoint, Some(Source::Hf { repo, revision })) => {
                format!("checkpoint {repo}@{}", &revision[..revision.len().min(7)])
            }
            (ManifestKind::Checkpoint, Some(Source::Path(p))) => {
                format!("checkpoint at {p}")
            }
            (ManifestKind::Checkpoint, Some(Source::RepoPath(p))) => {
                format!("checkpoint in repo at {p} (Git LFS)")
            }
            (ManifestKind::Checkpoint, None) => "checkpoint manifest without source".to_string(),
        },
        Err(e) => format!("manifest unreadable ({e})"),
    };
    vec![
        format!("dataset: {}", path.display()),
        format!(
            "records: {} · labeled: {n} (corrected {corrected}) · unlabeled: {} · other questions version: {} · corrupt: {} · duplicates: {}",
            ds.records.len(),
            labels.unlabeled,
            labels.other_version,
            ds.corrupt,
            ds.duplicates
        ),
        format!(
            "splits (labeled): train {} · calibration {} · eval {}",
            count(Split::Train),
            count(Split::Calib),
            count(Split::Eval)
        ),
        format!("thresholds: calibrate: {n}/{MIN_CALIBRATE} · train: {n}/{MIN_TRAIN}"),
        format!("team model: {team}"),
    ]
}

pub fn load_examples(root: &Path) -> anyhow::Result<Vec<Labeled>> {
    let path = dataset::dataset_path(root);
    let ds = dataset::read(&path).with_context(|| path.display().to_string())?;
    Ok(dataset::labeled(&ds, &questions_version()).examples)
}

pub async fn load_pinned_laya() -> anyhow::Result<Arc<LayaModel>> {
    let paths = LocalPaths::from_home().context("cannot resolve ~/.kode")?;
    let (_, dylib) = installed_runtime(&paths, ORT_VERSION)
        .context("onnx runtime not installed — run `kode setup`")?;
    init_runtime(&dylib)?;
    let dir = verify_model_dir(&paths, MODELS_REVISION, MODEL_FILES, LAYA_DIR)?;
    Ok(tokio::task::spawn_blocking(move || shared_laya(&dir, DevicePref::Cpu)).await??)
}

pub fn calibration_lines(c: &Calibration) -> Vec<String> {
    let m = |label: &str, x: &kode_local::calibrate::Metrics| {
        format!(
            "{label} accuracy {:.3} · ECE {:.3} (n={})",
            x.accuracy, x.ece, x.n
        )
    };
    let t = &c.temps.temperature;
    let mut lines = vec![
        format!("fit on {} decisions (train + calibration splits)", c.n_fit),
        m("before:", &c.before),
        m("after: ", &c.after),
        format!(
            "temperatures: choice {:.2} · score {:.2} · noul {:.2}",
            t.first().copied().unwrap_or(1.0),
            t.get(1).copied().unwrap_or(1.0),
            t.get(2).copied().unwrap_or(1.0)
        ),
    ];
    for (bucket, value) in &c.temps.temperature_by_options {
        lines.push(format!("  {bucket}: {value:.2}"));
    }
    if c.before.n < MIN_EVAL_DECISIONS {
        lines.push(format!(
            "note: the eval split has {} decisions (< {MIN_EVAL_DECISIONS}); these numbers are noisy",
            c.before.n
        ));
    }
    lines
}

pub fn calibration_manifest(c: &Calibration) -> anyhow::Result<TeamModel> {
    Ok(TeamModel {
        kind: ManifestKind::Calibration,
        base_revision: MODELS_REVISION.to_string(),
        questions_version: questions_version(),
        source: None,
        files: vec![],
        temps: c.temps.clone(),
        report: serde_json::to_value(c)?,
    })
}

pub async fn calibrate_cmd(root: &Path, write: bool) -> anyhow::Result<()> {
    let examples = load_examples(root)?;
    if examples.len() < MIN_CALIBRATE {
        anyhow::bail!(
            "calibration needs {MIN_CALIBRATE} labeled records; the dataset has {}",
            examples.len()
        );
    }
    if write
        && let Ok(Some(m)) = read_manifest(root)
        && m.kind == ManifestKind::Checkpoint
    {
        anyhow::bail!(
            "a team checkpoint is active (it was calibrated by `kode router train`); remove {} to calibrate the pinned model instead",
            manifest_path(root).display()
        );
    }
    let model = load_pinned_laya().await?;
    let current = model.temperatures().clone();
    let items =
        tokio::task::spawn_blocking(move || build_items(model.as_ref(), &examples)).await??;
    let result = calibrate(&items, &current, &[Split::Train, Split::Calib]);
    for line in calibration_lines(&result) {
        println!("{line}");
    }
    if write {
        write_manifest(root, &calibration_manifest(&result)?)?;
        println!(
            "wrote {} — commit it to share with the team",
            manifest_path(root).display()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainReport {
    pub id: String,
    /// `passed` | `rejected`.
    pub status: String,
    pub questions_version: String,
    pub n_train_rows: usize,
    pub evaluation: Evaluation,
}

/// One row per (train-split record, labeled question).
pub fn write_train_jsonl(examples: &[Labeled], path: &Path) -> anyhow::Result<usize> {
    use std::io::Write;
    let questions = route_questions();
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut rows = 0;
    for ex in examples.iter().filter(|e| split_of(&e.id) == Split::Train) {
        for q in &questions {
            if let Some(target) = ex.labels.get(q.key) {
                let row = serde_json::json!({
                    "id": ex.id, "state": ex.state, "question": q.def, "target": target,
                });
                writeln!(file, "{row}")?;
                rows += 1;
            }
        }
    }
    file.flush()?;
    Ok(rows)
}

pub type OpenCandidate = dyn Fn(&Path) -> anyhow::Result<Arc<dyn LogitSource>> + Send + Sync;

pub async fn run_training(
    root: &Path,
    examples: &[Labeled],
    runner: &dyn TrainRunner,
    base: Arc<dyn LogitSource>,
    base_temps: Temperatures,
    open_candidate: &OpenCandidate,
) -> anyhow::Result<TrainReport> {
    let id = dataset::new_id();
    let dir = candidates_dir(root).join(&id);
    std::fs::create_dir_all(&dir)?;
    let train_jsonl = dir.join("train.jsonl");
    let n_train_rows = write_train_jsonl(examples, &train_jsonl)?;
    let script = dir.join("train_laya.py");
    std::fs::write(&script, TRAIN_SCRIPT)?;
    let out_dir = dir.join("model");
    runner
        .run(&TrainJob {
            id: id.clone(),
            script,
            train_jsonl,
            out_dir: out_dir.clone(),
        })
        .await?;
    for f in ["model.onnx", "laya.json", "tokenizer.json"] {
        if !out_dir.join(f).is_file() {
            anyhow::bail!(
                "training finished but {f} is missing in {}",
                out_dir.display()
            );
        }
    }
    let candidate = open_candidate(&out_dir)?;
    let ex = examples.to_vec();
    let (base_items, cand_items) = tokio::task::spawn_blocking(move || {
        Ok::<_, kode_local::LocalError>((
            build_items(base.as_ref(), &ex)?,
            build_items(candidate.as_ref(), &ex)?,
        ))
    })
    .await??;
    let evaluation = evaluate(&base_items, &base_temps, &cand_items);
    let report = TrainReport {
        id: id.clone(),
        status: if evaluation.gate.passed {
            "passed"
        } else {
            "rejected"
        }
        .to_string(),
        questions_version: questions_version(),
        n_train_rows,
        evaluation,
    };
    std::fs::write(
        dir.join("report.json"),
        serde_json::to_string_pretty(&report)? + "\n",
    )?;
    Ok(report)
}

pub async fn train_cmd(root: &Path, config: &KodeConfig, remote: bool) -> anyhow::Result<()> {
    let examples = load_examples(root)?;
    if examples.len() < MIN_TRAIN {
        anyhow::bail!(
            "training needs {MIN_TRAIN} labeled records; the dataset has {}",
            examples.len()
        );
    }
    let runner: Box<dyn TrainRunner> = if remote {
        let repo = config.router.training.hf_dataset.trim();
        if repo.is_empty() {
            anyhow::bail!(
                "set [router.training] hf_dataset = \"<you>/<private-dataset>\" for --remote"
            );
        }
        Box::new(HfJobsRunner {
            dataset_repo: repo.to_string(),
        })
    } else {
        Box::new(UvRunner)
    };
    let base = load_pinned_laya().await?;
    let base_temps = match read_manifest(root) {
        Ok(Some(m))
            if m.kind == ManifestKind::Calibration && m.base_revision == MODELS_REVISION =>
        {
            m.temps
        }
        _ => base.temperatures().clone(),
    };
    let open: &OpenCandidate =
        &|dir: &Path| Ok(Arc::new(LayaModel::load(dir, DevicePref::Cpu)?) as Arc<dyn LogitSource>);
    let report = run_training(root, &examples, runner.as_ref(), base, base_temps, open).await?;
    let e = &report.evaluation;
    println!("candidate {} — {}", report.id, report.status);
    println!(
        "  base:      accuracy {:.3} · ECE {:.3} (n={})",
        e.base.accuracy, e.base.ece, e.base.n
    );
    println!(
        "  candidate: accuracy {:.3} · ECE {:.3} (n={})",
        e.candidate.accuracy, e.candidate.ece, e.candidate.n
    );
    println!("  gate: {}", e.gate.reason);
    if e.gate.passed {
        println!(
            "publish with: kode router publish {} --to hf:<repo>|path:<dir>|lfs:<dir>",
            report.id
        );
    }
    Ok(())
}

const CHECKPOINT_FILES: &[&str] = &["laya.json", "model.onnx", "tokenizer.json"];

#[derive(Debug, Clone, PartialEq)]
pub enum PublishTarget {
    Hf(String),
    Path(PathBuf),
    /// Repo-relative directory, tracked with Git LFS.
    Lfs(String),
}

pub fn parse_target(s: &str) -> Result<PublishTarget, String> {
    let (kind, value) = s
        .split_once(':')
        .ok_or_else(|| "use hf:<repo>, path:<dir>, or lfs:<repo-relative dir>".to_string())?;
    match kind {
        "hf" if value.contains('/') && !value.starts_with('/') => {
            Ok(PublishTarget::Hf(value.to_string()))
        }
        "hf" => Err(format!("`{value}` is not an HF repo id (owner/name)")),
        "path" if !value.is_empty() => Ok(PublishTarget::Path(PathBuf::from(value))),
        "lfs" => {
            let p = Path::new(value);
            if value.is_empty()
                || p.is_absolute()
                || value.starts_with('/')
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                Err(format!("`{value}` must be a path inside the repo"))
            } else {
                Ok(PublishTarget::Lfs(value.trim_end_matches('/').to_string()))
            }
        }
        _ => Err(format!("unknown target `{kind}` (hf, path, lfs)")),
    }
}

pub fn checkpoint_files(dir: &Path) -> anyhow::Result<Vec<FileEntry>> {
    CHECKPOINT_FILES
        .iter()
        .map(|name| {
            let path = dir.join(name);
            Ok(FileEntry {
                path: name.to_string(),
                sha256: sha256_file(&path).with_context(|| path.display().to_string())?,
                size: std::fs::metadata(&path)?.len(),
            })
        })
        .collect()
}

fn read_passed_report(root: &Path, candidate: &str) -> anyhow::Result<TrainReport> {
    let path = candidates_dir(root).join(candidate).join("report.json");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        anyhow::anyhow!("candidate {candidate} has no report (training did not finish)")
    })?;
    let report: TrainReport = serde_json::from_str(&text)?;
    if report.status != "passed" {
        anyhow::bail!(
            "candidate {candidate} was rejected by the gate: {}",
            report.evaluation.gate.reason
        );
    }
    Ok(report)
}

fn copy_checkpoint(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for name in CHECKPOINT_FILES {
        std::fs::copy(from.join(name), to.join(name))
            .with_context(|| format!("copying {name} to {}", to.display()))?;
    }
    Ok(())
}

fn checkpoint_manifest(
    report: &TrainReport,
    source: Source,
    files: Vec<FileEntry>,
) -> anyhow::Result<TeamModel> {
    Ok(TeamModel {
        kind: ManifestKind::Checkpoint,
        base_revision: String::new(),
        questions_version: report.questions_version.clone(),
        source: Some(source),
        files,
        temps: report.evaluation.candidate_temps.clone(),
        report: serde_json::to_value(&report.evaluation)?,
    })
}

/// `path:` and `lfs:` publishing (no network); writes the manifest.
pub fn publish_local(
    root: &Path,
    candidate: &str,
    target: &PublishTarget,
) -> anyhow::Result<TeamModel> {
    let report = read_passed_report(root, candidate)?;
    let model_dir = candidates_dir(root).join(candidate).join("model");
    let files = checkpoint_files(&model_dir)?;
    let source = match target {
        PublishTarget::Path(dir) => {
            let dest = dir.join(candidate);
            copy_checkpoint(&model_dir, &dest)?;
            Source::Path(dest.to_string_lossy().to_string())
        }
        PublishTarget::Lfs(rel) => {
            copy_checkpoint(&model_dir, &root.join(rel).join(candidate))?;
            Source::RepoPath(format!("{rel}/{candidate}"))
        }
        PublishTarget::Hf(_) => anyhow::bail!("hf: targets are published by publish_cmd"),
    };
    let manifest = checkpoint_manifest(&report, source, files)?;
    write_manifest(root, &manifest)?;
    Ok(manifest)
}

pub async fn publish_cmd(root: &Path, candidate: &str, to: &str) -> anyhow::Result<()> {
    let target = parse_target(to).map_err(anyhow::Error::msg)?;
    let manifest = match &target {
        PublishTarget::Hf(repo) => {
            let report = read_passed_report(root, candidate)?;
            let model_dir = candidates_dir(root).join(candidate).join("model");
            let files = checkpoint_files(&model_dir)?;
            crate::trainer::hf(&[
                "repos",
                "create",
                repo,
                "--type",
                "model",
                "--private",
                "--exist-ok",
            ])
            .await?;
            crate::trainer::hf(&[
                "upload",
                repo,
                &model_dir.to_string_lossy(),
                ".",
                "--type",
                "model",
                "--commit-message",
                &format!("kode router candidate {candidate}"),
            ])
            .await?;
            let info: serde_json::Value = serde_json::from_str(
                &crate::trainer::hf_output(&["models", "info", repo, "--format", "json"]).await?,
            )?;
            let revision = info["sha"]
                .as_str()
                .context("hf models info did not return a revision")?
                .to_string();
            let manifest = checkpoint_manifest(
                &report,
                Source::Hf {
                    repo: repo.clone(),
                    revision,
                },
                files,
            )?;
            write_manifest(root, &manifest)?;
            manifest
        }
        _ => publish_local(root, candidate, &target)?,
    };
    println!(
        "wrote {} — commit it to share with the team",
        manifest_path(root).display()
    );
    if let Some(Source::RepoPath(p)) = &manifest.source {
        println!(
            "track the weights with Git LFS: git lfs track \"{p}/**\" && git add .gitattributes {p}"
        );
    }
    if matches!(target, PublishTarget::Hf(_)) {
        println!(
            "the HF repo is private; teammates need `hf auth login` (or HF_TOKEN) before `kode setup`"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_local::calibrate::{Calibration, Metrics};
    use kode_local::dataset::{Line, Record, TaskSummary, TeacherLabel, append, dataset_path};
    use kode_local::error::LocalError;
    use kode_local::sequence::QuestionDef;
    use kode_local::temps::Temperatures;

    fn cal() -> Calibration {
        Calibration {
            temps: Temperatures {
                temperature: vec![1.4, 0.8, 1.0],
                temperature_by_options: Default::default(),
            },
            before: Metrics {
                n: 120,
                accuracy: 0.55,
                ece: 0.21,
            },
            after: Metrics {
                n: 120,
                accuracy: 0.55,
                ece: 0.06,
            },
            n_fit: 900,
        }
    }

    fn root(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kode-router-cmd-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed(root: &Path, id: &str, ts: u64) {
        append(
            &dataset_path(root),
            &Line::Record(Record {
                v: 1,
                id: id.to_string(),
                ts,
                author: String::new(),
                kode: "0.4.12".to_string(),
                questions_version: questions_version(),
                state: format!("task: {id}\nproject: rust\nuncommitted files: 0"),
                laya: Default::default(),
                teacher: Some(TeacherLabel {
                    model: "m".to_string(),
                    probs: [
                        ("tier".to_string(), vec![0.1, 0.2, 0.7]),
                        ("effort".to_string(), vec![0.6, 0.3, 0.1]),
                        ("plan".to_string(), vec![0.2, 0.8]),
                    ]
                    .into_iter()
                    .collect(),
                }),
                outcome: TaskSummary {
                    iterations: 1,
                    tool_calls: 1,
                    files_changed: 0,
                    mutated: false,
                    verification: "notneeded".to_string(),
                    repair_attempted: false,
                },
            }),
        )
        .unwrap();
    }

    #[test]
    fn parse_assignments_validates_keys_and_options() {
        let ok = parse_assignments(&["tier=heavy".to_string(), "plan=direct".to_string()]).unwrap();
        assert_eq!(ok["tier"], "heavy");
        assert!(parse_assignments(&[]).is_err());
        assert!(
            parse_assignments(&["tier".to_string()])
                .unwrap_err()
                .contains("key=value")
        );
        assert!(
            parse_assignments(&["color=red".to_string()])
                .unwrap_err()
                .contains("unknown key")
        );
        assert!(
            parse_assignments(&["tier=huge".to_string()])
                .unwrap_err()
                .contains("light, standard, heavy")
        );
    }

    #[test]
    fn correct_by_id_appends_a_correction() {
        let dir = root("correct-id");
        seed(&dir, "r1", 1);
        let msg = correct(&dir, "r1", &["tier=light".to_string()]).unwrap();
        assert_eq!(msg, "corrected r1: tier=light");
        let ds = kode_local::dataset::read(&dataset_path(&dir)).unwrap();
        assert_eq!(ds.corrections.len(), 1);
        assert_eq!(ds.corrections[0].target, "r1");
        assert!(
            correct(&dir, "nope", &["tier=light".to_string()])
                .unwrap_err()
                .contains("no record")
        );
    }

    #[test]
    fn correct_last_picks_the_newest_record() {
        let dir = root("correct-last");
        seed(&dir, "r1", 1);
        seed(&dir, "r2", 2);
        // No git repo here, so the author filter is empty and all records count.
        assert!(
            correct(&dir, "last", &["plan=plan".to_string()])
                .unwrap()
                .starts_with("corrected r2")
        );
    }

    #[test]
    fn status_reports_counts_and_thresholds() {
        let dir = root("status");
        seed(&dir, "r1", 1);
        let lines = status_lines(&dir).join("\n");
        assert!(lines.contains("records: 1"));
        assert!(lines.contains("labeled: 1"));
        assert!(lines.contains("calibrate: 1/50"));
        assert!(lines.contains("train: 1/300"));
        assert!(lines.contains("team model: none"));
    }

    #[test]
    fn describe_last_shows_teacher_answers() {
        let dir = root("describe");
        assert_eq!(
            describe_last(&dir),
            vec!["no router training records yet".to_string()]
        );
        seed(&dir, "r1", 1);
        let text = describe_last(&dir).join("\n");
        assert!(text.contains("r1"));
        assert!(text.contains("tier=heavy"));
        assert!(text.contains("plan=direct"));
    }

    #[test]
    fn calibration_lines_show_before_after_and_temperatures() {
        let text = calibration_lines(&cal()).join("\n");
        assert!(text.contains("fit on 900 decisions"));
        assert!(text.contains("before: accuracy 0.550 · ECE 0.210 (n=120)"));
        assert!(text.contains("after:  accuracy 0.550 · ECE 0.060 (n=120)"));
        assert!(text.contains("choice 1.40"));
    }

    #[test]
    fn calibration_manifest_targets_the_pinned_revision() {
        let m = calibration_manifest(&cal()).unwrap();
        assert_eq!(m.kind, ManifestKind::Calibration);
        assert_eq!(m.base_revision, kode_local::pins::MODELS_REVISION);
        assert_eq!(m.questions_version, questions_version());
        assert_eq!(m.temps.temperature, vec![1.4, 0.8, 1.0]);
        assert_eq!(m.report["after"]["ece"], serde_json::json!(0.06));
    }

    #[test]
    fn load_examples_counts_only_current_version() {
        let dir = root("examples");
        seed(&dir, "r1", 1);
        assert_eq!(load_examples(&dir).unwrap().len(), 1);
    }

    struct Fake {
        good: bool,
    }

    impl LogitSource for Fake {
        fn logits(&self, state: &str, q: &QuestionDef) -> Result<Vec<f32>, LocalError> {
            let k = q.options.len();
            if !self.good {
                return Ok(vec![0.0; k]);
            }
            let right = q
                .options
                .iter()
                .position(|(key, _)| state.contains(&format!("={key}")))
                .unwrap_or(0);
            Ok((0..k).map(|i| if i == right { 3.0 } else { 0.0 }).collect())
        }
        fn temperatures(&self) -> Temperatures {
            Temperatures::default()
        }
    }

    struct FakeRunner;

    #[async_trait::async_trait]
    impl TrainRunner for FakeRunner {
        async fn run(&self, job: &TrainJob) -> anyhow::Result<()> {
            assert!(job.train_jsonl.is_file());
            assert!(job.script.is_file());
            std::fs::create_dir_all(&job.out_dir)?;
            for f in ["model.onnx", "laya.json", "tokenizer.json"] {
                std::fs::write(job.out_dir.join(f), b"x")?;
            }
            Ok(())
        }
    }

    fn examples(n: usize) -> Vec<Labeled> {
        (0..n)
            .map(|i| Labeled {
                id: format!("01J{i:023}"),
                state: "task tier=heavy effort=low plan=plan".to_string(),
                labels: [
                    ("tier".to_string(), vec![0.0, 0.0, 1.0]),
                    ("effort".to_string(), vec![1.0, 0.0, 0.0]),
                    ("plan".to_string(), vec![1.0, 0.0]),
                ]
                .into_iter()
                .collect(),
                corrected: false,
            })
            .collect()
    }

    #[test]
    fn train_jsonl_holds_only_train_split_rows() {
        let dir = root("train-jsonl");
        let ex = examples(200);
        let path = dir.join("train.jsonl");
        let rows = write_train_jsonl(&ex, &path).unwrap();
        let train_records = ex
            .iter()
            .filter(|e| split_of(&e.id) == Split::Train)
            .count();
        assert_eq!(rows, train_records * 3);
        let first: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert!(first["question"]["options"].is_array());
        assert!(first["target"].is_array());
    }

    #[tokio::test]
    async fn run_training_gates_and_writes_a_report() {
        let dir = root("run-training");
        let ex = examples(600);
        let report = run_training(
            &dir,
            &ex,
            &FakeRunner,
            Arc::new(Fake { good: false }),
            Temperatures::default(),
            &|_: &Path| Ok(Arc::new(Fake { good: true }) as Arc<dyn LogitSource>),
        )
        .await
        .unwrap();
        assert_eq!(report.status, "passed", "{}", report.evaluation.gate.reason);
        let saved = candidates_dir(&dir).join(&report.id).join("report.json");
        let text = std::fs::read_to_string(saved).unwrap();
        assert!(text.contains("\"status\": \"passed\""));
    }

    #[tokio::test]
    async fn run_training_rejects_a_worse_candidate() {
        let dir = root("run-training-bad");
        let report = run_training(
            &dir,
            &examples(600),
            &FakeRunner,
            Arc::new(Fake { good: true }),
            Temperatures::default(),
            &|_: &Path| Ok(Arc::new(Fake { good: false }) as Arc<dyn LogitSource>),
        )
        .await
        .unwrap();
        assert_eq!(report.status, "rejected");
    }

    use kode_local::manifest::{ModelChoice, choose};

    fn passed_candidate(root: &Path, id: &str, status: &str) {
        let dir = candidates_dir(root).join(id);
        std::fs::create_dir_all(dir.join("model")).unwrap();
        for f in ["model.onnx", "laya.json", "tokenizer.json"] {
            std::fs::write(dir.join("model").join(f), format!("{f} bytes")).unwrap();
        }
        let report = TrainReport {
            id: id.to_string(),
            status: status.to_string(),
            questions_version: questions_version(),
            n_train_rows: 900,
            evaluation: kode_local::calibrate::Evaluation {
                base: Default::default(),
                candidate: Default::default(),
                candidate_temps: Temperatures {
                    temperature: vec![1.2, 1.0, 1.0],
                    temperature_by_options: Default::default(),
                },
                gate: kode_local::calibrate::Gate {
                    passed: status == "passed",
                    reason: "r".to_string(),
                },
                n_calib: 90,
            },
        };
        std::fs::write(
            dir.join("report.json"),
            serde_json::to_string(&report).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn parse_target_accepts_three_kinds_and_rejects_escapes() {
        assert_eq!(
            parse_target("hf:team/router").unwrap(),
            PublishTarget::Hf("team/router".to_string())
        );
        assert_eq!(
            parse_target("path:D:/shared").unwrap(),
            PublishTarget::Path(PathBuf::from("D:/shared"))
        );
        assert_eq!(
            parse_target("lfs:models/router").unwrap(),
            PublishTarget::Lfs("models/router".to_string())
        );
        assert!(parse_target("hf:noslash").is_err());
        assert!(parse_target("lfs:../outside").is_err());
        assert!(parse_target("lfs:/abs").is_err());
        assert!(parse_target("s3:bucket").is_err());
    }

    #[test]
    fn publish_to_path_writes_a_manifest_the_router_accepts() {
        let root = root("publish-path");
        let shared = root.join("shared");
        passed_candidate(&root, "01JCAND", "passed");
        let m = publish_local(&root, "01JCAND", &PublishTarget::Path(shared.clone())).unwrap();
        assert_eq!(
            m.source,
            Some(Source::Path(
                shared.join("01JCAND").to_string_lossy().to_string()
            ))
        );
        assert_eq!(m.files.len(), 3);
        let paths = LocalPaths {
            root: root.join("home"),
        };
        let (choice, note) = choose(
            &root,
            &paths,
            kode_local::pins::MODELS_REVISION,
            &questions_version(),
        );
        assert!(note.is_none(), "{note:?}");
        assert!(matches!(choice, ModelChoice::Team { .. }));
    }

    #[test]
    fn publish_to_lfs_uses_a_repo_relative_source() {
        let root = root("publish-lfs");
        passed_candidate(&root, "01JCAND", "passed");
        let m = publish_local(
            &root,
            "01JCAND",
            &PublishTarget::Lfs("models/router".to_string()),
        )
        .unwrap();
        assert_eq!(
            m.source,
            Some(Source::RepoPath("models/router/01JCAND".to_string()))
        );
        assert!(root.join("models/router/01JCAND/model.onnx").is_file());
    }

    #[test]
    fn publish_refuses_rejected_candidates() {
        let root = root("publish-rejected");
        passed_candidate(&root, "01JBAD", "rejected");
        let err = publish_local(&root, "01JBAD", &PublishTarget::Path(root.join("s"))).unwrap_err();
        assert!(err.to_string().contains("rejected"));
    }

    #[test]
    fn publish_refuses_candidate_without_report() {
        let root = root("publish-noreport");
        std::fs::create_dir_all(candidates_dir(&root).join("01JHALF/model")).unwrap();
        let err =
            publish_local(&root, "01JHALF", &PublishTarget::Path(root.join("s"))).unwrap_err();
        assert!(err.to_string().contains("no report"));
    }
}
