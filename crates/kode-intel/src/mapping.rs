//! Shared result mapping for the MCP and embedded zindeks adapters.
//!
//! Both backends speak the same zindeks tool payloads (`health_check`,
//! `search`, `file_outline`, `get_context`) and the same JSON-RPC envelope, so
//! the parsing/normalization lives here once and is reused by both.

use std::path::Path;

use serde_json::Value;

use crate::error::{IntelError, Result};
use crate::types::{
    ArchSymbol, ArchitectureSummary, CodeContext, CodeSearchResult, FileOutline, GraphSymbol,
    IntelHealth, OutlineSymbol, TraceNode,
};

/// Normalizes a filesystem path for cross-plane comparison (`\` vs `/`, case,
/// trailing separators). Used to match a bound root against `list_projects`.
pub(crate) fn normalize_path(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Parses a zindeks tool's JSON text payload, tolerating a known zindeks
/// quirk on Windows: some tools embed raw OS paths without escaping
/// backslashes, producing technically-invalid JSON (e.g. `"C:\Users\..."`).
///
/// Strict parsing is tried first (the common, correct case); only on failure
/// are backslashes that aren't part of a valid JSON escape doubled and retried
/// once. This never changes well-formed payloads and never throws where strict
/// parsing would have succeeded.
pub(crate) fn parse_tool_json(text: &str) -> Result<Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Ok(v);
    }
    let repaired = repair_stray_backslashes(text);
    serde_json::from_str(&repaired)
        .map_err(|e| IntelError::Protocol(format!("invalid tool json: {e}")))
}

fn repair_stray_backslashes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            let valid_simple = matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't');
            let valid_unicode = next == 'u'
                && i + 6 <= chars.len()
                && chars[i + 2..i + 6].iter().all(|c| c.is_ascii_hexdigit());
            if valid_simple || valid_unicode {
                out.push('\\');
            } else {
                out.push('\\');
                out.push('\\');
            }
            out.push(next);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Whether a zindeks tool message means "no project is loaded / indexed".
pub(crate) fn is_not_indexed_error(msg: &str) -> bool {
    msg.contains("No project loaded") || msg.contains("NO_PROJECT")
}

/// Extracts the `content[0].text` payload from a raw JSON-RPC `tools/call`
/// response, mirroring `kode_mcp::McpClient::call_tool` — including mapping a
/// JSON-RPC error or `isError` result to the matching [`IntelError`].
///
/// `root` is only used to phrase the embedded (not-indexed) recovery hint.
pub(crate) fn extract_tool_text(response: &Value, root: &Path) -> Result<String> {
    if let Some(err) = response.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("zindeks request failed");
        if is_not_indexed_error(msg) {
            return Err(IntelError::NotIndexed(root.display().to_string()));
        }
        return Err(IntelError::Tool(msg.to_string()));
    }

    let result = response
        .get("result")
        .ok_or_else(|| IntelError::Protocol("jsonrpc response missing result".to_string()))?;

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str());

    if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        let msg = text.unwrap_or("tool call failed");
        if is_not_indexed_error(msg) {
            return Err(IntelError::NotIndexed(root.display().to_string()));
        }
        return Err(IntelError::Tool(msg.to_string()));
    }

    text.map(|s| s.to_string())
        .ok_or_else(|| IntelError::Protocol("missing content[0].text".to_string()))
}

/// Maps a `health_check` payload to [`IntelHealth`], including the optional
/// `sqlite_version` / `sqlite_version_number` fields added in zindeks 0.10.0.
/// Absent legacy fields map to `None` rather than inventing a version.
pub(crate) fn health_from_value(v: &Value) -> IntelHealth {
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();
    let counts = v.get("counts").cloned().unwrap_or_default();
    let get_count = |key: &str| counts.get(key).and_then(|n| n.as_u64()).unwrap_or(0);

    IntelHealth {
        status,
        documents: get_count("documents"),
        symbols: get_count("symbols"),
        edges: get_count("edges"),
        sqlite_version: v
            .get("sqlite_version")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        sqlite_version_number: v
            .get("sqlite_version_number")
            .and_then(|n| n.as_u64())
            .map(|n| n as u32),
    }
}

