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

/// zindeks server health snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct IntelHealth {
    pub status: String,
    pub documents: u64,
    pub symbols: u64,
    pub edges: u64,
}
