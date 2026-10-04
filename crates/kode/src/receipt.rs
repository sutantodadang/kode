//! Shareable receipts built only from persisted session data.

use std::path::Path;

use crate::ledger::{FactSource, LedgerEntry, VerifyOutcome};
use crate::session::Turn;

pub struct ReceiptMeta {
    pub session: String,
    pub model: Option<String>,
}

fn thousands(n: u64) -> String {
    crate::repo_map::thousands(n)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The first prose paragraph (lines joined with spaces), skipping blank
/// lines and fenced code.
pub fn first_paragraph(response: &str) -> Option<String> {
    let mut in_code = false;
    let mut para: Vec<&str> = Vec::new();
    for line in response.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if !para.is_empty() {
                break;
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if trimmed.is_empty() {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        para.push(trimmed);
    }
    (!para.is_empty()).then(|| para.join(" "))
}

/// Final status per check, in first-seen order, with a repair flag.
fn final_checks(ledger: &[LedgerEntry]) -> Vec<(String, VerifyOutcome, u64, bool)> {
    let mut out: Vec<(String, VerifyOutcome, u64, bool)> = Vec::new();
    for entry in ledger {
        if let LedgerEntry::Verify {
            name,
            outcome,
            duration_ms,
        } = entry
        {
            match out.iter_mut().find(|(n, ..)| n == name) {
                Some(row) => {
                    let repaired = row.3 || row.1 == VerifyOutcome::Failed;
                    *row = (name.clone(), *outcome, *duration_ms, repaired);
                }
                None => out.push((name.clone(), *outcome, *duration_ms, false)),
            }
        }
    }
    out
}

pub fn turn_markdown(turn: &Turn, include_personal: bool) -> String {
    let title = turn.task.lines().next().unwrap_or("").trim();
    let mut md = format!("### Kode receipt — {title}\n\n");
    if let Some(p) = first_paragraph(&turn.response) {
        md.push_str(&format!("> {p}\n\n"));
    }
    if turn.ledger.is_empty() {
        md.push_str("_No ledger recorded for this turn._\n");
        return md;
    }
    let ledger = &turn.ledger;

    for entry in ledger {
        if let LedgerEntry::GraphAnswer {
            query,
            symbol,
            latency_ms,
        } = entry
        {
            md.push_str(&format!(
                "**Answered from the code graph:** {query} of {symbol} · 0 tokens · {latency_ms}ms\n"
            ));
        }
        if let LedgerEntry::Route { answers } = entry {
            let value = |k: &str| answers.iter().find(|a| a.key == k).map(|a| a.value.clone());
            let mut parts = Vec::new();
            if let Some(t) = value("tier") {
                parts.push(t);
            }
            if let Some(e) = value("effort") {
                parts.push(format!("effort {e}"));
            }
            if let Some(p) = value("plan") {
                parts.push(p);
            }
            let confidence = answers
                .iter()
                .filter(|a| a.laya && a.key == "tier")
                .find_map(|a| a.confidence);
            let origin = match confidence {
                Some(c) => format!(" (confidence {c:.2})"),
                None => " (static)".to_string(),
            };
            md.push_str(&format!("**Route:** {}{origin}\n", parts.join(" · ")));
        }
    }
    let count = |s: FactSource| {
        ledger
            .iter()
            .filter(|e| matches!(e, LedgerEntry::Fact { source, .. } if *source == s))
            .count()
    };
    md.push_str(&format!(
        "**Context:** {} · {} · {}\n",
        plural(count(FactSource::Zindeks), "graph fact", "graph facts"),
        plural(count(FactSource::Ingat), "memory", "memories"),
        plural(count(FactSource::Git), "git signal", "git signals"),
    ));

    let files: Vec<_> = ledger
        .iter()
        .filter_map(|e| match e {
            LedgerEntry::Change { files, .. } => Some(files),
            _ => None,
        })
        .flatten()
        .collect();
    if !files.is_empty() {
        md.push_str("\n| File | Change | Impact |\n|---|---|---|\n");
        for f in files {
            let impact = ledger
                .iter()
                .find_map(|e| match e {
                    LedgerEntry::Impact {
                        file,
                        symbol,
                        callers,
                        tests,
                        ..
                    } if *file == f.path => {
                        Some(format!("{symbol} ← {callers} callers, {tests} tests"))
                    }
                    _ => None,
                })
                .unwrap_or_default();
            md.push_str(&format!(
                "| {} | +{} −{} | {impact} |\n",
                f.path, f.added, f.removed
            ));
        }
    }

    let checks = final_checks(ledger);
    if !checks.is_empty() {
        md.push_str("\n| Check | Result | Time |\n|---|---|---|\n");
        for (name, outcome, ms, repaired) in checks {
            let time = format!("{:.1}s", ms as f64 / 1000.0);
            let row = match outcome {
                VerifyOutcome::Passed if repaired => {
                    format!("| {name} | ✓ passed (after repair) | {time} |")
                }
                VerifyOutcome::Passed => format!("| {name} | ✓ passed | {time} |"),
                VerifyOutcome::Failed => format!("| {name} | ✗ failed | {time} |"),
                VerifyOutcome::Skipped => format!("| {name} | – skipped | |"),
            };
            md.push_str(&row);
            md.push('\n');
        }
    }

    md.push('\n');
    for entry in ledger {
        match entry {
            LedgerEntry::Usage {
                input,
                output,
                cached,
            } => md.push_str(&match cached {
                Some(c) => format!(
                    "**Tokens:** {} in ({} cached) · {} out\n",
                    thousands(*input),
                    thousands(*c),
                    thousands(*output)
                ),
                None => format!(
                    "**Tokens:** {} in · {} out\n",
                    thousands(*input),
                    thousands(*output)
                ),
            }),
            LedgerEntry::Memory {
                text, team: true, ..
            } => md.push_str(&format!("**Team memory saved:** \"{text}\"\n")),
            LedgerEntry::Memory {
                text, team: false, ..
            } if include_personal => md.push_str(&format!("**Memory saved:** \"{text}\"\n")),
            _ => {}
        }
    }
    md
}

pub fn markdown(turns: &[Turn], meta: &ReceiptMeta, include_personal: bool) -> String {
    let mut md = turns
        .iter()
        .map(|t| turn_markdown(t, include_personal))
        .collect::<Vec<_>>()
        .join("\n---\n\n");
    let model = meta
        .model
        .as_deref()
        .map(|m| format!("Model: {m} · "))
        .unwrap_or_default();
    md.push_str(&format!("\n_{model}Session: {}_\n", meta.session));
    md
}

pub fn trailers(turns: &[Turn], meta: &ReceiptMeta) -> String {
    let all: Vec<LedgerEntry> = turns
        .iter()
        .flat_map(|t| t.ledger.iter().cloned())
        .collect();
    let checks = final_checks(&all);
    let names = |o: VerifyOutcome| -> Vec<String> {
        checks
            .iter()
            .filter(|c| c.1 == o)
            .map(|c| c.0.clone())
            .collect()
    };
    let mut out = String::new();
    for (key, outcome) in [
        ("Kode-Verified", VerifyOutcome::Passed),
        ("Kode-Failed", VerifyOutcome::Failed),
        ("Kode-Skipped", VerifyOutcome::Skipped),
    ] {
        let list = names(outcome);
        if !list.is_empty() {
            out.push_str(&format!("{key}: {}\n", list.join(", ")));
        }
    }
    if let Some(model) = &meta.model {
        out.push_str(&format!("Kode-Model: {model}\n"));
    }
    out.push_str(&format!("Kode-Session: {}\n", meta.session));
    out
}

pub fn select_turns(turns: Vec<Turn>, turn: Option<usize>) -> Result<Vec<Turn>, String> {
    match turn {
        None => Ok(turns),
        Some(n) if n >= 1 && n <= turns.len() => Ok(vec![turns[n - 1].clone()]),
        Some(n) => Err(format!("no turn {n} — this session has {}", turns.len())),
    }
}

/// Command-runner seam for `gh`; returns `(success, stderr)`.
pub type GhRunner<'a> = &'a dyn Fn(&[&str]) -> std::io::Result<(bool, String)>;

/// Adds `body` as a comment on the current branch's PR via `gh`. Never
/// edits the PR body.
pub fn post_pr_comment(body: &str, gh: GhRunner<'_>) -> Result<(), String> {
    let path = std::env::temp_dir().join(format!("kode-receipt-{}.md", std::process::id()));
    std::fs::write(&path, body).map_err(|e| format!("not posted: cannot write temp file: {e}"))?;
    let path_text = path.to_string_lossy().to_string();
    let result = gh(&["pr", "comment", "--body-file", &path_text]);
    let _ = std::fs::remove_file(&path);
    match result {
        Ok((true, _)) => Ok(()),
        Ok((false, stderr)) => Err(format!(
            "not posted: {}",
            stderr.lines().next().unwrap_or("gh failed").trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("not posted: gh (GitHub CLI) not found".to_string())
        }
        Err(e) => Err(format!("not posted: {e}")),
    }
}

fn run_gh(args: &[&str]) -> std::io::Result<(bool, String)> {
    let out = std::process::Command::new("gh").args(args).output()?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

pub async fn run(
    cwd: &Path,
    session: Option<String>,
    turn: Option<usize>,
    pr: bool,
    trailer: bool,
    include_personal: bool,
) -> anyhow::Result<()> {
    let id = session
        .or_else(|| crate::session::latest(cwd))
        .ok_or_else(|| anyhow::anyhow!("no sessions in .kode/sessions — run a task first"))?;
    let (turns, _) = crate::session::load(cwd, &id)?;
    let turns = select_turns(turns, turn).map_err(anyhow::Error::msg)?;
    let meta = ReceiptMeta {
        session: id.clone(),
        model: crate::session::model_for(cwd, &id).map(|(_, model)| model),
    };
    if trailer {
        print!("{}", trailers(&turns, &meta));
        return Ok(());
    }
    let body = markdown(&turns, &meta, include_personal);
    if pr {
        match post_pr_comment(&body, &run_gh) {
            Ok(()) => println!("receipt posted as a PR comment"),
            Err(reason) => {
                print!("{body}");
                eprintln!("{reason}");
            }
        }
    } else {
        print!("{body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::*;
    use crate::session::Turn;

    fn turn(task: &str, response: &str, ledger: Vec<LedgerEntry>) -> Turn {
        Turn {
            ts: "t".into(),
            task: task.into(),
            images: vec![],
            response: response.into(),
            tool_calls: 3,
            ledger,
        }
    }

    fn verify(name: &str, outcome: VerifyOutcome, ms: u64) -> LedgerEntry {
        LedgerEntry::Verify {
            name: name.into(),
            outcome,
            duration_ms: ms,
        }
    }

    fn full_turn() -> Turn {
        turn(
            "fix the flaky retry in fetch_catalog\nmore detail",
            "fetch_catalog has 4 callers and fails when the catalog is slow.\n\nSecond paragraph.",
            vec![
                LedgerEntry::Route {
                    answers: vec![
                        RouteRecord {
                            key: "tier".into(),
                            value: "standard".into(),
                            confidence: Some(0.71),
                            laya: true,
                            reason: None,
                        },
                        RouteRecord {
                            key: "effort".into(),
                            value: "high".into(),
                            confidence: Some(0.6),
                            laya: true,
                            reason: None,
                        },
                        RouteRecord {
                            key: "plan".into(),
                            value: "direct".into(),
                            confidence: Some(0.5),
                            laya: true,
                            reason: None,
                        },
                    ],
                },
                LedgerEntry::Fact {
                    source: FactSource::Zindeks,
                    text: "a".into(),
                },
                LedgerEntry::Fact {
                    source: FactSource::Zindeks,
                    text: "b".into(),
                },
                LedgerEntry::Fact {
                    source: FactSource::Ingat,
                    text: "c".into(),
                },
                LedgerEntry::Change {
                    files: vec![ChangeRecord {
                        path: "crates/kode-context/src/catalog.rs".into(),
                        added: 14,
                        removed: 3,
                    }],
                    reverted: vec![],
                },
                LedgerEntry::Impact {
                    file: "crates/kode-context/src/catalog.rs".into(),
                    symbol: "fetch_catalog".into(),
                    callers: 4,
                    crates: 2,
                    tests: 3,
                },
                verify("test", VerifyOutcome::Failed, 30_000),
                verify("fmt", VerifyOutcome::Passed, 400),
                verify("test", VerifyOutcome::Passed, 41_000),
                verify("lint", VerifyOutcome::Skipped, 0),
                LedgerEntry::Usage {
                    input: 41_230,
                    output: 2_104,
                    cached: Some(36_100),
                },
                LedgerEntry::Memory {
                    id: "m1".into(),
                    text: "Run tests serially on Windows".into(),
                    team: true,
                },
                LedgerEntry::Memory {
                    id: "m2".into(),
                    text: "my private note".into(),
                    team: false,
                },
            ],
        )
    }

    #[test]
    fn full_turn_receipt() {
        let md = turn_markdown(&full_turn(), false);
        let expected = "\
### Kode receipt — fix the flaky retry in fetch_catalog

> fetch_catalog has 4 callers and fails when the catalog is slow.

**Route:** standard · effort high · direct (confidence 0.71)
**Context:** 2 graph facts · 1 memory · 0 git signals

| File | Change | Impact |
|---|---|---|
| crates/kode-context/src/catalog.rs | +14 −3 | fetch_catalog ← 4 callers, 3 tests |

| Check | Result | Time |
|---|---|---|
| test | ✓ passed (after repair) | 41.0s |
| fmt | ✓ passed | 0.4s |
| lint | – skipped | |

**Tokens:** 41,230 in (36,100 cached) · 2,104 out
**Team memory saved:** \"Run tests serially on Windows\"
";
        assert_eq!(md, expected);
        assert!(!md.contains("my private note"));
        assert!(
            turn_markdown(&full_turn(), true).contains("**Memory saved:** \"my private note\"")
        );
    }

    #[test]
    fn missing_cache_is_omitted_not_zero() {
        let t = turn(
            "t",
            "r",
            vec![LedgerEntry::Usage {
                input: 10,
                output: 2,
                cached: None,
            }],
        );
        assert!(turn_markdown(&t, false).contains("**Tokens:** 10 in · 2 out\n"));
    }

    #[test]
    fn legacy_turn_renders_without_tables() {
        let md = turn_markdown(&turn("old task", "old answer", vec![]), false);
        assert_eq!(
            md,
            "### Kode receipt — old task\n\n> old answer\n\n_No ledger recorded for this turn._\n"
        );
    }

    #[test]
    fn first_paragraph_skips_code_and_blank_lines() {
        assert_eq!(
            first_paragraph(
                "\n\n```rust\nfn x() {}\n```\n\nThe fix keeps retries.\nIt also logs.\n\nMore."
            ),
            Some("The fix keeps retries. It also logs.".into())
        );
        assert_eq!(first_paragraph("```\nonly code\n```"), None);
    }

    #[test]
    fn graph_answer_turn_says_so() {
        let t = turn(
            "who calls append_turn",
            "Callers of append_turn:\n- x",
            vec![
                LedgerEntry::GraphAnswer {
                    query: "callers".into(),
                    symbol: "append_turn".into(),
                    latency_ms: 12,
                },
                LedgerEntry::Usage {
                    input: 0,
                    output: 0,
                    cached: Some(0),
                },
            ],
        );
        assert!(turn_markdown(&t, false).contains(
            "**Answered from the code graph:** callers of append_turn · 0 tokens · 12ms"
        ));
    }

    #[test]
    fn session_markdown_joins_turns_and_adds_footer() {
        let meta = ReceiptMeta {
            session: "20261004-102201".into(),
            model: Some("sonnet-5.5".into()),
        };
        let md = markdown(
            &[turn("a", "x", vec![]), turn("b", "y", vec![])],
            &meta,
            false,
        );
        assert!(md.contains("\n---\n\n### Kode receipt — b"));
        assert!(md.ends_with("_Model: sonnet-5.5 · Session: 20261004-102201_\n"));
    }

    #[test]
    fn trailers_use_final_status_across_turns() {
        let t1 = turn(
            "a",
            "",
            vec![
                verify("test", VerifyOutcome::Failed, 1),
                verify("lint", VerifyOutcome::Skipped, 0),
            ],
        );
        let t2 = turn(
            "b",
            "",
            vec![
                verify("test", VerifyOutcome::Passed, 1),
                verify("fmt", VerifyOutcome::Passed, 1),
                verify("clippy", VerifyOutcome::Failed, 1),
            ],
        );
        let meta = ReceiptMeta {
            session: "s1".into(),
            model: Some("m".into()),
        };
        assert_eq!(
            trailers(&[t1, t2], &meta),
            "Kode-Verified: test, fmt\nKode-Failed: clippy\nKode-Skipped: lint\nKode-Model: m\nKode-Session: s1\n"
        );
    }

    #[test]
    fn trailers_without_checks_or_model() {
        let meta = ReceiptMeta {
            session: "s1".into(),
            model: None,
        };
        assert_eq!(
            trailers(&[turn("a", "", vec![])], &meta),
            "Kode-Session: s1\n"
        );
    }

    #[test]
    fn no_pr_prints_reason() {
        let gh = |_: &[&str]| {
            Ok((
                false,
                "no pull requests found for branch \"feat/x\"\nmore".to_string(),
            ))
        };
        assert_eq!(
            post_pr_comment("body", &gh).unwrap_err(),
            "not posted: no pull requests found for branch \"feat/x\""
        );
    }

    #[test]
    fn missing_gh_is_explained() {
        let gh = |_: &[&str]| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert_eq!(
            post_pr_comment("body", &gh).unwrap_err(),
            "not posted: gh (GitHub CLI) not found"
        );
    }

    #[test]
    fn posts_with_body_file() {
        let seen = std::sync::Mutex::new(Vec::<String>::new());
        let gh = |args: &[&str]| {
            seen.lock()
                .unwrap()
                .extend(args.iter().map(|s| s.to_string()));
            let body = std::fs::read_to_string(args[3]).unwrap();
            assert_eq!(body, "body");
            Ok((true, String::new()))
        };
        post_pr_comment("body", &gh).unwrap();
        let args = seen.lock().unwrap();
        assert_eq!(&args[..3], &["pr", "comment", "--body-file"]);
    }

    #[test]
    fn select_turns_by_number() {
        let turns = vec![turn("a", "", vec![]), turn("b", "", vec![])];
        assert_eq!(select_turns(turns.clone(), None).unwrap().len(), 2);
        assert_eq!(select_turns(turns.clone(), Some(2)).unwrap()[0].task, "b");
        assert_eq!(
            select_turns(turns, Some(3)).unwrap_err(),
            "no turn 3 — this session has 2"
        );
    }
}
