//! In-process zindeks backend.
//!
//! Loads Kode's pinned `zindeks` shared library through `libloading` and drives
//! the same JSON-RPC engine the stdio MCP adapter speaks — but with no child
//! process, no MCP server and no TCP port. Engine parsing, graph/search logic,
//! index writer locks and watcher close ordering are unchanged; only the
//! transport is replaced by the checked C ABI (version 1).
//!
//! Ownership / threading model:
//!
//! * Each [`EmbeddedZindeks`] owns one **dedicated engine thread**. That thread
//!   loads the library, opens the handle, runs every ABI call and closes the
//!   handle — all on the same OS thread. This is required: the engine's
//!   SQLite/tree-sitter state is created on the opening thread and must not be
//!   used from another thread, and the pool thread stack is too small for the
//!   parser's deep recursion (the worker gets [`ENGINE_STACK_BYTES`]).
//! * Because the engine never leaves its thread, no raw pointer is `Send`/`Sync`
//!   and no `unsafe impl` is needed for the wrapper. Callers talk to the thread
//!   over a channel; a canceled caller future simply drops its reply receiver
//!   while the thread finishes the in-flight call and keeps the handle open.
//! * `Drop` sends a close command and joins the thread, so the handle is closed
//!   exactly once, after the last request, before the library is unloaded.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use libloading::Library;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

use crate::CodeIntelligence;
use crate::error::{IntelError, Result};
use crate::ffi::{self, Symbols, ZindeksBuffer, ZindeksHandle};
use crate::mapping;
use crate::types::{CodeContext, CodeContextRequest, CodeSearchResult, FileOutline, IntelHealth};

