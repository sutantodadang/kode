//! `.kode/router/dataset.jsonl`: the team's append-only, committed router
//! training data — teacher-labeled task records plus user corrections.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::route::route_questions;

const UNION_MERGE_RULE: &str = "dataset.jsonl merge=union";

pub fn dataset_path(root: &Path) -> PathBuf {
    root.join(".kode").join("router").join("dataset.jsonl")
}

pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn v1() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub iterations: u32,
    pub tool_calls: u32,
    pub files_changed: usize,
    pub mutated: bool,
    pub verification: String,
    pub repair_attempted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeacherLabel {
    pub model: String,
    /// Question key → probabilities in `route_questions()` option order.
    pub probs: BTreeMap<String, Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default = "v1")]
    pub v: u32,
    pub id: String,
    pub ts: u64,
    pub author: String,
    pub kode: String,
    pub questions_version: String,
    pub state: String,
    #[serde(default)]
    pub laya: BTreeMap<String, Vec<f32>>,
    pub teacher: Option<TeacherLabel>,
    pub outcome: TaskSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    #[serde(default = "v1")]
    pub v: u32,
    pub id: String,
    #[serde(rename = "ref")]
    pub target: String,
    pub ts: u64,
    pub author: String,
    pub set: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Line {
    Record(Record),
    Correction(Correction),
}

/// `git merge` keeps both sides' lines of an append-only file instead of
/// conflicting on its tail.
fn ensure_union_merge(dir: &Path) -> std::io::Result<()> {
    let path = dir.join(".gitattributes");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == UNION_MERGE_RULE) {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{UNION_MERGE_RULE}")
}

pub fn append(path: &Path, line: &Line) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        ensure_union_merge(parent)?;
    }
    let json = serde_json::to_string(line).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{json}")
}

#[derive(Debug, Default)]
pub struct Dataset {
    pub records: Vec<Record>,
    pub corrections: Vec<Correction>,
    pub corrupt: usize,
    pub duplicates: usize,
}

pub fn read(path: &Path) -> std::io::Result<Dataset> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Dataset::default()),
        Err(e) => return Err(e),
    };
    let mut ds = Dataset::default();
    let mut seen = HashSet::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Line>(line) {
            Ok(parsed) => {
                let id = match &parsed {
                    Line::Record(r) => r.id.clone(),
                    Line::Correction(c) => c.id.clone(),
                };
                if !seen.insert(id) {
                    ds.duplicates += 1;
                    continue;
                }
                match parsed {
                    Line::Record(r) => ds.records.push(r),
                    Line::Correction(c) => ds.corrections.push(c),
                }
            }
            Err(_) => ds.corrupt += 1,
        }
    }
    Ok(ds)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Labeled {
    pub id: String,
    pub state: String,
    /// Question key → target distribution (option order).
    pub labels: BTreeMap<String, Vec<f32>>,
    pub corrected: bool,
}

#[derive(Debug, Default)]
pub struct Labels {
    pub examples: Vec<Labeled>,
    pub other_version: usize,
    pub unlabeled: usize,
}

fn onehot(k: usize, i: usize) -> Vec<f32> {
    (0..k).map(|j| if j == i { 1.0 } else { 0.0 }).collect()
}

