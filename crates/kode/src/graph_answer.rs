//! Answers pure code-graph lookups without a model call, when the local
//! router is confident. Every miss is an `Err(reason)` the caller turns
//! into a sourced note before falling back to the model.

use kode_intel::{CodeIntelligence, GraphSymbol, TraceDirection, TraceNode};
use kode_local::route::RouteDecision;

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

/// First candidate with an exact-name match; among same-named symbols the
/// highest graph degree wins.
pub async fn resolve_symbol(
    intel: &dyn CodeIntelligence,
    candidates: &[String],
) -> Option<GraphSymbol> {
    for name in candidates {
        let Ok(rows) = intel.symbols(name, 20).await else {
            continue;
        };
        if let Some(best) = rows
            .into_iter()
            .filter(|r| &r.name == name)
            .max_by_key(|r| r.degree)
        {
            return Some(best);
        }
    }
    None
}

pub fn is_test_symbol(name: &str, file: &str) -> bool {
    let file_name = file.rsplit('/').next().unwrap_or(file);
    name.starts_with("test_")
        || name.starts_with("Test")
        || file.contains("/tests/")
        || file.starts_with("tests/")
        || file_name == "tests.rs"
        || file_name.ends_with("_test.go")
        || file_name.ends_with("_test.py")
        || (file_name.starts_with("test_") && file_name.ends_with(".py"))
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
    let symbol = resolve_symbol(intel, &candidates(task))
        .await
        .ok_or_else(|| "no known symbol in the prompt".to_string())?;
    let name = symbol.name.clone();
    let (facts, heading) = match query {
        GraphQuery::Definition => {
            let rows = intel.symbols(&name, 20).await.map_err(|e| e.to_string())?;
            let facts: Vec<String> = rows
                .iter()
                .filter(|r| r.name == name)
                .take(MAX_DEFS)
                .map(|r| format!("{} · {} · {}:{}", r.name, r.kind, r.path, r.line))
                .collect();
            (facts, format!("Definition of {name}:"))
        }
        GraphQuery::Callers | GraphQuery::Callees => {
            let direction = if query == GraphQuery::Callers {
                TraceDirection::Inbound
            } else {
                TraceDirection::Outbound
            };
            let nodes = intel
                .trace(&name, direction, 1)
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
                    symbol.path
                ),
            )
        }
        GraphQuery::Impact => {
            let nodes = intel
                .trace(&name, TraceDirection::Inbound, 3)
                .await
                .map_err(|e| e.to_string())?;
            let tests = nodes
                .iter()
                .filter(|n| n.depth > 0 && is_test_symbol(&n.name, &n.file))
                .count();
            let mut facts: Vec<String> = (1..=3)
                .filter_map(|d| {
                    let at: Vec<String> = listed(&nodes, d)
                        .iter()
                        .filter(|n| !is_test_symbol(&n.name, &n.file))
                        .map(|n| n.name.clone())
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
            (facts, format!("Changing {name} ({}) affects:", symbol.path))
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

    fn sym(name: &str, path: &str, degree: u32) -> GraphSymbol {
        GraphSymbol {
            name: name.into(),
            kind: "function".into(),
            path: path.into(),
            line: 10,
            degree,
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
    async fn resolve_prefers_exact_highest_degree() {
        let mock = MockCodeIntelligence {
            symbols: vec![
                sym("new", "crates/a.rs", 3),
                sym("new", "crates/b.rs", 40),
                sym("newer", "crates/c.rs", 99),
            ],
            ..Default::default()
        };
        let got = resolve_symbol(&mock, &["new".to_string()]).await.unwrap();
        assert_eq!(got.path, "crates/b.rs");
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
    async fn callers_answer_lists_depth_one_nodes() {
        let mock = MockCodeIntelligence {
            symbols: vec![sym("append_turn", "crates/kode/src/session.rs", 13)],
            trace_nodes: vec![
                TraceNode {
                    name: "append_turn".into(),
                    kind: "function".into(),
                    file: "crates/kode/src/session.rs".into(),
                    depth: 0,
                },
                TraceNode {
                    name: "record_completed_turn".into(),
                    kind: "function".into(),
                    file: "crates/kode/src/tui/state.rs".into(),
                    depth: 1,
                },
            ],
            ..Default::default()
        };
        let a = answer(&mock, GraphQuery::Callers, "who calls append_turn")
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
            symbols: vec![sym("lonely", "x.rs", 0)],
            ..Default::default()
        };
        let err = answer(&mock, GraphQuery::Callers, "who calls `lonely`")
            .await
            .unwrap_err();
        assert_eq!(err, "no callers found for lonely");
    }

    #[tokio::test]
    async fn no_symbol_is_a_miss() {
        let mock = MockCodeIntelligence::default();
        let err = answer(&mock, GraphQuery::Definition, "where is the thing")
            .await
            .unwrap_err();
        assert_eq!(err, "no known symbol in the prompt");
    }

    #[test]
    fn test_symbol_detection() {
        assert!(is_test_symbol("test_parse", "src/lib.rs"));
        assert!(is_test_symbol("parses", "crates/kode/tests/status.rs"));
        assert!(is_test_symbol("parses", "crates/kode/src/tui/tests.rs"));
        assert!(is_test_symbol("TestFoo", "pkg/foo_test.go"));
        assert!(is_test_symbol("check", "tests/test_api.py"));
        assert!(!is_test_symbol(
            "execute_task",
            "crates/kode/src/pipeline.rs"
        ));
    }
}
