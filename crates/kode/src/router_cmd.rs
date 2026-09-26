//! `kode router …`: team router training commands. Status/correct are
//! synchronous so the TUI can call them directly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kode_local::calibrate::{MIN_CALIBRATE, MIN_TRAIN};
use kode_local::dataset::{self, Correction, Line, Split, split_of};
use kode_local::manifest::{ManifestKind, Source, read_manifest};
use kode_local::route::{questions_version, route_questions};

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

#[cfg(test)]
mod tests {
    use super::*;
    use kode_local::dataset::{Line, Record, TaskSummary, TeacherLabel, append, dataset_path};

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
}
