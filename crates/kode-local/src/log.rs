//! `.kode/router-log.jsonl`: one line per routed task — the fine-tune data
//! for a future Laya checkpoint. Writing it must never affect the task.

use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use kode_core::event::RouteSource;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::route::RouteDecision;

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// `completed` | `cancelled` | `failed`.
    pub status: String,
    pub verification: String,
    pub iterations: u32,
    pub tool_calls: u32,
}

#[derive(Debug, Serialize)]
pub struct LoggedAnswer {
    pub key: String,
    pub value: String,
    pub source: String,
    pub confidence: Option<f32>,
    pub probs: Vec<f32>,
}

#[derive(Debug, Serialize)]
pub struct RouterLogLine {
    pub ts: u64,
    pub task: String,
    pub device: String,
    pub latency_ms: u64,
    pub answers: Vec<LoggedAnswer>,
    pub outcome: Outcome,
}

pub fn task_key(task: &str, log_text: bool) -> String {
    if log_text {
        return task.to_string();
    }
    let digest = Sha256::digest(task.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

pub fn log_line(
    decision: &RouteDecision,
    task: &str,
    log_text: bool,
    outcome: Outcome,
) -> RouterLogLine {
    let answers = decision
        .answers
        .iter()
        .map(|a| LoggedAnswer {
            key: a.key.clone(),
            value: a.value.clone(),
            source: match &a.source {
                RouteSource::Laya => "laya".to_string(),
                RouteSource::Static(reason) => format!("static: {reason}"),
            },
            confidence: a.confidence,
            probs: decision
                .probs
                .iter()
                .find(|(k, _)| *k == a.key)
                .map(|(_, p)| p.clone())
                .unwrap_or_default(),
        })
        .collect();
    RouterLogLine {
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        task: task_key(task, log_text),
        device: decision.device.clone(),
        latency_ms: decision.latency_ms,
        answers,
        outcome,
    }
}

pub fn append(path: &Path, line: &RouterLogLine) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(line).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{json}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{resolve_route, route_questions};

    fn outcome() -> Outcome {
        Outcome {
            status: "completed".to_string(),
            verification: "verified".to_string(),
            iterations: 3,
            tool_calls: 7,
        }
    }

    fn temp(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kode-router-log-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn task_is_hashed_unless_log_text() {
        let hashed = task_key("secret task", false);
        assert!(hashed.starts_with("sha256:"));
        assert!(!hashed.contains("secret"));
        assert_eq!(task_key("secret task", true), "secret task");
    }

    #[test]
    fn line_carries_probs_and_sources() {
        let d = resolve_route(
            &route_questions(),
            Ok(vec![
                vec![0.0, 0.0, 1.0],
                vec![1.0 / 3.0; 3],
                vec![1.0, 0.0],
            ]),
            0.6,
        );
        let line = log_line(&d, "t", false, outcome());
        assert_eq!(line.answers[0].source, "laya");
        assert_eq!(line.answers[0].probs, vec![0.0, 0.0, 1.0]);
        assert!(line.answers[1].source.starts_with("static: low confidence"));
        assert_eq!(line.outcome.tool_calls, 7);
    }

    #[test]
    fn append_writes_one_json_object_per_line() {
        let path = temp("append").join(".kode/router-log.jsonl");
        let d = resolve_route(&route_questions(), Err("disabled".to_string()), 0.6);
        append(&path, &log_line(&d, "a", false, outcome())).unwrap();
        append(&path, &log_line(&d, "b", false, outcome())).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["outcome"]["status"], "completed");
        }
    }

    #[test]
    fn append_fails_cleanly_when_parent_is_a_file() {
        let dir = temp("blocked");
        std::fs::write(dir.join(".kode"), b"not a dir").unwrap();
        let d = resolve_route(&route_questions(), Err("disabled".to_string()), 0.6);
        assert!(
            append(
                &dir.join(".kode/router-log.jsonl"),
                &log_line(&d, "a", false, outcome())
            )
            .is_err()
        );
    }
}
