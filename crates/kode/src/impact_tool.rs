//! Decorates edit tools with blast radius: after a successful edit, emit
//! `Impact`, append callers to the tool result, and remember covering
//! tests for targeted verification.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kode_core::event::{EventBus, KodeEvent};
use kode_intel::{CodeIntelligence, TraceDirection};
use kode_tools::{RequiredPermission, Result, Tool, ToolContext, ToolOutput};

use crate::impact::{self, Impact};
use crate::test_symbols::TestClassifier;

/// Per-task impact state shared by every decorated tool.
#[derive(Default, Clone)]
pub struct ImpactLog {
    tests: Arc<Mutex<Vec<(String, String)>>>,
    noted_unavailable: Arc<AtomicBool>,
}

impl ImpactLog {
    /// Distinct `(test name, file)` pairs reached by this task's edits.
    pub fn covering_tests(&self) -> Vec<(String, String)> {
        self.tests.lock().map(|t| t.clone()).unwrap_or_default()
    }

    fn add_tests(&self, found: &[(String, String)]) {
        if let Ok(mut tests) = self.tests.lock() {
            for t in found {
                if !tests.contains(t) {
                    tests.push(t.clone());
                }
            }
        }
    }
}

pub struct ImpactAwareTool {
    inner: Arc<dyn Tool>,
    intel: Arc<dyn CodeIntelligence>,
    events: EventBus,
    log: ImpactLog,
}

impl ImpactAwareTool {
    pub fn new(
        inner: Arc<dyn Tool>,
        intel: Arc<dyn CodeIntelligence>,
        events: EventBus,
        log: ImpactLog,
    ) -> Self {
        Self {
            inner,
            intel,
            events,
            log,
        }
    }

    fn note_unavailable(&self, reason: &str) {
        if !self.log.noted_unavailable.swap(true, Ordering::Relaxed) {
            self.events.emit(KodeEvent::Note {
                text: format!("impact unavailable: {reason}"),
            });
        }
    }

    /// Impacts for `range` (lines of the file at `rel`, in the coordinates
    /// the index currently describes).
    async fn impacts(
        &self,
        root: &Path,
        rel: &str,
        range: (u32, u32),
        depth: u32,
    ) -> std::result::Result<Vec<Impact>, String> {
        let rel = index_path(root, rel);
        let outline = self
            .intel
            .file_outline(&rel)
            .await
            .map_err(|e| e.to_string())?;
        if outline.symbols.is_empty() && is_code_file(&rel) {
            return Err(format!("no indexed symbols for {rel}"));
        }
        let mut classifier = TestClassifier::new(root);
        let mut out = Vec::new();
        for symbol in impact::changed_symbols(&outline, range) {
            let rows = self
                .intel
                .exact_symbols(&symbol, Some(&rel))
                .await
                .map_err(|e| e.to_string())?;
            let overlapping: Vec<_> = rows
                .iter()
                .filter(|r| r.line <= range.1 && r.line_end >= range.0)
                .collect();
            let chosen: Vec<_> = if overlapping.is_empty() {
                rows.iter().collect()
            } else {
                overlapping
            };
            if chosen.is_empty() {
                continue;
            }
            let ids: Vec<i64> = chosen.iter().map(|r| r.id).collect();
            let nodes = self
                .intel
                .trace_ids(&ids, TraceDirection::Inbound, depth)
                .await
                .map_err(|e| e.to_string())?;
            out.push(impact::summarize(&rel, &symbol, &nodes, &mut |n| {
                classifier.is_test(&n.name, &n.file, n.line)
            }));
        }
        Ok(out)
    }
}

fn rel_path(args: &serde_json::Value) -> Option<String> {
    args.get("path")
        .and_then(serde_json::Value::as_str)
        .map(|p| p.replace('\\', "/"))
}

/// Repo-relative, `/`-separated path as the index spells it: absolute paths
/// under `root` are made relative and leading `./` is dropped.
fn index_path(root: &Path, raw: &str) -> String {
    let raw = raw.replace('\\', "/");
    let p = Path::new(&raw);
    if p.is_absolute()
        && let Ok(rel) = p.strip_prefix(root)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }
    let mut out = raw.as_str();
    while let Some(rest) = out.strip_prefix("./") {
        out = rest;
    }
    out.to_string()
}

