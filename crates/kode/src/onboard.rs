//! Zero-token tour for someone new to the repo: code map, team decisions,
//! and where to start reading.

use std::path::Path;

use kode_core::config::ZindeksConfig;
use kode_core::event::{KodeEvent, NoteSource};
use kode_intel::ArchitectureSummary;
use kode_memory::wire::WireEntry;

use crate::session_runtime::SessionRuntime;

const GROUPS: &[(&str, &[&str])] = &[
    ("DECISIONS", &["architecture-decision"]),
    ("CONVENTIONS", &["convention", "project-rule"]),
    ("KNOWN ISSUES", &["known-issue", "build-knowledge"]),
    ("REJECTED APPROACHES", &["rejected-approach"]),
];
const PER_GROUP: usize = 8;

pub fn memory_lines(entries: &[WireEntry], corrupt: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (title, kinds) in GROUPS {
        let rows: Vec<String> = entries
            .iter()
            .filter(|e| kinds.contains(&e.kind.as_str()))
            .take(PER_GROUP)
            .map(|e| {
                let first = e.content.lines().next().unwrap_or("").trim();
                if e.author.is_empty() {
                    format!("  {first}")
                } else {
                    format!("  {first} — {}", e.author)
                }
            })
            .collect();
        if !rows.is_empty() {
            lines.push((*title).to_string());
            lines.extend(rows);
        }
    }
    if lines.is_empty() && corrupt == 0 {
        return vec!["no team memory yet — share one with `kode remember --team`".to_string()];
    }
    if corrupt > 0 {
        lines.push(format!(
            "({corrupt} unreadable lines in .kode/memory/team.jsonl skipped)"
        ));
    }
    lines
}

pub fn start_here(a: &ArchitectureSummary) -> Vec<String> {
    a.high_fan_out
        .iter()
        .filter(|s| !crate::repo_map::is_noise(s))
        .take(3)
        .map(|s| format!("start here  {} — {}", s.name, s.file))
        .collect()
}

/// Map (graph-sourced), start points, then team memory (memory-sourced).
pub async fn onboard_events(
    runtime: &SessionRuntime,
    cfg: &ZindeksConfig,
    cwd: &Path,
) -> Vec<KodeEvent> {
    let mut events = Vec::new();
    let arch = match runtime.intel(cfg, cwd).await {
        Ok(Some(handle)) => {
            let bound = if handle.fresh {
                handle.backend.ensure_bound().await
            } else {
                Ok(())
            };
            match bound {
                Ok(()) => handle.backend.architecture(10).await.ok(),
                Err(_) => {
                    runtime.forget_intel().await;
                    None
                }
            }
        }
        _ => None,
    };
    match arch {
        Some(a) => {
            let mut lines = crate::repo_map::map_lines(&a, 0);
            lines.pop();
            lines.extend(start_here(&a));
            events.extend(lines.into_iter().map(|text| KodeEvent::SourcedNote {
                text,
                source: NoteSource::Zindeks,
            }));
        }
        None => events.push(KodeEvent::Note {
            text: "no code map yet — /index (or `kode index`) adds it".into(),
        }),
    }
    let (entries, corrupt) =
        kode_memory::wire::read_entries(&kode_memory::wire::team_file_path(cwd));
    events.extend(
        memory_lines(&entries, corrupt)
            .into_iter()
            .map(|text| KodeEvent::SourcedNote {
                text,
                source: NoteSource::Ingat,
            }),
    );
    events
}

pub async fn run(cwd: &Path, explain: bool) -> anyhow::Result<()> {
    let config = kode_core::config::KodeConfig::load(cwd)?;
    let runtime = SessionRuntime::new();
    let lines: Vec<String> = onboard_events(&runtime, &config.zindeks, cwd)
        .await
        .into_iter()
        .filter_map(|ev| match ev {
            KodeEvent::SourcedNote { text, .. } | KodeEvent::Note { text } => Some(text),
            _ => None,
        })
        .collect();
    for line in &lines {
        println!("{line}");
    }
    if explain {
        let model = crate::pipeline::ModelFactory::create(&config)?;
        let request = kode_model::ModelRequest {
            messages: vec![kode_model::Message::User(format!(
                "You are giving a new engineer a short spoken tour of this repository. \
                 Using only the facts below, explain in under 200 words where to start and what to keep in mind.\n\n{}",
                lines.join("\n")
            ))],
            max_tokens: Some(600),
            ..Default::default()
        };
        let answer = kode_model::collect_response(model.stream(request).await?).await?;
        println!("\n— model-written tour —\n{}", answer.content.trim());
        match answer.usage {
            Some(usage) => eprintln!(
                "— {} tokens in, {} out —",
                usage.input_tokens, usage.output_tokens
            ),
            None => eprintln!("— token usage not reported —"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_intel::{ArchSymbol, ArchitectureSummary};
    use kode_memory::wire::WireEntry;

    fn entry(kind: &str, content: &str, author: &str) -> WireEntry {
        WireEntry {
            v: 1,
            id: "id".into(),
            hash: "h".into(),
            kind: kind.into(),
            content: content.into(),
            tags: vec![],
            author: author.into(),
            repository: None,
            created_at: "t".into(),
            provenance: "explicit-user".into(),
        }
    }

    #[test]
    fn memory_lines_group_by_kind() {
        let lines = memory_lines(
            &[
                entry("architecture-decision", "Engines run in-process", "ana"),
                entry("convention", "Pipeline emits events only", "ben"),
                entry("project-rule", "No prints in pipeline", ""),
                entry("known-issue", "Windows CI needs serial tests", "ana"),
                entry("rejected-approach", "Spawned engine processes", "ana"),
                entry("user-preference", "terse output", "ben"),
            ],
            0,
        );
        assert_eq!(
            lines,
            vec![
                "DECISIONS",
                "  Engines run in-process — ana",
                "CONVENTIONS",
                "  Pipeline emits events only — ben",
                "  No prints in pipeline",
                "KNOWN ISSUES",
                "  Windows CI needs serial tests — ana",
                "REJECTED APPROACHES",
                "  Spawned engine processes — ana",
            ]
        );
    }

    #[test]
    fn onboard_reports_corrupt_team_lines() {
        let lines = memory_lines(&[entry("convention", "x", "")], 2);
        assert_eq!(
            lines.last().unwrap(),
            "(2 unreadable lines in .kode/memory/team.jsonl skipped)"
        );
    }

    #[test]
    fn no_team_memory_says_how_to_add_it() {
        assert_eq!(
            memory_lines(&[], 0),
            vec!["no team memory yet — share one with `kode remember --team`"]
        );
    }

    #[test]
    fn start_here_uses_core_orchestrators() {
        let a = ArchitectureSummary {
            high_fan_out: vec![
                ArchSymbol {
                    name: "new".into(),
                    kind: "method".into(),
                    file: "a.rs".into(),
                    degree: 90,
                },
                ArchSymbol {
                    name: "execute_task".into(),
                    kind: "function".into(),
                    file: "crates/kode/src/pipeline.rs".into(),
                    degree: 81,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            start_here(&a),
            vec!["start here  execute_task — crates/kode/src/pipeline.rs"]
        );
    }
}
