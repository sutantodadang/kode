//! Blast radius of an edit, from the outline and the call graph.

use std::collections::BTreeSet;

use kode_intel::{FileOutline, TraceNode};

const MAX_SYMBOLS: usize = 5;
const MAX_SITES: usize = 10;
const CALLABLE: &[&str] = &["function", "method"];

pub fn changed_range(old: &str, new: &str) -> Option<(u32, u32)> {
    if old == new {
        return None;
    }
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let start = prefix as u32 + 1;
    // Pure insertion changes no old line; anchor on the old line that follows it.
    let end = (a.len() - suffix).max(prefix + 1) as u32;
    Some((start.min(end), end))
}

/// Like [`changed_range`] but in NEW-file line coordinates: the lines of
/// `new` that differ from `old` (a pure deletion anchors on the line that
/// now follows it).
pub fn new_range(old: &str, new: &str) -> Option<(u32, u32)> {
    if old == new {
        return None;
    }
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let start = prefix as u32 + 1;
    let end = (b.len() - suffix).max(prefix + 1) as u32;
    Some((start.min(end), end))
}

pub fn range_of_substring(text: &str, needle: &str) -> Option<(u32, u32)> {
    let offset = text.find(needle)?;
    let start = text[..offset].matches('\n').count() as u32 + 1;
    let end = start + needle.trim_end_matches('\n').matches('\n').count() as u32;
    Some((start, end))
}

pub fn changed_symbols(outline: &FileOutline, (start, end): (u32, u32)) -> Vec<String> {
    let mut hits: Vec<_> = outline
        .symbols
        .iter()
        .filter(|s| s.line <= end && s.line_end >= start)
        .collect();
    let callables: Vec<_> = hits
        .iter()
        .copied()
        .filter(|s| CALLABLE.contains(&s.kind.as_str()))
        .collect();
    if !callables.is_empty() {
        hits = callables;
    } else {
        // Containers (impl, module) only when nothing smaller overlaps.
        hits.sort_by_key(|s| s.line_end - s.line);
        hits.retain(|s| !matches!(s.kind.as_str(), "impl" | "module"));
    }
    hits.sort_by_key(|s| s.line_end - s.line);
    let mut names = Vec::new();
    for s in hits {
        if !names.contains(&s.name) {
            names.push(s.name.clone());
        }
        if names.len() == MAX_SYMBOLS {
            break;
        }
    }
    names
}

#[derive(Debug, Clone, PartialEq)]
pub struct Impact {
    pub file: String,
    pub symbol: String,
    pub callers: u32,
    pub crates: u32,
    /// `(name, file)` of tests that reach the symbol within depth 3.
    pub tests: Vec<(String, String)>,
    pub caller_sites: Vec<String>,
}

/// `crates/<name>/…` → `<name>`, else the first path segment.
fn package_of(file: &str) -> &str {
    let mut parts = file.split('/');
    match (parts.next(), parts.next()) {
        (Some("crates"), Some(name)) => name,
        (Some(first), _) => first,
        _ => file,
    }
}

pub fn summarize(
    file: &str,
    symbol: &str,
    nodes: &[TraceNode],
    is_test: &mut dyn FnMut(&TraceNode) -> bool,
) -> Impact {
    let flags: Vec<bool> = nodes.iter().map(is_test).collect();
    let callers: Vec<&TraceNode> = nodes
        .iter()
        .zip(&flags)
        .filter(|(n, t)| n.depth == 1 && !**t)
        .map(|(n, _)| n)
        .collect();
    let crates: BTreeSet<&str> = callers.iter().map(|n| package_of(&n.file)).collect();
    let tests = nodes
        .iter()
        .zip(&flags)
        .filter(|(n, t)| n.depth > 0 && **t)
        .map(|(n, _)| (n.name.clone(), n.file.clone()))
        .collect();
    Impact {
        file: file.to_string(),
        symbol: symbol.to_string(),
        callers: callers.len() as u32,
        crates: crates.len() as u32,
        tests,
        caller_sites: callers
            .iter()
            .map(|n| format!("{} ({})", n.name, n.file))
            .collect(),
    }
}

