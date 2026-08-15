//! Talks to the ChatGPT Codex backend's Responses API using Kode's own
//! credential store (`~/.kode/auth/codex.json`, written by `kode auth login
//! codex`), with automatic token refresh.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::error::{ModelError, Result};
use crate::sse::extract_data_lines;
use crate::types::{FinishReason, ModelCapabilities, ModelRequest, StreamEvent, Usage};
use crate::{Model, ModelStream};

const CODEX_BACKEND_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const CODEX_REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const STALE_SECS: u64 = 50 * 60;

/// Parsed contents of `~/.codex/auth.json`.
#[derive(Clone)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub last_refresh: String,
    pub api_key: Option<String>,
    pub auth_mode: String,
}

// Manual Debug: never expose tokens/keys via logging/Debug-printing.
impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("access_token", &"***redacted***")
            .field("refresh_token", &"***redacted***")
            .field("account_id", &"***redacted***")
            .field("last_refresh", &self.last_refresh)
            .field("api_key", &self.api_key.as_ref().map(|_| "***redacted***"))
            .field("auth_mode", &self.auth_mode)
            .finish()
    }
}

/// Default location of Kode's own codex auth store: `~/.kode/auth/codex.json`.
pub fn default_auth_path() -> Option<PathBuf> {
    Some(kode_core::auth_dir()?.join("codex.json"))
}