/// Stack size for the embedded engine thread. The Zig indexer/parser recurses
/// deeply; the default thread stack is far too small and the engine traps.
const ENGINE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// A command sent to the engine thread.
enum Command {
    Request {
        json: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    Close,
}

/// An in-process zindeks engine bound to one repository.
pub struct EmbeddedZindeks {
    tx: UnboundedSender<Command>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    root: PathBuf,
    store_root: PathBuf,
    watch: bool,
}

impl EmbeddedZindeks {
    /// Loads `library` and opens a handle bound to `root`, storing its index
    /// under `store_root`. Performs the ABI version check before opening and
    /// the MCP `initialize` handshake after; both run on the engine thread.
    pub fn open(library: &Path, root: &Path, store_root: &Path, watch: bool) -> Result<Self> {
        let root = std::fs::canonicalize(root).map_err(|e| {
            IntelError::Unavailable(format!("cannot resolve repository {}: {e}", root.display()))
        })?;
        let root = strip_verbatim(root);

        std::fs::create_dir_all(store_root).map_err(|e| {
            IntelError::Unavailable(format!(
                "cannot create zindeks store {}: {e}",
                store_root.display()
            ))
        })?;
        let store_root = std::fs::canonicalize(store_root).map_err(|e| {
            IntelError::Unavailable(format!(
                "cannot resolve zindeks store {}: {e}",
                store_root.display()
            ))
        })?;
        let store_root = strip_verbatim(store_root);

        let options = json!({
            "repository": root.to_string_lossy(),
            "store_root": store_root.to_string_lossy(),
            "watch": watch,
        })
        .to_string();

        let (tx, rx) = unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let library = library.to_path_buf();

        let join = std::thread::Builder::new()
            .name("kode-zindeks".to_string())
            .stack_size(ENGINE_STACK_BYTES)
            .spawn(move || engine_thread(library, options, ready_tx, rx))
            .map_err(|e| IntelError::Unavailable(format!("cannot spawn zindeks engine: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                join: Mutex::new(Some(join)),
                root,
                store_root,
                watch,
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                Err(err)
            }
            Err(_) => {
                let _ = join.join();
                Err(IntelError::Unavailable(
                    "zindeks engine thread exited during open".to_string(),
                ))
            }
        }
    }

    /// Absolute, canonical repository this handle is bound to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store root the index lives under.
    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    /// Whether this handle runs the background incremental watcher.
    pub fn watching(&self) -> bool {
        self.watch
    }

    /// Whether `root` is already indexed by this engine (`list_projects`).
    /// A not-indexed engine response is treated as `false`, not an error.
    pub async fn is_indexed(&self) -> Result<bool> {
        let text = match self.tool_call_text("list_projects", json!({})).await {
            Ok(t) => t,
            Err(IntelError::NotIndexed(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let value = mapping::parse_tool_json(&text)?;
        let projects = value.as_array().cloned().unwrap_or_default();
        let target = mapping::normalize_path(&self.root.to_string_lossy());
        Ok(projects.iter().any(|p| {
            p.get("root")
                .and_then(|r| r.as_str())
                .map(mapping::normalize_path)
                .as_deref()
                == Some(target.as_str())
        }))
    }

    /// Binds `root` for this session, but only if it is already indexed. Never
    /// triggers a first-time index — that is an explicit `kode index`.
    pub async fn ensure_bound(&self) -> Result<()> {
        if self.is_indexed().await? {
            self.tool_call_text("index_repository", json!({"path": tool_path(&self.root)}))
                .await?;
            Ok(())
        } else {
            Err(IntelError::NotIndexed(self.root.display().to_string()))
        }
    }

    /// Explicitly (re)indexes the bound repository. This is the only place a
    /// first-time index can happen, and only `kode index` calls it.
    pub async fn index_repository(&self) -> Result<()> {
        self.tool_call_text("index_repository", json!({"path": tool_path(&self.root)}))
            .await
            .map(|_| ())
    }

    /// Sends a raw JSON-RPC message to the engine thread and awaits the reply.
    /// A notification yields `None`.
    async fn raw_request(&self, request: &str) -> Result<Option<String>> {
        if request.len() > ffi::MAX_REQUEST_BYTES {
            return Err(IntelError::Protocol(format!(
                "zindeks request exceeds the {} byte limit",
                ffi::MAX_REQUEST_BYTES
            )));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Request {
                json: request.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| IntelError::Unavailable("zindeks engine thread is gone".to_string()))?;
        reply_rx
            .await
            .map_err(|_| IntelError::Unavailable("zindeks engine dropped the reply".to_string()))?
    }

    /// Runs one `tools/call` on the engine thread and returns its text.
    async fn tool_call_text(&self, name: &str, arguments: Value) -> Result<String> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        })
        .to_string();

        let body = self
            .raw_request(&request)
            .await?
            .ok_or_else(|| IntelError::Protocol("zindeks returned no response".to_string()))?;
        let value: Value = serde_json::from_str(&body)
            .map_err(|e| IntelError::Protocol(format!("invalid jsonrpc response: {e}")))?;
        mapping::extract_tool_text(&value, &self.root)
    }
}

impl Drop for EmbeddedZindeks {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Close);
        if let Ok(mut guard) = self.join.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

#[async_trait::async_trait]
impl CodeIntelligence for EmbeddedZindeks {
    async fn health(&self) -> Result<IntelHealth> {
        let text = self.tool_call_text("health_check", json!({})).await?;
        Ok(mapping::health_from_value(&mapping::parse_tool_json(
            &text,
        )?))
    }

    async fn get_context(&self, request: CodeContextRequest) -> Result<CodeContext> {
        let mut args = serde_json::Map::new();
        args.insert("query".to_string(), json!(request.query));
        if !request.working_set.is_empty() {
            args.insert("working_set".to_string(), json!(request.working_set));
        }
        if let Some(max_tokens) = request.max_tokens {
            args.insert("max_tokens".to_string(), json!(max_tokens));
        }
        let text = self
            .tool_call_text("get_context", Value::Object(args))
            .await?;
        Ok(mapping::context_from_value(&mapping::parse_tool_json(
            &text,
        )?))
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<CodeSearchResult>> {
        let text = self
            .tool_call_text("search", json!({"query": query, "limit": limit}))
            .await?;
        Ok(mapping::search_from_value(&mapping::parse_tool_json(
            &text,
        )?))
    }

    async fn file_outline(&self, path: &str) -> Result<FileOutline> {
        let text = self
            .tool_call_text("file_outline", json!({"path": path}))
            .await?;
        Ok(mapping::outline_from_value(
            &mapping::parse_tool_json(&text)?,
            path,
        ))
    }

    async fn ensure_bound(&self) -> Result<()> {
        EmbeddedZindeks::ensure_bound(self).await
    }

    async fn index_repository(&self) -> Result<()> {
        EmbeddedZindeks::index_repository(self).await
    }

    fn watching(&self) -> bool {
        self.watching()
    }
}

/// The engine thread body: load + open + initialize, report readiness, then
/// serve requests until a `Close` (or the caller drops the sender), then drop
/// the engine — which closes the handle — on this same thread.
fn engine_thread(
    library: PathBuf,
    options: String,
    ready: std::sync::mpsc::Sender<Result<()>>,
    mut rx: UnboundedReceiver<Command>,
) {
    let engine = match Engine::start(&library, &options) {
        Ok(engine) => engine,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }

    while let Some(command) = rx.blocking_recv() {
        match command {
            Command::Request { json, reply } => {
                let _ = reply.send(engine.raw_request(&json));
            }
            Command::Close => break,
        }
    }
    // `engine` drops here, closing the handle before the library unloads.
}

/// Engine state that lives only on the engine thread.
struct Engine {
    symbols: Symbols,
    handle: *mut ZindeksHandle,
    /// Kept alive until after the handle is closed; declared last so it drops
    /// last (after `Drop for Engine`).
    _library: Library,
}

impl Engine {
    fn start(library: &Path, options: &str) -> Result<Self> {
        let lib = unsafe { Library::new(library) }.map_err(|e| {
            IntelError::Unavailable(format!(
                "cannot load zindeks library {}: {e}",
                library.display()
            ))
        })?;
        let symbols = unsafe { Symbols::load(&lib) }.map_err(|e| {
            IntelError::Protocol(format!(
                "zindeks library {} is missing an ABI 1 symbol: {e}",
                library.display()
            ))
        })?;
        check_abi(unsafe { (symbols.abi_version)() })?;

        let mut out: *mut ZindeksHandle = std::ptr::null_mut();
        let mut error = ZindeksBuffer::empty();
        // SAFETY: options is valid UTF-8 for its length; both out pointers are
        // live for the call; this runs on the dedicated engine thread.
        let rc = unsafe { (symbols.open)(options.as_ptr(), options.len(), &mut out, &mut error) };

        if rc != ffi::STATUS_OK {
            let message = read_and_free(&symbols, &mut error)
                .unwrap_or_else(|| format!("zindeks_open failed with status {rc}"));
            return Err(map_open_status(rc, message));
        }
        unsafe { (symbols.buffer_free)(&mut error) };
        if out.is_null() {
            return Err(IntelError::Protocol(
                "zindeks_open reported success but returned no handle".to_string(),
            ));
        }

        let engine = Self {
            symbols,
            handle: out,
            _library: lib,
        };

        // Complete the MCP handshake so tool calls behave exactly as over stdio.
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "clientInfo": {"name": "kode"}}
        })
        .to_string();
        engine.raw_request(&init)?;

        Ok(engine)
    }

