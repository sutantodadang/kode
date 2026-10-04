//! Answers pure code-graph lookups without a model call, when the local
//! router is confident. Every miss is an `Err(reason)` the caller turns
//! into a sourced note before falling back to the model.

use std::path::Path;

use kode_intel::{CodeIntelligence, GraphSymbol, TraceDirection, TraceNode};
use kode_local::route::RouteDecision;

use crate::test_symbols::TestClassifier;

const MAX_LIST: usize = 15;
const MAX_DEFS: usize = 5;
const STOPWORDS: &[&str] = &[
    "Where", "What", "Who", "How", "Which", "Why", "When", "Main", "Show", "Find",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphQuery {
    Definition,
    Callers,
    Callees,
    Impact,
    Structure,
}

impl GraphQuery {
    pub fn as_str(self) -> &'static str {
        match self {
            GraphQuery::Definition => "definition",
            GraphQuery::Callers => "callers",
            GraphQuery::Callees => "callees",
            GraphQuery::Impact => "impact",
            GraphQuery::Structure => "structure",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "definition" => GraphQuery::Definition,
            "callers" => GraphQuery::Callers,
            "callees" => GraphQuery::Callees,
            "impact" => GraphQuery::Impact,
            "structure" => GraphQuery::Structure,
            _ => return None,
        })
    }
}

/// Graph path only when Laya itself answered both questions and the
/// `answer=graph` confidence clears `threshold`. Static answers never
/// trigger it.
pub fn wants_graph(decision: &RouteDecision, threshold: f32) -> Option<GraphQuery> {
    use kode_core::event::RouteSource;
    let answer = decision.answer("answer")?;
    let kind = decision.answer("graph_query")?;
    if answer.source != RouteSource::Laya || kind.source != RouteSource::Laya {
        return None;
    }
    if answer.value != "graph" || answer.confidence.unwrap_or(0.0) < threshold {
        return None;
    }
    GraphQuery::parse(&kind.value)
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit()
}

/// Symbol candidates in priority order: backticked spans, `a::b` paths
/// (last segment), then `snake_case`, `CamelCase` (two capitals) and
/// `name()` tokens. Deduplicated, order kept.
pub fn candidates(task: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |s: &str, out: &mut Vec<String>| {
        let s = s.trim_end_matches("()");
        if is_identifier(s) && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    for (i, span) in task.split('`').enumerate() {
        if i % 2 == 1 {
            push(span.rsplit("::").next().unwrap_or(span), &mut out);
        }
    }
    let words: Vec<&str> = task
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '(' || c == ')'))
        .filter(|w| !w.is_empty())
        .collect();
    for w in &words {
        if w.contains("::") {
            push(w.rsplit("::").next().unwrap_or(w), &mut out);
        }
    }
    for w in &words {
        let bare = w.trim_end_matches("()");
        let snake = bare.contains('_');
        let camel = bare.chars().next().is_some_and(|c| c.is_uppercase())
            && bare.chars().skip(1).any(|c| c.is_uppercase())
            && bare.chars().any(|c| c.is_lowercase());
        let called = w.ends_with("()");
        if (snake || camel || called) && !STOPWORDS.contains(&bare) && !w.contains("::") {
            push(bare, &mut out);
        }
    }
    out
}

