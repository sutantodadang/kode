//! Zero-token repo map: totals, core orchestrators (fan-out) and hot
//! symbols (fan-in) straight from the code graph.

use std::path::Path;
use std::time::Instant;

use kode_core::config::ZindeksConfig;
use kode_core::event::{KodeEvent, NoteSource};
use kode_intel::{ArchSymbol, ArchitectureSummary};

use crate::session_runtime::SessionRuntime;

/// Names too generic to tell a newcomer anything.
const GENERIC: &[&str] = &[
    "new", "default", "push", "len", "fmt", "from", "into", "clone", "drop", "eq", "hash", "state",
    "run", "main", "get", "set",
];
const PER_ROW: usize = 3;

fn is_noise(s: &ArchSymbol) -> bool {
    GENERIC.contains(&s.name.as_str())
        || s.file.ends_with("tests.rs")
        || s.file.contains("/tests/")
        || s.name.starts_with("test_")
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn map_lines(a: &ArchitectureSummary, elapsed_ms: u64) -> Vec<String> {
    let mut lines = vec![format!(
        "repo  {} files · {} symbols · {} edges",
        thousands(a.total_files),
        thousands(a.total_symbols),
        thousands(a.total_edges)
    )];
    let core: Vec<String> = a
        .high_fan_out
        .iter()
        .filter(|s| !is_noise(s))
        .take(PER_ROW)
        .map(|s| format!("{} ({})", s.name, file_name(&s.file)))
        .collect();
    if !core.is_empty() {
        lines.push(format!("core  {}", core.join(" · ")));
    }
    let hot: Vec<String> = a
        .high_fan_in
        .iter()
        .filter(|s| !is_noise(s))
        .take(PER_ROW)
        .map(|s| format!("{} ← {}", s.name, s.degree))
        .collect();
    if !hot.is_empty() {
        lines.push(format!("hot   {}", hot.join(" · ")));
    }
    lines.push(format!("map · 0 tokens · {elapsed_ms}ms"));
    lines
}

/// Builds the map as events for the shared bus. Never fails: an engine
/// problem becomes one plain `Note`.
pub async fn map_events(
    runtime: &SessionRuntime,
    cfg: &ZindeksConfig,
    cwd: &Path,
) -> Vec<KodeEvent> {
    let started = Instant::now();
    let backend = match runtime.intel(cfg, cwd).await {
        Ok(Some(handle)) => {
            if handle.fresh
                && let Err(e) = handle.backend.ensure_bound().await
            {
                runtime.forget_intel().await;
                return vec![KodeEvent::Note {
                    text: format!("repo map unavailable: {e}"),
                }];
            }
            handle.backend
        }
        Ok(None) => {
            return vec![KodeEvent::Note {
                text: "repo map unavailable: zindeks is disabled".into(),
            }];
        }
        Err(e) => {
            return vec![KodeEvent::Note {
                text: format!("repo map unavailable: {e}"),
            }];
        }
    };
    match backend.architecture(10).await {
        Ok(a) => map_lines(&a, started.elapsed().as_millis() as u64)
            .into_iter()
            .map(|text| KodeEvent::SourcedNote {
                text,
                source: NoteSource::Zindeks,
            })
            .collect(),
        Err(e) => vec![KodeEvent::Note {
            text: format!("repo map unavailable: {e}"),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_intel::{ArchSymbol, ArchitectureSummary};

    fn sym(name: &str, file: &str, degree: u32) -> ArchSymbol {
        ArchSymbol {
            name: name.into(),
            kind: "function".into(),
            file: file.into(),
            degree,
        }
    }

    #[test]
    fn map_lines_show_totals_core_and_hot_without_noise() {
        let a = ArchitectureSummary {
            total_files: 132,
            total_symbols: 2624,
            total_edges: 6029,
            entry_points: vec![],
            high_fan_out: vec![
                sym("new", "crates/kode/src/tui/state.rs", 91),
                sym("execute_task", "crates/kode/src/pipeline.rs", 81),
                sym("compile", "crates/kode-context/src/compile.rs", 48),
            ],
            high_fan_in: vec![
                sym("push", "crates/kode-model/src/stream.rs", 302),
                sym("state", "crates/kode/src/tui/tests.rs", 160),
                sym("apply_event", "crates/kode/src/tui/events.rs", 104),
                sym("emit", "crates/kode-core/src/event.rs", 64),
            ],
        };
        let lines = map_lines(&a, 140);
        assert_eq!(lines[0], "repo  132 files · 2,624 symbols · 6,029 edges");
        assert_eq!(
            lines[1],
            "core  execute_task (pipeline.rs) · compile (compile.rs)"
        );
        assert_eq!(lines[2], "hot   apply_event ← 104 · emit ← 64");
        assert_eq!(lines.last().unwrap(), "map · 0 tokens · 140ms");
    }

    #[test]
    fn map_lines_omit_empty_sections() {
        let lines = map_lines(&ArchitectureSummary::default(), 5);
        assert_eq!(
            lines,
            vec![
                "repo  0 files · 0 symbols · 0 edges",
                "map · 0 tokens · 5ms"
            ]
        );
    }
}