    fn raw_request(&self, request: &str) -> Result<Option<String>> {
        if request.len() > ffi::MAX_REQUEST_BYTES {
            return Err(IntelError::Protocol(format!(
                "zindeks request exceeds the {} byte limit",
                ffi::MAX_REQUEST_BYTES
            )));
        }
        if self.handle.is_null() {
            return Err(IntelError::Unavailable(
                "zindeks handle is closed".to_string(),
            ));
        }

        let mut response = ZindeksBuffer::empty();
        // SAFETY: handle is live (checked), request is valid UTF-8 for its
        // length, and this thread owns the handle for its whole lifetime.
        let rc = unsafe {
            (self.symbols.request)(self.handle, request.as_ptr(), request.len(), &mut response)
        };
        let owned = OwnedBuffer {
            symbols: &self.symbols,
            buf: response,
        };

        match rc {
            ffi::STATUS_OK => {
                if owned.buf.is_empty() {
                    return Ok(None);
                }
                if owned.buf.len > ffi::MAX_RESPONSE_BYTES {
                    return Err(IntelError::Protocol(
                        "zindeks response exceeds the 16 MiB limit".to_string(),
                    ));
                }
                // SAFETY: ptr/len describe a readable buffer until buffer_free
                // (on drop below).
                let bytes = unsafe { std::slice::from_raw_parts(owned.buf.ptr, owned.buf.len) };
                let text = std::str::from_utf8(bytes)
                    .map_err(|e| {
                        IntelError::Protocol(format!("zindeks response is not UTF-8: {e}"))
                    })?
                    .to_string();
                Ok(Some(text))
            }
            other => {
                let message = if owned.buf.is_empty() {
                    None
                } else {
                    // SAFETY: as above; still valid until buffer_free.
                    let bytes = unsafe { std::slice::from_raw_parts(owned.buf.ptr, owned.buf.len) };
                    Some(String::from_utf8_lossy(bytes).into_owned())
                };
                Err(map_request_status(
                    other,
                    message
                        .unwrap_or_else(|| format!("zindeks_request failed with status {other}")),
                ))
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: this runs on the engine thread; no request is in flight;
            // the pointer came from `open` and is closed exactly once.
            unsafe { (self.symbols.close)(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

/// Frees a library-owned buffer on drop, covering error and panic paths.
struct OwnedBuffer<'a> {
    symbols: &'a Symbols,
    buf: ZindeksBuffer,
}

impl Drop for OwnedBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: `buf` was produced by this library's open/request.
        unsafe { (self.symbols.buffer_free)(&mut self.buf) };
    }
}

/// Formats a path for zindeks tool arguments. Forward slashes are accepted on
/// every platform and avoid any backslash round-tripping in the engine's path
/// handling.
fn tool_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Removes the Windows `\\?\` verbatim prefix that `std::fs::canonicalize`
/// adds but the zindeks engine's `realpath` cannot parse (it rejects such
/// paths with `BadPathName`). A no-op on other platforms.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().into_owned();
        if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
            return PathBuf::from(format!("\\\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix("\\\\?\\") {
            return PathBuf::from(rest.to_string());
        }
    }
    path
}

/// Verifies the loaded library reports the ABI this crate was written against.
fn check_abi(reported: u32) -> Result<()> {
    if reported != ffi::ABI_VERSION {
        return Err(IntelError::Protocol(format!(
            "zindeks library reports ABI {reported}, Kode requires ABI {}",
            ffi::ABI_VERSION
        )));
    }
    Ok(())
}

/// Copies a library-owned error body to an owned `String` and frees it.
fn read_and_free(symbols: &Symbols, buffer: &mut ZindeksBuffer) -> Option<String> {
    let text = if buffer.is_empty() {
        None
    } else {
        // SAFETY: ptr/len describe a readable buffer until buffer_free.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) };
        Some(String::from_utf8_lossy(bytes).into_owned())
    };
    unsafe { (symbols.buffer_free)(buffer) };
    text
}

fn map_open_status(status: i32, message: String) -> IntelError {
    match status {
        ffi::STATUS_INVALID_INPUT => IntelError::Protocol(message),
        ffi::STATUS_ALLOC_ERROR => IntelError::Unavailable(message),
        ffi::STATUS_INIT_ERROR | ffi::STATUS_BUSY => IntelError::Unavailable(message),
        _ => IntelError::Protocol(message),
    }
}

fn map_request_status(status: i32, message: String) -> IntelError {
    match status {
        ffi::STATUS_INVALID_INPUT => IntelError::Protocol(message),
        ffi::STATUS_ALLOC_ERROR => IntelError::Unavailable(message),
        ffi::STATUS_INIT_ERROR | ffi::STATUS_BUSY => IntelError::Tool(message),
        _ => IntelError::Protocol(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_mismatch_is_refused() {
        assert!(check_abi(1).is_ok());
        let err = check_abi(2).unwrap_err();
        assert!(matches!(err, IntelError::Protocol(_)));
    }

    #[test]
    fn strip_verbatim_removes_windows_prefix() {
        #[cfg(windows)]
        {
            assert_eq!(
                strip_verbatim(PathBuf::from(r"\\?\C:\repo")),
                PathBuf::from(r"C:\repo")
            );
            assert_eq!(
                strip_verbatim(PathBuf::from(r"\\?\UNC\server\share")),
                PathBuf::from(r"\\server\share")
            );
        }
        let plain = PathBuf::from("/repo");
        assert_eq!(strip_verbatim(plain.clone()), plain);
    }
}