fn plural(n: u32, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

pub fn row_text_parts(symbol: &str, callers: u32, crates: u32, tests: u32) -> String {
    format!(
        "impact · {symbol} ← {}, {}, {}",
        plural(callers, "caller"),
        plural(crates, "crate"),
        plural(tests, "test")
    )
}

pub fn row_text(i: &Impact) -> String {
    row_text_parts(&i.symbol, i.callers, i.crates, i.tests.len() as u32)
}

pub fn tool_result_text(impacts: &[Impact]) -> String {
    let mut out = String::new();
    let mut budget = MAX_SITES;
    for i in impacts.iter().filter(|i| i.callers > 0) {
        out.push_str(&format!(
            "\n\nimpact: {} has {} across {}:",
            i.symbol,
            plural(i.callers, "caller"),
            plural(i.crates, "crate")
        ));
        let shown = i.caller_sites.len().min(budget);
        for site in &i.caller_sites[..shown] {
            out.push_str(&format!("\n  {site}"));
        }
        budget -= shown;
        if i.caller_sites.len() > shown {
            out.push_str(&format!("\n  +{} more", i.caller_sites.len() - shown));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_intel::{FileOutline, OutlineSymbol, TraceNode};

    fn sym(name: &str, kind: &str, line: u32, line_end: u32) -> OutlineSymbol {
        OutlineSymbol {
            name: name.into(),
            kind: kind.into(),
            line,
            line_end,
        }
    }

    fn node(name: &str, file: &str, depth: u32) -> TraceNode {
        TraceNode {
            id: 0,
            name: name.into(),
            kind: "function".into(),
            file: file.into(),
            line: 1,
            depth,
        }
    }

    fn by_name(n: &TraceNode) -> bool {
        crate::test_symbols::is_test_symbol(&n.name, &n.file)
    }

    #[test]
    fn new_range_is_in_new_file_coordinates() {
        let old = "a\nb\nc\nd\n";
        assert_eq!(new_range(old, "a\nB\nc\nd\n"), Some((2, 2)));
        assert_eq!(new_range(old, "a\nb\nX\nY\nc\nd\n"), Some((3, 4)));
        assert_eq!(new_range(old, "a\nd\n"), Some((2, 2)));
        assert_eq!(new_range(old, old), None);
    }

    #[test]
    fn changed_range_finds_the_edited_lines_in_the_old_text() {
        let old = "a\nb\nc\nd\n";
        assert_eq!(changed_range(old, "a\nB\nc\nd\n"), Some((2, 2)));
        assert_eq!(changed_range(old, "a\nb\nX\nY\nc\nd\n"), Some((3, 3)));
        assert_eq!(changed_range(old, "a\nd\n"), Some((2, 3)));
        assert_eq!(changed_range(old, old), None);
    }

    #[test]
    fn range_of_substring_maps_to_lines() {
        assert_eq!(range_of_substring("a\nb\nc\nd\n", "b\nc"), Some((2, 3)));
        assert_eq!(range_of_substring("a\nb\n", "zzz"), None);
    }

    #[test]
    fn changed_symbols_prefers_innermost_function() {
        let outline = FileOutline {
            path: "x.rs".into(),
            symbols: vec![
                sym("Catalog", "struct_type", 1, 3),
                sym("impl Catalog", "impl", 5, 40),
                sym("fetch", "method", 10, 20),
                sym("tests", "module", 50, 90),
                sym("fetch_retries", "function", 60, 70),
            ],
        };
        assert_eq!(changed_symbols(&outline, (12, 14)), vec!["fetch"]);
        assert_eq!(changed_symbols(&outline, (62, 62)), vec!["fetch_retries"]);
        assert_eq!(changed_symbols(&outline, (2, 2)), vec!["Catalog"]);
        assert!(changed_symbols(&outline, (45, 46)).is_empty());
    }

    #[test]
    fn summarize_counts_callers_crates_and_tests() {
        let nodes = vec![
            node("fetch", "crates/kode-context/src/catalog.rs", 0),
            node("run_task", "crates/kode/src/pipeline.rs", 1),
            node("compile", "crates/kode-context/src/compile.rs", 1),
            node(
                "test_fetch_retries",
                "crates/kode-context/src/catalog.rs",
                1,
            ),
            node("main", "crates/kode/src/main.rs", 2),
            node("test_end_to_end", "crates/kode/tests/e2e.rs", 3),
        ];
        let i = summarize(
            "crates/kode-context/src/catalog.rs",
            "fetch",
            &nodes,
            &mut by_name,
        );
        assert_eq!(i.callers, 2); // depth-1, non-test
        assert_eq!(i.crates, 2); // kode, kode-context
        assert_eq!(i.tests.len(), 2);
        assert_eq!(
            row_text(&i),
            "impact · fetch ← 2 callers, 2 crates, 2 tests"
        );
        assert_eq!(
            i.caller_sites,
            vec![
                "run_task (crates/kode/src/pipeline.rs)",
                "compile (crates/kode-context/src/compile.rs)"
            ]
        );
    }

    #[test]
    fn summarize_uses_the_closure_for_inline_module_tests() {
        let nodes = vec![node("checks", "crates/a/src/lib.rs", 1)];
        let i = summarize("crates/a/src/lib.rs", "f", &nodes, &mut |n| {
            n.name == "checks"
        });
        assert_eq!((i.callers, i.tests.len()), (0, 1));
    }

    #[test]
    fn tool_result_text_caps_sites_at_ten() {
        let i = Impact {
            file: "f.rs".into(),
            symbol: "f".into(),
            callers: 14,
            crates: 1,
            tests: vec![],
            caller_sites: (0..14).map(|n| format!("c{n} (f.rs)")).collect(),
        };
        let text = tool_result_text(&[i]);
        assert!(text.starts_with("\n\nimpact: f has 14 callers across 1 crate:"));
        assert_eq!(text.lines().filter(|l| l.starts_with("  c")).count(), 10);
        assert!(text.contains("  +4 more"));
    }
}