/// Whether `path` has an extension the indexer extracts symbols from.
fn is_code_file(path: &str) -> bool {
    const EXTS: &[&str] = &[
        "rs", "py", "js", "jsx", "ts", "tsx", "go", "java", "c", "h", "cc", "cpp", "hpp", "zig",
    ];
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTS.contains(&ext))
}

fn read(root: &Path, rel: &str) -> Option<String> {
    let path = kode_tools::path::resolve_in_workspace(root, rel).ok()?;
    std::fs::read_to_string(path).ok()
}

#[async_trait::async_trait]
impl Tool for ImpactAwareTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> serde_json::Value {
        self.inner.parameters()
    }
    fn required_permission(&self) -> RequiredPermission {
        self.inner.required_permission()
    }
    fn output_mutated(&self, output: &ToolOutput) -> bool {
        self.inner.output_mutated(output)
    }

    async fn permission_note(&self, args: &serde_json::Value, ctx: &ToolContext) -> Option<String> {
        // Only an apply_patch target can be located before writing; never guess.
        let rel = rel_path(args)?;
        let old_string = args.get("old_string")?.as_str()?;
        let current = read(&ctx.workspace_root, &rel)?;
        let range = impact::range_of_substring(&current, old_string)?;
        let _ = self.intel.refresh().await; // best effort: fresh outline
        let impacts = self
            .impacts(&ctx.workspace_root, &rel, range, 3)
            .await
            .ok()?;
        let rows: Vec<String> = impacts.iter().map(impact::row_text).collect();
        (!rows.is_empty()).then(|| rows.join("\n"))
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let rel = rel_path(&args);
        let before = rel.as_deref().and_then(|r| read(&ctx.workspace_root, r));
        let mut output = self.inner.execute(args, ctx).await?;
        let (Some(rel), Some(before)) = (rel, before) else {
            return Ok(output); // new file: nothing in the graph yet
        };
        let Some(after) = read(&ctx.workspace_root, &rel) else {
            return Ok(output);
        };
        // A refreshed index describes the new file; otherwise fall back to
        // pre-edit coordinates.
        let range = match self.intel.refresh().await {
            Ok(()) => impact::new_range(&before, &after),
            Err(_) => impact::changed_range(&before, &after),
        };
        let Some(range) = range else {
            return Ok(output);
        };
        match self.impacts(&ctx.workspace_root, &rel, range, 3).await {
            Ok(impacts) => {
                for i in &impacts {
                    self.events.emit(KodeEvent::Impact {
                        file: i.file.clone(),
                        symbol: i.symbol.clone(),
                        callers: i.callers,
                        crates: i.crates,
                        tests: i.tests.len() as u32,
                    });
                    self.log.add_tests(&i.tests);
                }
                output.content.push_str(&impact::tool_result_text(&impacts));
            }
            Err(reason) => self.note_unavailable(&reason),
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_intel::{FileOutline, GraphSymbol, MockCodeIntelligence, OutlineSymbol, TraceNode};
    use kode_tools::tools::{ApplyPatch, WriteFile};

    fn workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-impact-tool-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(root: &std::path::Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            cancel: kode_core::CancellationToken::new(),
        }
    }

    fn gsym(id: i64, name: &str, line: u32, line_end: u32) -> GraphSymbol {
        GraphSymbol {
            id,
            name: name.into(),
            kind: "function".into(),
            path: "lib.rs".into(),
            line,
            line_end,
            degree: 1,
        }
    }

    fn osym(name: &str, line: u32, line_end: u32) -> OutlineSymbol {
        OutlineSymbol {
            name: name.into(),
            kind: "function".into(),
            line,
            line_end,
        }
    }

    fn caller_nodes() -> Vec<TraceNode> {
        vec![
            TraceNode {
                id: 10,
                name: "caller".into(),
                kind: "function".into(),
                file: "crates/b/src/x.rs".into(),
                line: 1,
                depth: 1,
            },
            TraceNode {
                id: 11,
                name: "test_fetch".into(),
                kind: "function".into(),
                file: "crates/a/src/lib.rs".into(),
                line: 1,
                depth: 1,
            },
        ]
    }

    fn mock_with_callers() -> MockCodeIntelligence {
        MockCodeIntelligence {
            outline: FileOutline {
                path: "lib.rs".into(),
                symbols: vec![osym("fetch", 1, 3)],
            },
            symbols: vec![gsym(1, "fetch", 1, 3)],
            trace_nodes: caller_nodes(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn successful_patch_emits_impact_and_appends_callers() {
        let root = workspace();
        std::fs::write(root.join("lib.rs"), "fn fetch() {\n    1\n}\n").unwrap();
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let log = ImpactLog::default();
        let tool = ImpactAwareTool::new(
            Arc::new(ApplyPatch),
            Arc::new(mock_with_callers()),
            bus,
            log.clone(),
        );
        let out = tool
            .execute(
                serde_json::json!({"path": "lib.rs", "old_string": "    1", "new_string": "    2"}),
                &ctx(&root),
            )
            .await
            .unwrap();
        assert!(
            out.content
                .contains("impact: fetch has 1 caller across 1 crate:")
        );
        match rx.try_recv().unwrap() {
            KodeEvent::Impact {
                symbol,
                callers,
                tests,
                ..
            } => {
                assert_eq!((symbol.as_str(), callers, tests), ("fetch", 1, 1));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            log.covering_tests(),
            vec![("test_fetch".to_string(), "crates/a/src/lib.rs".to_string())]
        );
    }

    #[tokio::test]
    async fn new_file_has_no_impact() {
        let root = workspace();
        let bus = EventBus::new(4);
        let mut rx = bus.subscribe();
        let tool = ImpactAwareTool::new(
            Arc::new(WriteFile),
            Arc::new(mock_with_callers()),
            bus,
            ImpactLog::default(),
        );
        let out = tool
            .execute(
                serde_json::json!({"path": "new.rs", "content": "x"}),
                &ctx(&root),
            )
            .await
            .unwrap();
        assert!(!out.content.contains("impact:"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn engine_error_notes_once_per_task() {
        let root = workspace();
        std::fs::write(root.join("lib.rs"), "a\nb\n").unwrap();
        let broken = MockCodeIntelligence {
            outline_error: Some("not indexed".into()),
            ..Default::default()
        };
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        let tool = ImpactAwareTool::new(
            Arc::new(ApplyPatch),
            Arc::new(broken),
            bus,
            ImpactLog::default(),
        );
        for (old, new) in [("a", "c"), ("b", "d")] {
            tool.execute(
                serde_json::json!({"path": "lib.rs", "old_string": old, "new_string": new}),
                &ctx(&root),
            )
            .await
            .unwrap();
        }
        let notes: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(
                |e| matches!(e, KodeEvent::Note { text } if text.starts_with("impact unavailable")),
            )
            .collect();
        assert_eq!(notes.len(), 1);
    }

    #[tokio::test]
    async fn permission_note_previews_impact_before_writing() {
        let root = workspace();
        std::fs::write(root.join("lib.rs"), "fn fetch() {\n    1\n}\n").unwrap();
        let tool = ImpactAwareTool::new(
            Arc::new(ApplyPatch),
            Arc::new(mock_with_callers()),
            EventBus::new(4),
            ImpactLog::default(),
        );
        let note = tool
            .permission_note(
                &serde_json::json!({"path": "lib.rs", "old_string": "    1", "new_string": "    2"}),
                &ctx(&root),
            )
            .await;
        assert_eq!(
            note.as_deref(),
            Some("impact · fetch ← 1 caller, 1 crate, 1 test")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("lib.rs")).unwrap(),
            "fn fetch() {\n    1\n}\n"
        );
    }

    fn impact_symbols(rx: &mut tokio::sync::broadcast::Receiver<KodeEvent>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                KodeEvent::Impact { symbol, .. } => Some(symbol),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn dot_slash_path_still_produces_impact() {
        let root = workspace();
        std::fs::write(root.join("lib.rs"), "fn fetch() {\n    1\n}\n").unwrap();
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let tool = ImpactAwareTool::new(
            Arc::new(ApplyPatch),
            Arc::new(mock_with_callers()),
            bus,
            ImpactLog::default(),
        );
        tool.execute(
            serde_json::json!({"path": "./lib.rs", "old_string": "    1", "new_string": "    2"}),
            &ctx(&root),
        )
        .await
        .unwrap();
        assert_eq!(impact_symbols(&mut rx), vec!["fetch"]);
    }

    fn three_fn_mock() -> MockCodeIntelligence {
        MockCodeIntelligence {
            outline: FileOutline {
                path: "lib.rs".into(),
                symbols: vec![osym("a", 5, 7), osym("b", 9, 11), osym("c", 13, 15)],
            },
            symbols: vec![
                gsym(1, "a", 5, 7),
                gsym(2, "b", 9, 11),
                gsym(3, "c", 13, 15),
            ],
            trace_nodes: caller_nodes(),
            ..Default::default()
        }
    }

    const STALE_FILE: &str =
        "// 1\n// 2\n// 3\n// 4\nfn a() {\n    1\n}\n\nfn b() {\n    2\n}\n\nfn c() {\n    3\n}\n";

    #[tokio::test]
    async fn refreshed_outline_matches_new_file_coordinates() {
        let root = workspace();
        std::fs::write(root.join("lib.rs"), STALE_FILE).unwrap();
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let tool = ImpactAwareTool::new(
            Arc::new(ApplyPatch),
            Arc::new(three_fn_mock()),
            bus,
            ImpactLog::default(),
        );
        tool.execute(
            serde_json::json!({"path": "lib.rs", "old_string": "    2", "new_string": "    22"}),
            &ctx(&root),
        )
        .await
        .unwrap();
        assert_eq!(impact_symbols(&mut rx), vec!["b"]);
    }

    #[tokio::test]
    async fn refresh_failure_falls_back_to_before_coordinates() {
        let root = workspace();
        std::fs::write(root.join("lib.rs"), "fn fetch() {\n    1\n}\n").unwrap();
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let mock = MockCodeIntelligence {
            refresh_error: Some("no refresh".into()),
            ..mock_with_callers()
        };
        let tool = ImpactAwareTool::new(
            Arc::new(ApplyPatch),
            Arc::new(mock),
            bus,
            ImpactLog::default(),
        );
        tool.execute(
            serde_json::json!({"path": "lib.rs", "old_string": "    1", "new_string": "    2"}),
            &ctx(&root),
        )
        .await
        .unwrap();
        assert_eq!(impact_symbols(&mut rx), vec!["fetch"]);
    }

    #[tokio::test]
    async fn empty_outline_for_code_file_notes_once() {
        let root = workspace();
        std::fs::write(root.join("lib.rs"), "a\nb\n").unwrap();
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        let tool = ImpactAwareTool::new(
            Arc::new(ApplyPatch),
            Arc::new(MockCodeIntelligence::default()),
            bus,
            ImpactLog::default(),
        );
        for (old, new) in [("a", "c"), ("b", "d")] {
            tool.execute(
                serde_json::json!({"path": "lib.rs", "old_string": old, "new_string": new}),
                &ctx(&root),
            )
            .await
            .unwrap();
        }
        let notes: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                KodeEvent::Note { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            notes,
            vec!["impact unavailable: no indexed symbols for lib.rs"]
        );
    }

    #[test]
    fn index_path_normalizes_dot_slash_and_absolute() {
        let root = std::env::temp_dir().join("repo");
        let root = root.as_path();
        assert_eq!(index_path(root, "./a\\b.rs"), "a/b.rs");
        assert_eq!(index_path(root, "././a.rs"), "a.rs");
        let abs = root.join("src").join("a.rs");
        assert_eq!(index_path(root, &abs.to_string_lossy()), "src/a.rs");
        assert!(is_code_file("x.rs") && !is_code_file("README.md"));
    }
}