/// Loads and parses `auth.json`. `auth_mode == "chatgpt"` (or anything other
/// than `"apikey"`) requires `tokens.access_token` and `tokens.refresh_token`
/// to be present; `"apikey"` mode does not.
pub fn load(path: &Path) -> Result<CodexAuth> {
    let content = std::fs::read_to_string(path).map_err(|e| ModelError::Api {
        status: 0,
        message: format!(
            "cannot read codex auth file {}: {e} — run: kode auth login codex",
            path.display()
        ),
    })?;
    let value: Value = serde_json::from_str(&content).map_err(|e| ModelError::Api {
        status: 0,
        message: format!(
            "invalid codex auth.json ({}): {e} — run: kode auth login codex",
            path.display()
        ),
    })?;

    let auth_mode = value
        .get("auth_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("chatgpt")
        .to_string();
    let api_key = value
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let tokens = value.get("tokens");
    let access_token = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let refresh_token = tokens
        .and_then(|t| t.get("refresh_token"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let account_id = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let last_refresh = value
        .get("last_refresh")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if auth_mode != "apikey" && (access_token.is_empty() || refresh_token.is_empty()) {
        return Err(ModelError::Api {
            status: 0,
            message: "codex auth.json missing chatgpt tokens — run: kode auth login codex"
                .to_string(),
        });
    }

    Ok(CodexAuth {
        access_token,
        refresh_token,
        account_id,
        last_refresh,
        api_key,
        auth_mode,
    })
}

/// Value-preserving rewrite of `auth.json`'s token fields after a refresh:
/// parses as a generic `Value`, mutates only `tokens.{id_token,access_token,
/// refresh_token}` and top-level `last_refresh`, and writes the rest back
/// untouched.
fn save_tokens(
    path: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
    last_refresh: &str,
) -> Result<()> {
    let content = std::fs::read_to_string(path).map_err(|e| ModelError::Api {
        status: 0,
        message: format!("cannot read codex auth file for update: {e}"),
    })?;
    let mut value: Value = serde_json::from_str(&content).map_err(|e| ModelError::Api {
        status: 0,
        message: format!("invalid codex auth.json for update: {e}"),
    })?;

    let mut token_fields = serde_json::Map::new();
    token_fields.insert("id_token".to_string(), Value::String(id_token.to_string()));
    token_fields.insert(
        "access_token".to_string(),
        Value::String(access_token.to_string()),
    );
    token_fields.insert(
        "refresh_token".to_string(),
        Value::String(refresh_token.to_string()),
    );

    if let Some(existing) = value.get_mut("tokens").and_then(|t| t.as_object_mut()) {
        for (k, v) in token_fields {
            existing.insert(k, v);
        }
    } else if let Some(obj) = value.as_object_mut() {
        obj.insert("tokens".to_string(), Value::Object(token_fields));
    }

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "last_refresh".to_string(),
            Value::String(last_refresh.to_string()),
        );
    }

    let pretty =
        serde_json::to_string_pretty(&value).map_err(|e| ModelError::Parse(e.to_string()))?;
    std::fs::write(path, pretty).map_err(|e| ModelError::Api {
        status: 0,
        message: format!("cannot write codex auth file: {e}"),
    })?;
    Ok(())
}

#[derive(Debug)]
pub struct CodexModel {
    auth_path: PathBuf,
    model: String,
    client: reqwest::Client,
    state: tokio::sync::Mutex<CodexAuth>,
}

impl CodexModel {
    /// Loads auth from `auth_path`. Rejects `auth_mode == "apikey"` with a
    /// present API key — that combination should use plain `OpenAiModel`
    /// against `api.openai.com` instead (the caller/pipeline decides this).
    pub fn new(auth_path: PathBuf, model: String) -> Result<Self> {
        let auth = load(&auth_path)?;
        if auth.auth_mode == "apikey" && auth.api_key.is_some() {
            return Err(ModelError::Api {
                status: 0,
                message: "codex auth_mode=apikey — use provider=\"openai\" with that key instead of provider=\"codex\"".to_string(),
            });
        }
        Ok(Self {
            auth_path,
            model,
            client: reqwest::Client::new(),
            state: tokio::sync::Mutex::new(auth),
        })
    }

    /// Refreshes tokens via the OAuth refresh endpoint and persists the
    /// result to `auth_path`, updating `auth` in place. Missing
    /// `refresh_token` in the response keeps the old one, per OAuth refresh
    /// convention.
    async fn refresh(&self, auth: &mut CodexAuth) -> Result<()> {
        let body = serde_json::json!({
            "client_id": CODEX_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": auth.refresh_token,
            "scope": "openid profile email",
        });
        let resp = self
            .client
            .post(CODEX_REFRESH_URL)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let message: String = text.chars().take(2000).collect();
            return Err(ModelError::Api { status, message });
        }

        let wire: RefreshResponse = resp.json().await.map_err(ModelError::Http)?;
        let last_refresh = format_rfc3339_secs(now_secs());
        let new_refresh_token = wire
            .refresh_token
            .unwrap_or_else(|| auth.refresh_token.clone());

        save_tokens(
            &self.auth_path,
            &wire.id_token,
            &wire.access_token,
            &new_refresh_token,
            &last_refresh,
        )?;

        auth.access_token = wire.access_token;
        auth.refresh_token = new_refresh_token;
        auth.last_refresh = last_refresh;
        Ok(())
    }

    async fn send_request(
        &self,
        access_token: &str,
        account_id: &str,
        session_id: &str,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let resp = self
            .client
            .post(CODEX_BACKEND_URL)
            .bearer_auth(access_token)
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "codex_cli_rs")
            .header("Accept", "text/event-stream")
            .header("session_id", session_id)
            .json(body)
            .send()
            .await?;
        Ok(resp)
    }
}

#[derive(serde::Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    id_token: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[async_trait::async_trait]
impl Model for CodexModel {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        // Refresh-if-stale under the mutex. We hold the guard across the
        // refresh `.await` (serialized by design: only one refresh/request
        // preparation happens at a time for a given CodexModel), but we drop
        // it before sending/streaming the actual request so concurrent calls
        // don't block on each other's network I/O.
        let (mut access_token, account_id) = {
            let mut guard = self.state.lock().await;
            if needs_refresh(&guard.last_refresh, now_secs()) {
                self.refresh(&mut guard).await?;
            }
            (guard.access_token.clone(), guard.account_id.clone())
        };

