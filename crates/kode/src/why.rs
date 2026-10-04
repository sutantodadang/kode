//! `/why`: the provenance of one persisted turn, from its ledger only.

use crate::ledger::{FactSource, LedgerEntry, VerifyOutcome};
use crate::session::Turn;

pub fn parse_turn_arg(arg: &str, turns: usize) -> Result<usize, String> {
    if turns == 0 {
        return Err("no completed turns yet".to_string());
    }
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(turns - 1);
    }
    let n: usize = arg
        .parse()
        .map_err(|_| "usage: /why [turn number]".to_string())?;
    if n == 0 || n > turns {
        return Err(format!("no turn {n} — this session has {turns}"));
    }
    Ok(n - 1)
}

fn section(out: &mut Vec<String>, title: &str, rows: Vec<String>) {
    if rows.is_empty() {
        return;
    }
    out.push(format!(" {title}"));
    out.extend(rows.into_iter().map(|r| format!("   {r}")));
}

pub fn why_lines(turn: &Turn, number: usize) -> Vec<String> {
    let task: String = turn
        .task
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(60)
        .collect();
    let mut out = vec![format!(" WHY · turn {number} · {task}"), String::new()];
    if turn.ledger.is_empty() {
        out.push(" no ledger recorded for this turn".to_string());
    } else {
        let mut route = Vec::new();
        let (mut graph, mut memory, mut git, mut impact, mut verify, mut cost) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        for entry in &turn.ledger {
            match entry {
                LedgerEntry::Route { answers } => {
                    route.extend(answers.iter().map(|a| match (&a.reason, a.laya) {
                        (_, true) => {
                            format!(
                                "{}={} (laya {:.2})",
                                a.key,
                                a.value,
                                a.confidence.unwrap_or(0.0)
                            )
                        }
                        (Some(r), false) => format!("{}={} (static: {r})", a.key, a.value),
                        (None, false) => format!("{}={} (static)", a.key, a.value),
                    }))
                }
                LedgerEntry::Fact {
                    source: FactSource::Zindeks,
                    text,
                } => graph.push(text.clone()),
                LedgerEntry::Fact {
                    source: FactSource::Ingat,
                    text,
                } => memory.push(text.clone()),
                LedgerEntry::Fact {
                    source: FactSource::Git,
                    text,
                } => git.push(text.clone()),
                LedgerEntry::GraphAnswer {
                    query,
                    symbol,
                    latency_ms,
                } => graph.push(format!(
                    "answered from graph: {query} of {symbol} · 0 tokens · {latency_ms}ms"
                )),
                LedgerEntry::Change { files } => git.extend(
                    files
                        .iter()
                        .map(|f| format!("{} +{} −{}", f.path, f.added, f.removed)),
                ),
                LedgerEntry::Impact {
                    symbol,
                    callers,
                    crates,
                    tests,
                    ..
                } => impact.push(format!(
                    "{symbol} ← {callers} callers, {crates} crates, {tests} tests"
                )),
                LedgerEntry::Verify {
                    name,
                    outcome,
                    duration_ms,
                } => verify.push(match outcome {
                    VerifyOutcome::Passed => {
                        format!("{name} ✓ {:.1}s", *duration_ms as f64 / 1000.0)
                    }
                    VerifyOutcome::Failed => {
                        format!("{name} ✗ {:.1}s", *duration_ms as f64 / 1000.0)
                    }
                    VerifyOutcome::Skipped => format!("{name} – skipped"),
                }),
                LedgerEntry::Usage {
                    input,
                    output,
                    cached,
                } => cost.push(match cached {
                    Some(c) => format!("{input} in ({c} cached) · {output} out"),
                    None => format!("{input} in · {output} out · cache not reported"),
                }),
            }
        }
        section(&mut out, "ROUTE", route);
        section(&mut out, "GRAPH", graph);
        section(&mut out, "MEMORY", memory);
        section(&mut out, "GIT", git);
        section(&mut out, "IMPACT", impact);
        section(&mut out, "VERIFY", verify);
        section(&mut out, "COST", cost);
    }
    out.push(String::new());
    out.push(" Esc closes".to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::*;
    use crate::session::Turn;

    fn turn(ledger: Vec<LedgerEntry>) -> Turn {
        Turn {
            ts: "t".into(),
            task: "fix retry in fetch".into(),
            images: vec![],
            response: "r".into(),
            tool_calls: 2,
            ledger,
        }
    }

    #[test]
    fn why_on_legacy_turn() {
        let lines = why_lines(&turn(vec![]), 1);
        assert_eq!(lines[0], " WHY · turn 1 · fix retry in fetch");
        assert!(lines.contains(&" no ledger recorded for this turn".to_string()));
        assert_eq!(lines.last().unwrap(), " Esc closes");
    }

    #[test]
    fn why_groups_by_section_and_keeps_skipped_honest() {
        let lines = why_lines(
            &turn(vec![
                LedgerEntry::Route {
                    answers: vec![
                        RouteRecord {
                            key: "tier".into(),
                            value: "heavy".into(),
                            confidence: Some(0.82),
                            laya: true,
                            reason: None,
                        },
                        RouteRecord {
                            key: "plan".into(),
                            value: "direct".into(),
                            confidence: None,
                            laya: false,
                            reason: Some("disabled".into()),
                        },
                    ],
                },
                LedgerEntry::Fact {
                    source: FactSource::Zindeks,
                    text: "fetch ← 4 callers".into(),
                },
                LedgerEntry::Fact {
                    source: FactSource::Ingat,
                    text: "30s timeout reverted".into(),
                },
                LedgerEntry::Change {
                    files: vec![ChangeRecord {
                        path: "a.rs".into(),
                        added: 3,
                        removed: 1,
                    }],
                },
                LedgerEntry::Verify {
                    name: "lint".into(),
                    outcome: VerifyOutcome::Skipped,
                    duration_ms: 0,
                },
                LedgerEntry::Usage {
                    input: 100,
                    output: 20,
                    cached: None,
                },
            ]),
            2,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains(" ROUTE\n   tier=heavy (laya 0.82)\n   plan=direct (static: disabled)")
        );
        assert!(joined.contains(" GRAPH\n   fetch ← 4 callers"));
        assert!(joined.contains(" MEMORY\n   30s timeout reverted"));
        assert!(joined.contains(" GIT\n   a.rs +3 −1"));
        assert!(joined.contains(" VERIFY\n   lint – skipped"));
        assert!(joined.contains(" COST\n   100 in · 20 out · cache not reported"));
    }

    #[test]
    fn why_arg_parsing() {
        assert_eq!(parse_turn_arg("", 3), Ok(2));
        assert_eq!(parse_turn_arg("1", 3), Ok(0));
        assert_eq!(
            parse_turn_arg("4", 3),
            Err("no turn 4 — this session has 3".to_string())
        );
        assert_eq!(
            parse_turn_arg("x", 3),
            Err("usage: /why [turn number]".to_string())
        );
        assert_eq!(
            parse_turn_arg("", 0),
            Err("no completed turns yet".to_string())
        );
    }
}
