/// Request for a token-budgeted, task-scoped context blob from zindeks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodeContextRequest {
    pub query: String,
    pub working_set: Vec<String>,
    pub max_tokens: Option<u32>,
}

/// A pre-rendered markdown context blob returned by zindeks `get_context`.
/// The text is passed through as-is; callers should not attempt to parse it.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeContext {
    pub text: String,
    pub token_estimate: u32,
}

/// A single ranked hit from zindeks `search`.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeSearchResult {
    pub path: String,
    pub snippet: String,
    pub score: f64,
}

/// A symbol entry from zindeks `file_outline`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineSymbol {
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub line_end: u32,
}

/// The symbol outline of a single file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileOutline {
    pub path: String,
    pub symbols: Vec<OutlineSymbol>,
}

/// One ranked symbol from zindeks `get_architecture`. `degree` is fan-out
/// or fan-in depending on the list it sits in (0 for entry points).
#[derive(Debug, Clone, PartialEq)]
pub struct ArchSymbol {
    pub name: String,
    pub kind: String,
    /// Repo-relative, forward slashes.
    pub file: String,
    pub degree: u32,
}

/// Whole-repo shape from zindeks `get_architecture`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArchitectureSummary {
    pub total_files: u64,
    pub total_symbols: u64,
    pub total_edges: u64,
    pub entry_points: Vec<ArchSymbol>,
    pub high_fan_out: Vec<ArchSymbol>,
    pub high_fan_in: Vec<ArchSymbol>,
}

/// A symbol row from zindeks `search_graph`.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphSymbol {
    pub name: String,
    pub kind: String,
    /// Repo-relative, forward slashes.
    pub path: String,
    pub line: u32,
    /// `in_degree + out_degree`; used to pick among same-named symbols.
    pub degree: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceDirection {
    Inbound,
    Outbound,
}

impl TraceDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            TraceDirection::Inbound => "inbound",
            TraceDirection::Outbound => "outbound",
        }
    }
}

/// One node of a zindeks `trace_call_path` BFS. The root has depth 0.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceNode {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub depth: u32,
}

/// zindeks server health snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct IntelHealth {
    pub status: String,
    pub documents: u64,
    pub symbols: u64,
    pub edges: u64,
    /// Runtime SQLite version (`health_check.sqlite_version`, zindeks >= 0.10.0).
    /// `None` when the backend payload predates the field (external legacy
    /// servers), so doctor reports "unknown" instead of inventing a version.
    pub sqlite_version: Option<String>,
    /// Runtime SQLite version number, mirrored from
    /// `health_check.sqlite_version_number`.
    pub sqlite_version_number: Option<u32>,
}