        let body = build_body(&self.model, &request);
        let session_id = make_session_id();

        let mut resp = self
            .send_request(&access_token, &account_id, &session_id, &body)
            .await?;

        if resp.status().as_u16() == 401 {
            let mut guard = self.state.lock().await;
            self.refresh(&mut guard).await?;
            access_token = guard.access_token.clone();
            drop(guard);
            resp = self
                .send_request(&access_token, &account_id, &session_id, &body)
                .await?;
        }

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let message: String = text.chars().take(2000).collect();
            return Err(ModelError::Api { status, message });
        }

        let bytes_stream = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
        let state = CodexStreamState {
            bytes: Box::pin(bytes_stream),
            buffer: String::new(),
            pending: VecDeque::new(),
            sse_state: CodexSseState::default(),
            done: false,
        };

        let stream = futures::stream::try_unfold(state, codex_sse_step);
        Ok(Box::pin(stream))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            id: self.model.clone(),
            supports_tools: true,
            supports_streaming: true,
        }
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = std::result::Result<Vec<u8>, reqwest::Error>> + Send>>;

struct CodexStreamState {
    bytes: ByteStream,
    buffer: String,
    pending: VecDeque<StreamEvent>,
    sse_state: CodexSseState,
    done: bool,
}

async fn codex_sse_step(
    mut state: CodexStreamState,
) -> std::result::Result<Option<(StreamEvent, CodexStreamState)>, ModelError> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some((event, state)));
        }
        if state.done {
            if state.sse_state.finished {
                return Ok(None);
            }
            return Err(ModelError::Parse(
                "codex stream ended before response.completed".to_string(),
            ));
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => {
                let text = String::from_utf8_lossy(&chunk).into_owned();
                let lines = extract_data_lines(&mut state.buffer, &text);
                for payload in lines {
                    if payload == "[DONE]" {
                        state.done = true;
                        break;
                    }
                    let value: Value = serde_json::from_str(&payload).map_err(|e| {
                        ModelError::Parse(format!("invalid codex SSE payload JSON: {e}"))
                    })?;
                    let events = map_sse_json(&value, &mut state.sse_state)?;
                    state.pending.extend(events);
                }
            }
            Some(Err(e)) => return Err(ModelError::Http(e)),
            None => state.done = true,
        }
    }
}

#[derive(Default)]
struct CodexSseState {
    fn_call_index: u32,
    saw_fn: bool,
    finished: bool,
}

/// Maps one decoded Codex Responses-API SSE event to zero or more
/// [`StreamEvent`]s. Unrecognized `type`s are ignored. `response.failed`
/// surfaces as an error rather than an event.
fn map_sse_json(v: &Value, state: &mut CodexSseState) -> Result<Vec<StreamEvent>> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "response.output_text.delta" => {
            let delta = v
                .get("delta")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            Ok(vec![StreamEvent::TextDelta(delta)])
        }
        "response.output_item.done" => {
            let Some(item) = v.get("item") else {
                return Ok(vec![]);
            };
            if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                return Ok(vec![]);
            }
            let call_id = item
                .get("call_id")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string();
            let name = item
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or_default()
                .to_string();
            let index = state.fn_call_index;
            state.fn_call_index += 1;
            state.saw_fn = true;
            Ok(vec![StreamEvent::ToolCallDelta {
                index,
                id: Some(call_id),
                name: Some(name),
                arguments_delta: arguments,
            }])
        }
        "response.completed" => {
            state.finished = true;
            let usage = v
                .get("response")
                .and_then(|r| r.get("usage"))
                .map(|u| Usage {
                    input_tokens: u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                    output_tokens: u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                });
            let reason = if state.saw_fn {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            };
            Ok(vec![StreamEvent::Finished { reason, usage }])
        }
        "response.failed" => {
            let message = v
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("codex response failed")
                .to_string();
            Err(ModelError::Api { status: 0, message })
        }
        _ => Ok(vec![]),
    }
}