/// First candidate with an exact-name match, with ALL its rows (highest
/// degree first); the caller decides what to do with ambiguity.
pub async fn resolve_symbol(
    intel: &dyn CodeIntelligence,
    candidates: &[String],
) -> Option<(String, Vec<GraphSymbol>)> {
    for name in candidates {
        let Ok(rows) = intel.exact_symbols(name, None).await else {
            continue;
        };
        if !rows.is_empty() {
            return Some((name.clone(), rows));
        }
    }
    None
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphAnswer {
    pub query: GraphQuery,
    pub symbol: String,
    /// One `g`-knotted ledger line each.
    pub facts: Vec<String>,
    /// Plain-text answer: printed by exec, stored as the turn's response.
    pub text: String,
}

fn listed(nodes: &[TraceNode], depth: u32) -> Vec<&TraceNode> {
    nodes.iter().filter(|n| n.depth == depth).collect()
}

fn capped(lines: Vec<String>) -> Vec<String> {
    let total = lines.len();
    let mut out: Vec<String> = lines.into_iter().take(MAX_LIST).collect();
    if total > MAX_LIST {
        out.push(format!("+{} more", total - MAX_LIST));
    }
    out
}

pub async fn answer(
    intel: &dyn CodeIntelligence,
    query: GraphQuery,
    task: &str,
    root: &Path,
) -> Result<GraphAnswer, String> {
    if query == GraphQuery::Structure {
        let arch = intel.architecture(10).await.map_err(|e| e.to_string())?;
        let mut facts = crate::repo_map::map_lines(&arch, 0);
        facts.pop(); // drop the "map · 0 tokens" receipt; GraphAnswered carries it
        let text = format!("Repository structure (code graph):\n{}", facts.join("\n"));
        return Ok(GraphAnswer {
            query,
            symbol: "repository".into(),
            facts,
            text,
        });
    }
    let (name, rows) = resolve_symbol(intel, &candidates(task))
        .await
        .ok_or_else(|| "no known symbol in the prompt".to_string())?;
    if query != GraphQuery::Definition && rows.len() > 1 {
        return Err(format!(
            "`{name}` names {} symbols; ask about one by file",
            rows.len()
        ));
    }
    let (facts, heading) = match query {
        GraphQuery::Definition => {
            let mut facts: Vec<String> = rows
                .iter()
                .take(MAX_DEFS)
                .map(|r| format!("{} · {} · {}:{}", r.name, r.kind, r.path, r.line))
                .collect();
            if rows.len() > MAX_DEFS {
                facts.push(format!("+{} more", rows.len() - MAX_DEFS));
            }
            (facts, format!("Definition of {name}:"))
        }
        GraphQuery::Callers | GraphQuery::Callees => {
            let direction = if query == GraphQuery::Callers {
                TraceDirection::Inbound
            } else {
                TraceDirection::Outbound
            };
            let nodes = intel
                .trace_ids(&[rows[0].id], direction, 1)
                .await
                .map_err(|e| e.to_string())?;
            let arrow = if direction == TraceDirection::Inbound {
                "←"
            } else {
                "→"
            };
            let facts = capped(
                listed(&nodes, 1)
                    .iter()
                    .map(|n| format!("{name} {arrow} {} ({})", n.name, file_name(&n.file)))
                    .collect(),
            );
            (
                facts,
                format!(
                    "{} of {name} ({}):",
                    if direction == TraceDirection::Inbound {
                        "Callers"
                    } else {
                        "Callees"
                    },
                    rows[0].path
                ),
            )
        }
        GraphQuery::Impact => {
            let nodes = intel
                .trace_ids(&[rows[0].id], TraceDirection::Inbound, 3)
                .await
                .map_err(|e| e.to_string())?;
            let mut classifier = TestClassifier::new(root);
            let is_test: Vec<bool> = nodes
                .iter()
                .map(|n| classifier.is_test(&n.name, &n.file, n.line))
                .collect();
            let tests = nodes
                .iter()
                .zip(&is_test)
                .filter(|(n, t)| n.depth > 0 && **t)
                .count();
            let mut facts: Vec<String> = (1..=3)
                .filter_map(|d| {
                    let at: Vec<String> = nodes
                        .iter()
                        .zip(&is_test)
                        .filter(|(n, t)| n.depth == d && !**t)
                        .map(|(n, _)| n.name.clone())
                        .collect();
                    (!at.is_empty()).then(|| {
                        let shown: Vec<String> = at.iter().take(MAX_LIST).cloned().collect();
                        let more = at.len().saturating_sub(MAX_LIST);
                        let tail = if more > 0 {
                            format!(" +{more} more")
                        } else {
                            String::new()
                        };
                        format!("depth {d}: {}{tail}", shown.join(", "))
                    })
                })
                .collect();
            if tests > 0 {
                facts.push(format!("covered by {tests} tests"));
            }
            (
                facts,
                format!("Changing {name} ({}) affects:", rows[0].path),
            )
        }
        GraphQuery::Structure => unreachable!("handled above"),
    };
    if facts.is_empty() {
        return Err(format!("no {} found for {name}", query.as_str()));
    }
    let text = format!(
        "{heading}\n{}",
        facts
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(GraphAnswer {
        query,
        symbol: name,
        facts,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_core::event::{RouteAnswer, RouteSource};
    use kode_intel::{GraphSymbol, MockCodeIntelligence, TraceNode};
    use kode_local::route::RouteDecision;

    fn decision(answers: Vec<(&str, &str, bool, f32)>) -> RouteDecision {
        RouteDecision {
            tier: String::new(),
            effort: String::new(),
            plan: false,
            answers: answers
                .into_iter()
                .map(|(k, v, laya, c)| RouteAnswer {
                    key: k.into(),
                    value: v.into(),
                    confidence: Some(c),
                    source: if laya {
                        RouteSource::Laya
                    } else {
                        RouteSource::Static("low".into())
                    },
                })
                .collect(),
            probs: vec![],
            device: String::new(),
            latency_ms: 0,
        }
    }

    fn sym(id: i64, name: &str, path: &str, degree: u32) -> GraphSymbol {
        GraphSymbol {
            id,
            name: name.into(),
            kind: "function".into(),
            path: path.into(),
            line: 10,
            line_end: 20,
            degree,
        }
    }

    fn tnode(id: i64, name: &str, file: &str, line: u32, depth: u32) -> TraceNode {
        TraceNode {
            id,
            name: name.into(),
            kind: "function".into(),
            file: file.into(),
            line,
            depth,
        }
    }

    #[test]
    fn wants_graph_requires_both_answers_from_laya() {
        let ok = decision(vec![
            ("answer", "graph", true, 0.9),
            ("graph_query", "callers", true, 0.7),
        ]);
        assert_eq!(wants_graph(&ok, 0.8), Some(GraphQuery::Callers));
        let low = decision(vec![
            ("answer", "graph", true, 0.7),
            ("graph_query", "callers", true, 0.9),
        ]);
        assert_eq!(wants_graph(&low, 0.8), None);
        let static_kind = decision(vec![
            ("answer", "graph", true, 0.95),
            ("graph_query", "definition", false, 0.3),
        ]);
        assert_eq!(wants_graph(&static_kind, 0.8), None);
        let model = decision(vec![
            ("answer", "model", true, 0.99),
            ("graph_query", "callers", true, 0.9),
        ]);
        assert_eq!(wants_graph(&model, 0.8), None);
    }

    #[test]
    fn candidates_prefer_backticks_then_paths_then_identifiers() {
        assert_eq!(
            candidates(
                "who calls `fetch_catalog` and session::append_turn or KodeEvent? Where is Main"
            ),
            vec!["fetch_catalog", "append_turn", "KodeEvent"]
        );
        assert!(candidates("explain how retries work").is_empty());
        assert_eq!(candidates("what calls run_task()"), vec!["run_task"]);
    }

    #[tokio::test]
    async fn resolve_returns_all_exact_rows() {
        let mock = MockCodeIntelligence {
            symbols: vec![
                sym(1, "new", "crates/a.rs", 3),
                sym(2, "new", "crates/b.rs", 40),
                sym(3, "newer", "crates/c.rs", 99),
            ],
            ..Default::default()
        };
        let (name, rows) = resolve_symbol(&mock, &["new".to_string()]).await.unwrap();
        assert_eq!(name, "new");
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn resolve_none_when_no_candidate_exists() {
        let mock = MockCodeIntelligence::default();
        assert!(
            resolve_symbol(&mock, &["ghost".to_string()])
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn exact_symbol_among_many_substring_matches_still_answers() {
        let mut symbols: Vec<GraphSymbol> = (0..40)
            .map(|n| sym(100 + n, &format!("append_turn_{n:02}"), "x.rs", 0))
            .collect();
        symbols.push(sym(1, "append_turn", "crates/kode/src/session.rs", 13));
        let mock = MockCodeIntelligence {
            symbols,
            trace_nodes: vec![tnode(2, "record", "crates/kode/src/tui/state.rs", 5, 1)],
            ..Default::default()
        };
        let a = answer(
            &mock,
            GraphQuery::Callers,
            "who calls append_turn",
            Path::new("."),
        )
        .await
        .unwrap();
        assert_eq!(a.facts, vec!["append_turn ← record (state.rs)"]);
    }

    #[tokio::test]
    async fn ambiguous_symbol_falls_back_for_callers_but_lists_definitions() {
        let mock = MockCodeIntelligence {
            symbols: vec![
                sym(1, "new", "crates/a.rs", 3),
                sym(2, "new", "crates/b.rs", 40),
            ],
            trace_nodes: vec![tnode(3, "x", "y.rs", 1, 1)],
            ..Default::default()
        };
        let err = answer(
            &mock,
            GraphQuery::Callers,
            "who calls `new`",
            Path::new("."),
        )
        .await
        .unwrap_err();
        assert!(err.contains("names 2 symbols"), "{err}");
        let def = answer(
            &mock,
            GraphQuery::Definition,
            "where is `new`",
            Path::new("."),
        )
        .await
        .unwrap();
        assert_eq!(def.facts.len(), 2);
    }

    #[tokio::test]
    async fn impact_counts_inline_module_test_as_test() {
        let root = std::env::temp_dir().join(format!("kode-graph-answer-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "fn target() {}\n\n#[cfg(test)]\nmod tests {\n    fn checks() {\n    }\n}\n",
        )
        .unwrap();
        let mock = MockCodeIntelligence {
            symbols: vec![sym(1, "target", "lib.rs", 1)],
            trace_nodes: vec![tnode(2, "checks", "lib.rs", 5, 1)],
            ..Default::default()
        };
        let a = answer(&mock, GraphQuery::Impact, "impact of `target`", &root)
            .await
            .unwrap();
        assert_eq!(a.facts, vec!["covered by 1 tests"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn callers_answer_lists_depth_one_nodes() {
        let mock = MockCodeIntelligence {
            symbols: vec![sym(1, "append_turn", "crates/kode/src/session.rs", 13)],
            trace_nodes: vec![tnode(
                2,
                "record_completed_turn",
                "crates/kode/src/tui/state.rs",
                9,
                1,
            )],
            ..Default::default()
        };
        let a = answer(
            &mock,
            GraphQuery::Callers,
            "who calls append_turn",
            Path::new("."),
        )
        .await
        .unwrap();
        assert_eq!(a.symbol, "append_turn");
        assert_eq!(
            a.facts,
            vec!["append_turn ← record_completed_turn (state.rs)"]
        );
        assert!(
            a.text
                .starts_with("Callers of append_turn (crates/kode/src/session.rs):")
        );
    }

    #[tokio::test]
    async fn empty_result_is_a_miss_not_an_answer() {
        let mock = MockCodeIntelligence {
            symbols: vec![sym(1, "lonely", "x.rs", 0)],
            ..Default::default()
        };
        let err = answer(
            &mock,
            GraphQuery::Callers,
            "who calls `lonely`",
            Path::new("."),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "no callers found for lonely");
    }

    #[tokio::test]
    async fn no_symbol_is_a_miss() {
        let mock = MockCodeIntelligence::default();
        let err = answer(
            &mock,
            GraphQuery::Definition,
            "where is the thing",
            Path::new("."),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "no known symbol in the prompt");
    }
}