/// Maps a `get_context` payload to [`CodeContext`]. The markdown is passed
/// through verbatim.
pub(crate) fn context_from_value(v: &Value) -> CodeContext {
    let text = v
        .get("context")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let token_estimate = v
        .get("token_estimate")
        .and_then(|n| n.as_u64())
        .unwrap_or(0) as u32;
    CodeContext {
        text,
        token_estimate,
    }
}

/// Maps a `search` result array to [`CodeSearchResult`]s.
pub(crate) fn search_from_value(value: &Value) -> Vec<CodeSearchResult> {
    let rows = value.as_array().cloned().unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            let path = row
                .get("p")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            let snippet = row
                .get("x")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            let score = row
                .get("fused_score")
                .and_then(|n| n.as_f64())
                .unwrap_or(0.0);
            CodeSearchResult {
                path,
                snippet,
                score,
            }
        })
        .collect()
}

/// Maps a `file_outline` payload to [`FileOutline`]. `requested_path` is the
/// fallback path when the payload omits it.
pub(crate) fn outline_from_value(v: &Value, requested_path: &str) -> FileOutline {
    let path = v
        .get("path")
        .and_then(|s| s.as_str())
        .unwrap_or(requested_path)
        .to_string();
    let symbols = v
        .get("symbols")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|s| OutlineSymbol {
            name: s
                .get("n")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            kind: s
                .get("k")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            line: s.get("l").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            line_end: s.get("e").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        })
        .collect();
    FileOutline { path, symbols }
}