/// Per key: the latest correction (one-hot) beats the teacher's
/// distribution. Records of another questions version are counted, not used.
pub fn labeled(ds: &Dataset, version: &str) -> Labels {
    let questions = route_questions();
    let mut out = Labels::default();
    for r in &ds.records {
        if r.questions_version != version {
            out.other_version += 1;
            continue;
        }
        let mut labels = BTreeMap::new();
        if let Some(teacher) = &r.teacher {
            for q in &questions {
                if let Some(p) = teacher.probs.get(q.key)
                    && p.len() == q.def.options.len()
                {
                    let sum: f32 = p.iter().sum();
                    if sum.is_finite() && sum > 0.0 && p.iter().all(|x| x.is_finite() && *x >= 0.0)
                    {
                        labels.insert(q.key.to_string(), p.iter().map(|x| x / sum).collect());
                    }
                }
            }
        }
        let mut corrections: Vec<&Correction> =
            ds.corrections.iter().filter(|c| c.target == r.id).collect();
        corrections.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        let mut corrected = false;
        for c in corrections {
            for (key, value) in &c.set {
                if let Some(q) = questions.iter().find(|q| q.key == key)
                    && let Some(i) = q.def.options.iter().position(|(k, _)| k == value)
                {
                    labels.insert(key.clone(), onehot(q.def.options.len(), i));
                    corrected = true;
                }
            }
        }
        if labels.is_empty() {
            out.unlabeled += 1;
            continue;
        }
        out.examples.push(Labeled {
            id: r.id.clone(),
            state: r.state.clone(),
            labels,
            corrected,
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Split {
    Eval,
    Calib,
    Train,
}

/// Stable as the dataset grows: an eval record never becomes a train record.
pub fn split_of(id: &str) -> Split {
    match Sha256::digest(id.as_bytes())[0] % 10 {
        0 => Split::Eval,
        1 => Split::Calib,
        _ => Split::Train,
    }
}

pub fn last_record<'a>(ds: &'a Dataset, author: Option<&str>) -> Option<&'a Record> {
    ds.records
        .iter()
        .filter(|r| author.is_none_or(|a| r.author == a))
        .max_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("kode-dataset-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn summary() -> TaskSummary {
        TaskSummary {
            iterations: 3,
            tool_calls: 5,
            files_changed: 1,
            mutated: true,
            verification: "verified".to_string(),
            repair_attempted: false,
        }
    }

    fn record(id: &str, ts: u64, version: &str, teacher: Option<[Vec<f32>; 3]>) -> Record {
        Record {
            v: 1,
            id: id.to_string(),
            ts,
            author: "ana".to_string(),
            kode: "0.4.12".to_string(),
            questions_version: version.to_string(),
            state: format!("task: {id}"),
            laya: BTreeMap::new(),
            teacher: teacher.map(|[t, e, p]| TeacherLabel {
                model: "m".to_string(),
                probs: [
                    ("tier".to_string(), t),
                    ("effort".to_string(), e),
                    ("plan".to_string(), p),
                ]
                .into_iter()
                .collect(),
            }),
            outcome: summary(),
        }
    }

    fn correction(id: &str, target: &str, ts: u64, set: &[(&str, &str)]) -> Correction {
        Correction {
            v: 1,
            id: id.to_string(),
            target: target.to_string(),
            ts,
            author: "ana".to_string(),
            set: set
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    const V: &str = "sha256:current";

    fn teacher() -> Option<[Vec<f32>; 3]> {
        Some([vec![0.2, 0.2, 0.6], vec![0.5, 0.3, 0.2], vec![0.1, 0.9]])
    }

    #[test]
    fn append_and_read_round_trip_with_ref_field() {
        let root = temp_root("roundtrip");
        let path = dataset_path(&root);
        append(&path, &Line::Record(record("r1", 1, V, teacher()))).unwrap();
        append(
            &path,
            &Line::Correction(correction("c1", "r1", 2, &[("tier", "heavy")])),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""type":"record""#));
        assert!(text.contains(r#""ref":"r1""#));
        let ds = read(&path).unwrap();
        assert_eq!(ds.records.len(), 1);
        assert_eq!(ds.corrections.len(), 1);
        assert_eq!(ds.corrections[0].target, "r1");
    }

    #[test]
    fn append_writes_union_merge_gitattributes() {
        let root = temp_root("gitattributes");
        append(
            &dataset_path(&root),
            &Line::Record(record("r1", 1, V, teacher())),
        )
        .unwrap();
        let attrs = std::fs::read_to_string(root.join(".kode/router/.gitattributes")).unwrap();
        assert!(attrs.contains("dataset.jsonl merge=union"));
        // Idempotent: a second append does not duplicate the rule.
        append(
            &dataset_path(&root),
            &Line::Record(record("r2", 2, V, teacher())),
        )
        .unwrap();
        let attrs = std::fs::read_to_string(root.join(".kode/router/.gitattributes")).unwrap();
        assert_eq!(attrs.matches("merge=union").count(), 1);
    }

    #[test]
    fn read_counts_corrupt_and_duplicate_lines() {
        let root = temp_root("corrupt");
        let path = dataset_path(&root);
        append(&path, &Line::Record(record("r1", 1, V, teacher()))).unwrap();
        append(&path, &Line::Record(record("r1", 1, V, teacher()))).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json\n\n");
        std::fs::write(&path, text).unwrap();
        let ds = read(&path).unwrap();
        assert_eq!(ds.records.len(), 1);
        assert_eq!(ds.duplicates, 1);
        assert_eq!(ds.corrupt, 1);
    }

    #[test]
    fn missing_file_reads_empty() {
        let ds = read(&temp_root("missing").join("nope.jsonl")).unwrap();
        assert!(ds.records.is_empty() && ds.corrupt == 0);
    }

    #[test]
    fn corrections_beat_teacher_and_latest_wins() {
        let ds = Dataset {
            records: vec![record("r1", 1, V, teacher())],
            corrections: vec![
                correction("c1", "r1", 5, &[("tier", "light")]),
                correction("c2", "r1", 9, &[("tier", "heavy"), ("plan", "direct")]),
                correction("c3", "r1", 7, &[("tier", "standard")]),
            ],
            ..Default::default()
        };
        let l = labeled(&ds, V);
        let ex = &l.examples[0];
        assert!(ex.corrected);
        assert_eq!(ex.labels["tier"], vec![0.0, 0.0, 1.0]);
        assert_eq!(ex.labels["plan"], vec![0.0, 1.0]);
        assert_eq!(ex.labels["effort"], vec![0.5, 0.3, 0.2]);
    }

    #[test]
    fn unknown_correction_values_are_ignored() {
        let ds = Dataset {
            records: vec![record("r1", 1, V, teacher())],
            corrections: vec![correction(
                "c1",
                "r1",
                5,
                &[("tier", "enormous"), ("color", "red")],
            )],
            ..Default::default()
        };
        let ex = &labeled(&ds, V).examples[0];
        assert!(!ex.corrected);
        assert_eq!(ex.labels["tier"], vec![0.2, 0.2, 0.6]);
    }

    #[test]
    fn other_versions_and_unlabeled_records_are_counted_not_used() {
        let ds = Dataset {
            records: vec![
                record("r1", 1, V, teacher()),
                record("r2", 2, "sha256:old", teacher()),
                record("r3", 3, V, None),
            ],
            ..Default::default()
        };
        let l = labeled(&ds, V);
        assert_eq!(l.examples.len(), 1);
        assert_eq!(l.other_version, 1);
        assert_eq!(l.unlabeled, 1);
    }

    #[test]
    fn teacher_probs_are_renormalised() {
        let ds = Dataset {
            records: vec![record(
                "r1",
                1,
                V,
                Some([vec![1.0, 1.0, 2.0], vec![0.5, 0.3, 0.2], vec![0.1, 0.9]]),
            )],
            ..Default::default()
        };
        assert_eq!(
            labeled(&ds, V).examples[0].labels["tier"],
            vec![0.25, 0.25, 0.5]
        );
    }

    #[test]
    fn invalid_teacher_distributions_are_not_training_labels() {
        let ds = Dataset {
            records: vec![record(
                "r1",
                1,
                V,
                Some([
                    vec![-1.0, 1.0, 1.0],
                    vec![f32::MAX, f32::MAX, 0.0],
                    vec![0.0, 0.0],
                ]),
            )],
            ..Default::default()
        };
        let labels = labeled(&ds, V);
        assert!(labels.examples.is_empty());
        assert_eq!(labels.unlabeled, 1);
    }

    #[test]
    fn splits_are_stable_and_roughly_ten_percent() {
        let ids: Vec<String> = (0..2000).map(|i| format!("01J{i:023}")).collect();
        let eval = ids.iter().filter(|id| split_of(id) == Split::Eval).count();
        let calib = ids.iter().filter(|id| split_of(id) == Split::Calib).count();
        assert!((140..=260).contains(&eval), "eval {eval}");
        assert!((140..=260).contains(&calib), "calib {calib}");
        for id in &ids {
            assert_eq!(split_of(id), split_of(id));
        }
    }

    #[test]
    fn last_record_filters_by_author() {
        let mut other = record("r2", 9, V, teacher());
        other.author = "bo".to_string();
        let ds = Dataset {
            records: vec![record("r1", 5, V, teacher()), other],
            ..Default::default()
        };
        assert_eq!(last_record(&ds, Some("ana")).unwrap().id, "r1");
        assert_eq!(last_record(&ds, None).unwrap().id, "r2");
        assert!(last_record(&ds, Some("cy")).is_none());
    }
}
