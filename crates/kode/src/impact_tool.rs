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

    /// Impacts for `range` (lines of the pre-edit file at `rel`).
    async fn impacts(
        &self,
        rel: &str,
        range: (u32, u32),
        depth: u32,
    ) -> std::result::Result<Vec<Impact>, String> {
        let outline = self
            .intel
            .file_outline(rel)
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for symbol in impact::changed_symbols(&outline, range) {
            let nodes = self
                .intel
                .trace(&symbol, TraceDirection::Inbound, depth)
                .await
                .map_err(|e| e.to_string())?;
            out.push(impact::summarize(rel, &symbol, &nodes));
        }
        Ok(out)
    }
}

fn rel_path(args: &serde_json::Value) -> Option<String> {
    args.get("path")
        .and_then(serde_json::Value::as_str)
        .map(|p| p.replace('\\', "/"))
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
        let impacts = self.impacts(&rel, range, 3).await.ok()?;
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
        let Some(range) = impact::changed_range(&before, &after) else {
            return Ok(output);
        };
        match self.impacts(&rel, range, 3).await {
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
    use kode_intel::{FileOutline, MockCodeIntelligence, OutlineSymbol, TraceNode};
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

    fn mock_with_callers() -> MockCodeIntelligence {
        MockCodeIntelligence {
            outline: FileOutline {
                path: "lib.rs".into(),
                symbols: vec![OutlineSymbol {
                    name: "fetch".into(),
                    kind: "function".into(),
                    line: 1,
                    line_end: 3,
                }],
            },
            trace_nodes: vec![
                TraceNode {
                    name: "fetch".into(),
                    kind: "function".into(),
                    file: "crates/a/src/lib.rs".into(),
                    depth: 0,
                },
                TraceNode {
                    name: "caller".into(),
                    kind: "function".into(),
                    file: "crates/b/src/x.rs".into(),
                    depth: 1,
                },
                TraceNode {
                    name: "test_fetch".into(),
                    kind: "function".into(),
                    file: "crates/a/src/lib.rs".into(),
                    depth: 1,
                },
            ],
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
}