/// Maps a `get_architecture` payload. Missing sections map to empty lists;
/// paths are normalized to forward slashes for display.
pub(crate) fn architecture_from_value(v: &Value) -> ArchitectureSummary {
    let list = |key: &str, degree_key: Option<&str>| -> Vec<ArchSymbol> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(ArchSymbol {
                            name: item.get("name")?.as_str()?.to_string(),
                            kind: item
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            file: item
                                .get("file")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .replace('\\', "/"),
                            degree: degree_key
                                .and_then(|k| item.get(k))
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as u32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let total = |key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
    ArchitectureSummary {
        total_files: total("total_files"),
        total_symbols: total("total_symbols"),
        total_edges: total("total_edges"),
        entry_points: list("entry_points", None),
        high_fan_out: list("high_fan_out", Some("fan_out")),
        high_fan_in: list("high_fan_in", Some("fan_in")),
    }
}

pub(crate) fn symbols_from_value(v: &Value) -> Vec<GraphSymbol> {
    v.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let num = |k: &str| r.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
                    Some(GraphSymbol {
                        name: r.get("n")?.as_str()?.to_string(),
                        kind: r.get("k").and_then(Value::as_str).unwrap_or("").to_string(),
                        path: r
                            .get("p")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .replace('\\', "/"),
                        line: num("l"),
                        degree: num("in_degree") + num("out_degree"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn trace_from_value(v: &Value) -> Vec<TraceNode> {
    v.get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| {
                    Some(TraceNode {
                        name: n.get("name")?.as_str()?.to_string(),
                        kind: n
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        file: n
                            .get("file_path")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .replace('\\', "/"),
                        depth: n.get("depth").and_then(Value::as_u64).unwrap_or(0) as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repairs_stray_backslashes_without_touching_valid_json() {
        assert_eq!(parse_tool_json(r#"{"a":"b"}"#).unwrap(), json!({"a":"b"}));
        let repaired = parse_tool_json(r#"{"p":"C:\Users\x"}"#).unwrap();
        assert_eq!(repaired["p"], r"C:\Users\x");
    }

    #[test]
    fn health_maps_sqlite_fields_when_present() {
        let v = json!({
            "status": "healthy",
            "counts": {"documents": 3, "symbols": 9, "edges": 12},
            "sqlite_version": "3.53.4",
            "sqlite_version_number": 3053004
        });
        let h = health_from_value(&v);
        assert_eq!(h.documents, 3);
        assert_eq!(h.sqlite_version.as_deref(), Some("3.53.4"));
        assert_eq!(h.sqlite_version_number, Some(3053004));
    }

    #[test]
    fn health_maps_legacy_payload_without_sqlite_fields_to_none() {
        let v = json!({"status": "healthy", "counts": {"documents": 1}});
        let h = health_from_value(&v);
        assert_eq!(h.sqlite_version, None);
        assert_eq!(h.sqlite_version_number, None);
    }

    #[test]
    fn tool_error_envelope_maps_to_tool_error() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "boom"}], "isError": true}
        });
        let err = extract_tool_text(&resp, Path::new("/repo")).unwrap_err();
        assert!(matches!(err, IntelError::Tool(m) if m == "boom"));
    }

    #[test]
    fn no_project_error_maps_to_embedded_not_indexed() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "No project loaded"}], "isError": true}
        });
        let err = extract_tool_text(&resp, Path::new("/repo")).unwrap_err();
        assert!(matches!(err, IntelError::NotIndexed(_)));
    }

    #[test]
    fn architecture_maps_totals_and_ranked_symbols() {
        let v: Value = serde_json::from_str(
            r#"{"total_files":132,"total_symbols":2624,"total_edges":6029,
                "entry_points":[{"name":"main","kind":"function","file":"crates\\kode\\src\\main.rs"}],
                "high_fan_out":[{"name":"execute_task","kind":"function","file":"crates\\kode\\src\\pipeline.rs","fan_out":81}],
                "high_fan_in":[{"name":"apply_event","kind":"function","file":"crates\\kode\\src\\tui\\events.rs","fan_in":104}]}"#,
        )
        .unwrap();
        let a = architecture_from_value(&v);
        assert_eq!(
            (a.total_files, a.total_symbols, a.total_edges),
            (132, 2624, 6029)
        );
        assert_eq!(a.entry_points[0].name, "main");
        assert_eq!(a.entry_points[0].file, "crates/kode/src/main.rs");
        assert_eq!(a.high_fan_out[0].degree, 81);
        assert_eq!(a.high_fan_in[0].degree, 104);
    }

    #[test]
    fn architecture_tolerates_missing_sections() {
        let a = architecture_from_value(&serde_json::json!({}));
        assert_eq!(a.total_files, 0);
        assert!(a.high_fan_in.is_empty());
    }

    #[test]
    fn symbols_map_compact_search_graph_rows() {
        let v: Value = serde_json::from_str(
            r#"[{"id":1,"n":"append_turn","k":"function","l":169,"e":174,"p":"crates\\kode\\src\\session.rs","out_degree":1,"in_degree":12}]"#,
        )
        .unwrap();
        let rows = symbols_from_value(&v);
        assert_eq!(
            rows,
            vec![GraphSymbol {
                name: "append_turn".into(),
                kind: "function".into(),
                path: "crates/kode/src/session.rs".into(),
                line: 169,
                degree: 13,
            }]
        );
    }

    #[test]
    fn trace_maps_nodes_with_depth() {
        let v: Value = serde_json::from_str(
            r#"{"has_cycle":false,"nodes":[
                {"name":"append_turn","kind":"function","file_path":"crates\\kode\\src\\session.rs","depth":0},
                {"name":"record_completed_turn","kind":"function","file_path":"crates\\kode\\src\\tui\\state.rs","depth":1}],
              "edges":[]}"#,
        )
        .unwrap();
        let nodes = trace_from_value(&v);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].name, "record_completed_turn");
        assert_eq!(nodes[1].file, "crates/kode/src/tui/state.rs");
        assert_eq!(nodes[1].depth, 1);
    }
}