/// Builds the Codex Responses-API request body. All `System` messages are
/// concatenated (joined by `"\n\n"`) into `instructions`; everything else
/// maps into `input` items. `tools` is omitted entirely when empty.
fn build_body(model: &str, request: &ModelRequest) -> Value {
    use crate::types::Message;

    let mut system_parts: Vec<&str> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for message in &request.messages {
        match message {
            Message::System(content) => system_parts.push(content.as_str()),
            Message::User(content) => {
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": content}],
                }));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if !content.is_empty() {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}],
                    }));
                }
                for tc in tool_calls {
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.name,
                        "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default(),
                    }));
                }
            }
            Message::Tool {
                tool_call_id,
                content,
            } => {
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": content,
                }));
            }
        }
    }

    let instructions = system_parts.join("\n\n");

    let mut body = serde_json::json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "store": false,
        "stream": true,
        "include": [],
    });

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": false,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }

    if let Some(effort) = &request.effort {
        body["reasoning"] = serde_json::json!({"effort": effort});
    }

    body
}

/// Whether a token last refreshed at `last_refresh` (loosely-parsed RFC3339,
/// seconds precision) is older than 50 minutes relative to `now_secs`.
/// Unparseable timestamps are treated as stale (fail safe: refresh).
fn needs_refresh(last_refresh: &str, now_secs: u64) -> bool {
    match parse_rfc3339_secs(last_refresh) {
        Some(last) => now_secs.saturating_sub(last) > STALE_SECS,
        None => true,
    }
}

/// Parses the `YYYY-MM-DDTHH:MM:SS` prefix of an RFC3339 string (ignoring
/// any sub-second fraction and the trailing `Z`/offset) into Unix seconds.
fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    let hh: u64 = s.get(11..13)?.parse().ok()?;
    let mm: u64 = s.get(14..16)?.parse().ok()?;
    let ss: u64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Formats Unix seconds as an RFC3339 UTC timestamp at seconds precision,
