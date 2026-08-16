//! JSONL session store for resume-chat: one file per session under
//! `.kode/sessions/`, header line + one line per completed turn.
//! Spec: docs/superpowers/specs/2026-08-17-resume-chat-design.md

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One completed conversation turn (task in, final answer out).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub ts: String,
    pub task: String,
    pub response: String,
    pub tool_calls: u32,
}

/// Listing row for the `/resume` picker.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub first_task: String,
    pub turns: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    v: u32,
    created: String,
    provider: String,
    model: String,
}

fn sessions_dir(cwd: &Path) -> PathBuf {
    cwd.join(".kode").join("sessions")
}

fn session_path(cwd: &Path, id: &str) -> PathBuf {
    sessions_dir(cwd).join(format!("{id}.jsonl"))
}

/// Current UTC time as (`YYYYMMDD-HHMMSS` id stamp, RFC3339 seconds).
/// Civil-from-days per Howard Hinnant's algorithm — std has no calendars.
pub fn now_utc_stamp() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (
        format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}"),
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z"),
    )
}

/// Creates a new session file with a header line; returns its id.
/// Same-second collision gets a `-2`, `-3`, ... suffix.
pub fn create(cwd: &Path, provider: &str, model: &str) -> std::io::Result<String> {
    let dir = sessions_dir(cwd);
    fs::create_dir_all(&dir)?;
    let (stamp, created) = now_utc_stamp();
    let mut id = stamp.clone();
    let mut n = 1u32;
    while session_path(cwd, &id).exists() {
        n += 1;
        id = format!("{stamp}-{n}");
    }
    let header = Header {
        v: 1,
        created,
        provider: provider.to_string(),
        model: model.to_string(),
    };
    let mut f = fs::File::create(session_path(cwd, &id))?;
    writeln!(f, "{}", serde_json::to_string(&header)?)?;
    Ok(id)
}

/// Newest session id, by filename ordering (UTC stamps sort correctly).
pub fn latest(cwd: &Path) -> Option<String> {
    list(cwd, 1).into_iter().next().map(|m| m.id)
}

/// Recent sessions, newest first. Sessions with zero turns are skipped —
/// nothing to resume.
pub fn list(cwd: &Path, limit: usize) -> Vec<SessionMeta> {
    let mut ids: Vec<String> = match fs::read_dir(sessions_dir(cwd)) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_suffix(".jsonl").map(|s| s.to_string())
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    ids.sort();
    ids.reverse();
    let mut out = Vec::new();
    for id in ids {
        if out.len() >= limit {
            break;
        }
        if let Ok((turns, _)) = load(cwd, &id)
            && !turns.is_empty()
        {
            out.push(SessionMeta {
                first_task: turns[0].task.clone(),
                turns: turns.len(),
                id,
            });
        }
    }
    out
}

/// Loads a session's turns. Returns `(turns, corrupt_line_count)` — corrupt
/// lines are skipped, never fatal.
pub fn load(cwd: &Path, id: &str) -> std::io::Result<(Vec<Turn>, usize)> {
    let text = fs::read_to_string(session_path(cwd, id))?;
    let mut turns = Vec::new();
    let mut corrupt = 0usize;
    for line in text.lines().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Turn>(line) {
            Ok(t) => turns.push(t),
            Err(_) => corrupt += 1,
        }
    }
    Ok((turns, corrupt))
}

/// Appends one completed turn.
pub fn append_turn(cwd: &Path, id: &str, turn: &Turn) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(session_path(cwd, id))?;
    writeln!(f, "{}", serde_json::to_string(turn)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn temp_cwd() -> PathBuf {
        static C: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = C.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kode-session-{}-{}-{n}",
            std::process::id(),
            nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn turn(task: &str) -> Turn {
        Turn {
            ts: "2026-08-17T00:00:00Z".to_string(),
            task: task.to_string(),
            response: format!("answer to {task}"),
            tool_calls: 1,
        }
    }

    #[test]
    fn create_append_load_round_trips() {
        let cwd = temp_cwd();
        let id = create(&cwd, "codex", "gpt-5.6-sol").unwrap();
        append_turn(&cwd, &id, &turn("t1")).unwrap();
        append_turn(&cwd, &id, &turn("t2")).unwrap();
        let (turns, corrupt) = load(&cwd, &id).unwrap();
        assert_eq!(corrupt, 0);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].task, "t1");
        assert_eq!(turns[1].response, "answer to t2");
    }

    #[test]
    fn latest_and_list_order_newest_first_and_skip_empty() {
        let cwd = temp_cwd();
        let a = create(&cwd, "codex", "m").unwrap();
        append_turn(&cwd, &a, &turn("first session")).unwrap();
        let b = create(&cwd, "codex", "m").unwrap(); // same second → suffix, sorts after
        append_turn(&cwd, &b, &turn("second session")).unwrap();
        let empty = create(&cwd, "codex", "m").unwrap(); // no turns → skipped
        let metas = list(&cwd, 10);
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].id, b);
        assert_eq!(metas[0].first_task, "second session");
        assert!(!metas.iter().any(|m| m.id == empty));
        assert_eq!(latest(&cwd).unwrap(), b);
    }

    #[test]
    fn load_skips_corrupt_lines_and_counts_them() {
        let cwd = temp_cwd();
        let id = create(&cwd, "codex", "m").unwrap();
        append_turn(&cwd, &id, &turn("good")).unwrap();
        let path = cwd
            .join(".kode")
            .join("sessions")
            .join(format!("{id}.jsonl"));
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{not json").unwrap();
        append_turn(&cwd, &id, &turn("also good")).unwrap();
        let (turns, corrupt) = load(&cwd, &id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(corrupt, 1);
    }

    #[test]
    fn latest_on_missing_dir_is_none() {
        let cwd = temp_cwd();
        assert_eq!(latest(&cwd), None);
        assert!(list(&cwd, 5).is_empty());
    }

    #[test]
    fn now_utc_stamp_shapes() {
        let (id, rfc) = now_utc_stamp();
        assert_eq!(id.len(), 15);
        assert_eq!(&id[8..9], "-");
        assert!(rfc.ends_with('Z'));
        assert_eq!(rfc.len(), 20);
    }
}