/// e.g. `2026-08-15T12:00:00Z`. Exposed (not just `pub(crate)`) for reuse by
/// `kode auth login codex`, which writes this same `last_refresh` format
/// into the freshly-created auth store entry.
pub fn format_rfc3339_secs(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date. Howard
/// Hinnant's well-known `days_from_civil` algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: civil `(year, month, day)` from days
/// since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Generates a uuid-format (8-4-4-4-12) session id without a `uuid`
/// dependency, mixing current-time nanoseconds with the process id.
fn make_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let low = nanos as u64;
    let high = (nanos >> 64) as u64;

    let a = low.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ pid;
    let b = high.wrapping_add(pid).wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ low.rotate_left(17);

    let combined = ((a as u128) << 64) | (b as u128);
    let hex = format!("{combined:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolCall, ToolSpec};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_file(name_hint: &str, contents: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-codex-test-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name_hint);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn build_body_full_mapping() {
        let request = ModelRequest {
            messages: vec![
                Message::System("sys one".to_string()),
                Message::System("sys two".to_string()),
                Message::User("hi".to_string()),
                Message::Assistant {
                    content: "assist text".to_string(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "foo".to_string(),
                        arguments: serde_json::json!({"a": 1}),
                    }],
                },
                Message::Tool {
                    tool_call_id: "call_1".to_string(),
                    content: "tool result".to_string(),
                },
            ],
            tools: vec![ToolSpec {
                name: "foo".to_string(),
                description: "desc".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_tokens: None,
            temperature: None,
            effort: None,
        };

        let body = build_body("gpt-5-codex", &request);

        let expected = serde_json::json!({
            "model": "gpt-5-codex",
            "instructions": "sys one\n\nsys two",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "assist text"}]},
                {"type": "function_call", "call_id": "call_1", "name": "foo", "arguments": "{\"a\":1}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "tool result"}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "store": false,
            "stream": true,
            "include": [],
            "tools": [
                {"type": "function", "name": "foo", "description": "desc", "parameters": {"type": "object"}, "strict": false}
            ]
        });

        assert_eq!(body, expected);
    }

    #[test]
    fn build_body_omits_tools_when_empty_and_assistant_text_when_blank() {
        let request = ModelRequest {
            messages: vec![
                Message::User("hi".to_string()),
                Message::Assistant {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "foo".to_string(),
                        arguments: serde_json::json!({}),
                    }],
                },
            ],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: None,
        };

        let body = build_body("gpt-5-codex", &request);
        assert!(body.get("tools").is_none());
        let input = body["input"].as_array().unwrap();
        // No output_text item for the blank assistant content, only the
        // function_call.
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], "function_call");
    }

    #[test]
    fn build_body_includes_reasoning_effort_when_set() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: Some("high".to_string()),
        };

        let body = build_body("gpt-5-codex", &request);
        assert_eq!(body["reasoning"], serde_json::json!({"effort": "high"}));
    }

    #[test]
    fn build_body_omits_reasoning_when_effort_absent() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: None,
        };

        let body = build_body("gpt-5-codex", &request);
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn map_sse_json_text_delta() {
        let mut state = CodexSseState::default();
        let v: Value =
            serde_json::from_str(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
                .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(events, vec![StreamEvent::TextDelta("hello".to_string())]);
    }

    #[test]
    fn map_sse_json_function_call_done() {
        let mut state = CodexSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"foo","arguments":"{\"a\":1}"}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("foo".to_string()),
                arguments_delta: "{\"a\":1}".to_string(),
            }]
        );
        assert!(state.saw_fn);
        assert_eq!(state.fn_call_index, 1);
    }

    #[test]
    fn map_sse_json_output_item_done_ignores_non_function_call() {
        let mut state = CodexSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"response.output_item.done","item":{"type":"message"}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert!(events.is_empty());
        assert!(!state.saw_fn);
    }

    #[test]
    fn map_sse_json_completed_without_fn_call_is_stop() {
        let mut state = CodexSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":7}}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 5,
                    output_tokens: 7
                }),
            }]
        );
        assert!(state.finished);
    }

    #[test]
    fn map_sse_json_completed_with_fn_call_is_tool_calls() {
        let mut state = CodexSseState {
            fn_call_index: 1,
            saw_fn: true,
            finished: false,
        };
        let v: Value =
            serde_json::from_str(r#"{"type":"response.completed","response":{}}"#).unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            }]
        );
    }

    #[test]
    fn map_sse_json_failed_is_error() {
        let mut state = CodexSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"response.failed","response":{"error":{"message":"boom"}}}"#,
        )
        .unwrap();
        let err = map_sse_json(&v, &mut state).unwrap_err();
        match err {
            ModelError::Api { status, message } => {
                assert_eq!(status, 0);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn map_sse_json_unknown_type_is_ignored() {
        let mut state = CodexSseState::default();
        let v: Value = serde_json::from_str(r#"{"type":"response.in_progress"}"#).unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn needs_refresh_boundary() {
        let now = 10_000u64;
        let fresh = format_rfc3339_secs(now - STALE_SECS); // exactly at boundary: not stale
        assert!(!needs_refresh(&fresh, now));
        let stale = format_rfc3339_secs(now - STALE_SECS - 1); // one second past boundary
        assert!(needs_refresh(&stale, now));
    }

    #[test]
    fn needs_refresh_unparseable_is_stale() {
        assert!(needs_refresh("not-a-timestamp", 100));
        assert!(needs_refresh("", 100));
    }

    #[test]
    fn format_rfc3339_secs_epoch() {
        assert_eq!(format_rfc3339_secs(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_rfc3339_secs_known_timestamp() {
        // 2025-08-15T00:00:00Z, verified against `date -u -d @1755216000`.
        assert_eq!(format_rfc3339_secs(1_755_216_000), "2025-08-15T00:00:00Z");
    }

    #[test]
    fn parse_rfc3339_secs_round_trips_with_format() {
        let s = format_rfc3339_secs(1_755_216_000);
        assert_eq!(parse_rfc3339_secs(&s), Some(1_755_216_000));
    }

    #[test]
    fn parse_rfc3339_secs_handles_nanos_suffix() {
        // Only the seconds-precision prefix is parsed; extra precision and
        // the trailing 'Z' are ignored.
        let parsed = parse_rfc3339_secs("2026-08-12T07:34:33.025503800Z");
        assert_eq!(
            parsed,
            Some(days_from_civil(2026, 8, 12) as u64 * 86400 + 7 * 3600 + 34 * 60 + 33)
        );
    }

    #[test]
    fn make_session_id_shape() {
        let id = make_session_id();
        assert_eq!(id.len(), 36);
        let bytes = id.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }

    #[test]
    fn make_session_id_varies_across_calls() {
        let a = make_session_id();
        let b = make_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn codex_auth_load_and_save_roundtrip_preserves_unknown_fields() {
        let path = temp_file(
            "auth.json",
            r#"{
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "old_id",
                    "access_token": "old_access",
                    "refresh_token": "old_refresh",
                    "account_id": "acct-123"
                },
                "last_refresh": "2026-08-12T07:34:33.025503800Z",
                "some_future_field": {"nested": true}
            }"#,
        );

        let auth = load(&path).unwrap();
        assert_eq!(auth.auth_mode, "chatgpt");
        assert_eq!(auth.access_token, "old_access");
        assert_eq!(auth.refresh_token, "old_refresh");
        assert_eq!(auth.account_id, "acct-123");
        assert!(auth.api_key.is_none());

        save_tokens(
            &path,
            "new_id",
            "new_access",
            "new_refresh",
            "2026-08-15T00:00:00Z",
        )
        .unwrap();

        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.access_token, "new_access");
        assert_eq!(reloaded.refresh_token, "new_refresh");
        assert_eq!(reloaded.account_id, "acct-123"); // untouched
        assert_eq!(reloaded.last_refresh, "2026-08-15T00:00:00Z");

        // Unknown/future fields must survive the rewrite.
        let raw = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            value["some_future_field"]["nested"],
            serde_json::json!(true)
        );
        assert_eq!(value["tokens"]["id_token"], serde_json::json!("new_id"));
    }

    #[test]
    fn codex_auth_load_missing_file_errors_actionable() {
        let dir = std::env::temp_dir().join(format!(
            "kode-codex-test-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("auth.json");
        let err = load(&path).unwrap_err();
        match err {
            ModelError::Api { status, message } => {
                assert_eq!(status, 0);
                assert!(message.contains("kode auth login codex"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn codex_auth_load_chatgpt_mode_missing_tokens_errors() {
        let path = temp_file("auth.json", r#"{"auth_mode": "chatgpt", "tokens": {}}"#);
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ModelError::Api { status: 0, .. }));
    }

    #[test]
    fn codex_auth_load_apikey_mode_without_tokens_ok() {
        let path = temp_file(
            "auth.json",
            r#"{"auth_mode": "apikey", "OPENAI_API_KEY": "sk-example"}"#,
        );
        let auth = load(&path).unwrap();
        assert_eq!(auth.auth_mode, "apikey");
        assert_eq!(auth.api_key.as_deref(), Some("sk-example"));
        assert!(auth.access_token.is_empty());
    }

    #[test]
    fn codex_model_new_rejects_apikey_mode_with_key_present() {
        let path = temp_file(
            "auth.json",
            r#"{"auth_mode": "apikey", "OPENAI_API_KEY": "sk-example"}"#,
        );
        let err = CodexModel::new(path, "gpt-5-codex".to_string()).unwrap_err();
        match err {
            ModelError::Api { message, .. } => {
                assert!(message.contains("provider=\"openai\""));
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
