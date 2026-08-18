//! Talks to the Anthropic Messages API using Kode's own credential store
//! (`~/.kode/auth/anthropic.json`, written by `kode auth login anthropic`),
//! with either an API key or an OAuth (Claude Pro/Max subscription) token,
//! automatically refreshed. Falls back to the `ANTHROPIC_API_KEY`
//! environment variable when no auth file is present.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::error::{ModelError, Result};
use crate::sse::extract_data_lines;
use crate::types::{FinishReason, Message, ModelCapabilities, ModelRequest, StreamEvent, Usage};
use crate::{Model, ModelStream};

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Anthropic requires `max_tokens` on every request; used when the caller's
/// `ModelRequest` doesn't set one.
const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Refresh an OAuth access token this many seconds before it actually
/// expires, to avoid racing a request against expiry.
const REFRESH_BUFFER_SECS: u64 = 60;

/// Parsed contents of `~/.kode/auth/anthropic.json`.
#[derive(Clone)]
pub enum AnthropicAuth {
    ApiKey(String),
    OAuth {
        access_token: String,
        refresh_token: String,
        expires_at: u64,
    },
}

// Manual Debug: never expose keys/tokens via logging/Debug-printing.
impl std::fmt::Debug for AnthropicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnthropicAuth::ApiKey(_) => f.debug_tuple("ApiKey").field(&"***redacted***").finish(),
            AnthropicAuth::OAuth { expires_at, .. } => f
                .debug_struct("OAuth")
                .field("access_token", &"***redacted***")
                .field("refresh_token", &"***redacted***")
                .field("expires_at", expires_at)
                .finish(),
        }
    }
}

/// Default location of Kode's own anthropic auth store:
/// `~/.kode/auth/anthropic.json`.
pub fn default_auth_path() -> Option<PathBuf> {
    Some(kode_core::auth_dir()?.join("anthropic.json"))
}

/// Loads and parses `anthropic.json`. When the file doesn't exist, falls
/// back to `ANTHROPIC_API_KEY` from the environment (per-file schema:
/// `{"type":"api","key":"..."}` or `{"type":"oauth","access_token":"...",
/// "refresh_token":"...","expires_at":<unix_secs>}`).
pub fn load(path: &Path) -> Result<AnthropicAuth> {
    if !path.exists() {
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
            && !key.is_empty()
        {
            return Ok(AnthropicAuth::ApiKey(key));
        }
        return Err(ModelError::Api {
            status: 0,
            message: format!(
                "cannot read anthropic auth file {}: not found — run: kode auth login anthropic (or set ANTHROPIC_API_KEY)",
                path.display()
            ),
        });
    }

    let content = std::fs::read_to_string(path).map_err(|e| ModelError::Api {
        status: 0,
        message: format!(
            "cannot read anthropic auth file {}: {e} — run: kode auth login anthropic",
            path.display()
        ),
    })?;
    let value: Value = serde_json::from_str(&content).map_err(|e| ModelError::Api {
        status: 0,
        message: format!(
            "invalid anthropic auth.json ({}): {e} — run: kode auth login anthropic",
            path.display()
        ),
    })?;

    let ty = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "api" => {
            let key = value
                .get("key")
                .and_then(|k| k.as_str())
                .filter(|k| !k.is_empty())
                .ok_or_else(|| ModelError::Api {
                    status: 0,
                    message: "anthropic auth.json type=api missing 'key' — run: kode auth login anthropic"
                        .to_string(),
                })?;
            Ok(AnthropicAuth::ApiKey(key.to_string()))
        }
        "oauth" => {
            let access_token = value
                .get("access_token")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let refresh_token = value
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let expires_at = value
                .get("expires_at")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if access_token.is_empty() || refresh_token.is_empty() {
                return Err(ModelError::Api {
                    status: 0,
                    message: "anthropic auth.json type=oauth missing tokens — run: kode auth login anthropic"
                        .to_string(),
                });
            }
            Ok(AnthropicAuth::OAuth {
                access_token,
                refresh_token,
                expires_at,
            })
        }
        other => Err(ModelError::Api {
            status: 0,
            message: format!(
                "anthropic auth.json has unknown type '{other}' — run: kode auth login anthropic"
            ),
        }),
    }
}

fn write_oauth(
    path: &Path,
    access_token: &str,
    refresh_token: &str,
    expires_at: u64,
) -> Result<()> {
    let value = serde_json::json!({
        "type": "oauth",
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expires_at": expires_at,
    });
    let pretty =
        serde_json::to_string_pretty(&value).map_err(|e| ModelError::Parse(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ModelError::Api {
            status: 0,
            message: format!("cannot create anthropic auth dir: {e}"),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    write_secret_file(path, &pretty).map_err(|e| ModelError::Api {
        status: 0,
        message: format!("cannot write anthropic auth file: {e}"),
    })?;
    Ok(())
}

// Credentials must never be world-readable; mirrors write_secret_file in the
// kode bin's auth.rs (0o600 on Unix, default ACLs on Windows).
fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(contents.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

/// Refreshes an OAuth access token via `ANTHROPIC_OAUTH_TOKEN_URL` and
/// persists the result to `auth_path`, updating `auth` in place. A no-op for
/// `AnthropicAuth::ApiKey`.
async fn refresh_tokens(
    client: &reqwest::Client,
    auth_path: &Path,
    auth: &mut AnthropicAuth,
) -> Result<()> {
    let AnthropicAuth::OAuth { refresh_token, .. } = auth else {
        return Ok(());
    };

    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
    });
    let resp = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
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
    let expires_at = now_secs() + wire.expires_in;
    let new_refresh_token = wire.refresh_token.unwrap_or_else(|| refresh_token.clone());

    write_oauth(
        auth_path,
        &wire.access_token,
        &new_refresh_token,
        expires_at,
    )?;

    *auth = AnthropicAuth::OAuth {
        access_token: wire.access_token,
        refresh_token: new_refresh_token,
        expires_at,
    };
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether an OAuth token expiring at `expires_at` (unix secs) needs
/// refreshing relative to `now` — true when within [`REFRESH_BUFFER_SECS`]
/// of expiry (or already past it).
fn needs_refresh(expires_at: u64, now: u64) -> bool {
    now + REFRESH_BUFFER_SECS >= expires_at
}

/// Header name/value pairs to attach for a given auth mode. API-key auth
/// sends `x-api-key`; OAuth sends a bearer token plus the
/// `anthropic-beta: oauth-2025-04-20` header (and must NOT send `x-api-key`).
fn auth_headers(auth: &AnthropicAuth) -> Vec<(&'static str, String)> {
    match auth {
        AnthropicAuth::ApiKey(key) => vec![("x-api-key", key.clone())],
        AnthropicAuth::OAuth { access_token, .. } => vec![
            ("authorization", format!("Bearer {access_token}")),
            ("anthropic-beta", "oauth-2025-04-20".to_string()),
        ],
    }
}

#[derive(Debug)]
pub struct AnthropicModel {
    auth_path: PathBuf,
    model: String,
    client: reqwest::Client,
    state: tokio::sync::Mutex<AnthropicAuth>,
}

impl AnthropicModel {
    pub fn new(auth_path: PathBuf, model: String) -> Result<Self> {
        let auth = load(&auth_path)?;
        Ok(Self {
            auth_path,
            model,
            client: reqwest::Client::new(),
            state: tokio::sync::Mutex::new(auth),
        })
    }

    async fn build_request(&self, auth: &AnthropicAuth, body: &Value) -> Result<reqwest::Response> {
        let mut req = self
            .client
            .post(ANTHROPIC_MESSAGES_URL)
            .header("anthropic-version", ANTHROPIC_VERSION);
        for (name, value) in auth_headers(auth) {
            req = req.header(name, value);
        }
        Ok(req.json(body).send().await?)
    }
}

#[async_trait::async_trait]
impl Model for AnthropicModel {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        // Refresh-if-stale under the mutex, same pattern as CodexModel: hold
        // the guard across the refresh `.await` (serialized refresh), but
        // drop it before the actual request/stream I/O so concurrent calls
        // don't block on each other.
        let auth = {
            let mut guard = self.state.lock().await;
            let stale = matches!(
                &*guard,
                AnthropicAuth::OAuth { expires_at, .. } if needs_refresh(*expires_at, now_secs())
            );
            if stale {
                refresh_tokens(&self.client, &self.auth_path, &mut guard).await?;
            }
            guard.clone()
        };

        let body = build_body(&self.model, &request);
        let mut resp = self.build_request(&auth, &body).await?;

        if resp.status().as_u16() == 401 && matches!(auth, AnthropicAuth::OAuth { .. }) {
            let refreshed = {
                let mut guard = self.state.lock().await;
                refresh_tokens(&self.client, &self.auth_path, &mut guard).await?;
                guard.clone()
            };
            resp = self.build_request(&refreshed, &body).await?;
        }

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let message: String = text.chars().take(2000).collect();
            return Err(ModelError::Api { status, message });
        }

        let bytes_stream = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
        let state = AnthropicStreamState {
            bytes: Box::pin(bytes_stream),
            buffer: String::new(),
            pending: VecDeque::new(),
            sse_state: AnthropicSseState::default(),
            done: false,
        };

        let stream = futures::stream::try_unfold(state, anthropic_sse_step);
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

struct AnthropicStreamState {
    bytes: ByteStream,
    buffer: String,
    pending: VecDeque<StreamEvent>,
    sse_state: AnthropicSseState,
    done: bool,
}

async fn anthropic_sse_step(
    mut state: AnthropicStreamState,
) -> std::result::Result<Option<(StreamEvent, AnthropicStreamState)>, ModelError> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some((event, state)));
        }
        if state.done {
            if state.sse_state.finished {
                return Ok(None);
            }
            return Err(ModelError::Parse(
                "anthropic stream ended before message_delta finish".to_string(),
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
                        ModelError::Parse(format!("invalid anthropic SSE payload JSON: {e}"))
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
struct AnthropicSseState {
    input_tokens: u64,
    finished: bool,
}

fn map_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        other => FinishReason::Other(other.to_string()),
    }
}

/// Maps one decoded Anthropic Messages-API SSE event to zero or more
/// [`StreamEvent`]s. Unrecognized `type`s (and `ping`) are ignored. `error`
/// surfaces as an error rather than an event.
fn map_sse_json(v: &Value, state: &mut AnthropicSseState) -> Result<Vec<StreamEvent>> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "message_start" => {
            state.input_tokens = v
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            Ok(vec![])
        }
        "content_block_start" => {
            let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            let Some(block) = v.get("content_block") else {
                return Ok(vec![]);
            };
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let id = block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(vec![StreamEvent::ToolCallDelta {
                    index,
                    id: Some(id),
                    name: Some(name),
                    arguments_delta: String::new(),
                }])
            } else {
                Ok(vec![])
            }
        }
        "content_block_delta" => {
            let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            let Some(delta) = v.get("delta") else {
                return Ok(vec![]);
            };
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    let text = delta
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    Ok(vec![StreamEvent::TextDelta(text)])
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    Ok(vec![StreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: partial,
                    }])
                }
                _ => Ok(vec![]),
            }
        }
        "message_delta" => {
            let Some(stop_reason) = v
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
            else {
                return Ok(vec![]);
            };
            let output_tokens = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let usage = Usage {
                input_tokens: state.input_tokens,
                output_tokens,
            };
            state.finished = true;
            Ok(vec![StreamEvent::Finished {
                reason: map_finish_reason(stop_reason),
                usage: Some(usage),
            }])
        }
        "message_stop" | "content_block_stop" | "ping" => Ok(vec![]),
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("anthropic stream error")
                .to_string();
            Err(ModelError::Api { status: 0, message })
        }
        _ => Ok(vec![]),
    }
}

/// Builds the Anthropic Messages-API request body. `System` messages are
/// concatenated (joined by `"\n\n"`) into the top-level `system` field
/// (omitted entirely when there are none); `Tool` messages map to a `user`
/// message carrying a single `tool_result` content block; assistant tool
/// calls map to `tool_use` content blocks. `max_tokens` defaults to
/// [`DEFAULT_MAX_TOKENS`] when the request doesn't set one — Anthropic
/// requires it on every call.
fn build_body(model: &str, request: &ModelRequest) -> Value {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for message in &request.messages {
        match message {
            Message::System(content) => system_parts.push(content.as_str()),
            Message::User(content) => {
                messages.push(serde_json::json!({"role": "user", "content": content}));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks: Vec<Value> = Vec::new();
                if !content.is_empty() {
                    blocks.push(serde_json::json!({"type": "text", "text": content}));
                }
                for tc in tool_calls {
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                    }));
                }
                messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
            }
            Message::Tool {
                tool_call_id,
                content,
            } => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content,
                    }],
                }));
            }
        }
    }

    let max_tokens = request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });

    if !system_parts.is_empty() {
        body["system"] = serde_json::json!(system_parts.join("\n\n"));
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolSpec};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_file(name_hint: &str, contents: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-anthropic-test-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name_hint);
        std::fs::write(&path, contents).unwrap();
        path
    }

    // --- build_body --------------------------------------------------------

    #[test]
    fn build_body_extracts_system_and_defaults_max_tokens() {
        let request = ModelRequest {
            messages: vec![
                Message::System("sys one".to_string()),
                Message::System("sys two".to_string()),
                Message::User("hi".to_string()),
            ],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: None,
        };

        let body = build_body("claude-sonnet-5", &request);
        assert_eq!(body["system"], serde_json::json!("sys one\n\nsys two"));
        assert_eq!(body["max_tokens"], serde_json::json!(DEFAULT_MAX_TOKENS));
        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "user", "content": "hi"}])
        );
        assert_eq!(body["stream"], serde_json::json!(true));
    }

    #[test]
    fn build_body_omits_system_when_absent() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: None,
        };
        let body = build_body("claude-sonnet-5", &request);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn build_body_respects_explicit_max_tokens() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: Some(256),
            temperature: None,
            effort: None,
        };
        let body = build_body("claude-sonnet-5", &request);
        assert_eq!(body["max_tokens"], serde_json::json!(256));
    }

    #[test]
    fn build_body_maps_tool_use_and_tool_result() {
        let request = ModelRequest {
            messages: vec![
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
            max_tokens: Some(100),
            temperature: Some(0.5),
            effort: None,
        };

        let body = build_body("claude-sonnet-5", &request);

        let expected_messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "assist text"},
                    {"type": "tool_use", "id": "call_1", "name": "foo", "input": {"a": 1}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "tool result"}
                ]
            }
        ]);
        assert_eq!(body["messages"], expected_messages);
        assert_eq!(
            body["tools"],
            serde_json::json!([
                {"name": "foo", "description": "desc", "input_schema": {"type": "object"}}
            ])
        );
        assert_eq!(body["temperature"], serde_json::json!(0.5));
    }

    #[test]
    fn build_body_omits_tools_when_empty() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: None,
        };
        let body = build_body("claude-sonnet-5", &request);
        assert!(body.get("tools").is_none());
    }

    // --- SSE mapping ---------------------------------------------------------

    #[test]
    fn map_sse_json_text_delta() {
        let mut state = AnthropicSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(events, vec![StreamEvent::TextDelta("hello".to_string())]);
    }

    #[test]
    fn map_sse_json_tool_call_start_then_input_delta() {
        let mut state = AnthropicSseState::default();
        let start: Value = serde_json::from_str(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"foo"}}"#,
        )
        .unwrap();
        let events = map_sse_json(&start, &mut state).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::ToolCallDelta {
                index: 1,
                id: Some("call_1".to_string()),
                name: Some("foo".to_string()),
                arguments_delta: String::new(),
            }]
        );

        let delta: Value = serde_json::from_str(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
        )
        .unwrap();
        let events = map_sse_json(&delta, &mut state).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: "{\"a\":1}".to_string(),
            }]
        );
    }

    #[test]
    fn map_sse_json_content_block_start_text_is_ignored() {
        let mut state = AnthropicSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn map_sse_json_message_start_captures_input_tokens() {
        let mut state = AnthropicSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":42}}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert!(events.is_empty());
        assert_eq!(state.input_tokens, 42);
    }

    #[test]
    fn map_sse_json_message_delta_finish_end_turn() {
        let mut state = AnthropicSseState {
            input_tokens: 42,
            finished: false,
        };
        let v: Value = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 42,
                    output_tokens: 7,
                }),
            }]
        );
        assert!(state.finished);
    }

    #[test]
    fn map_sse_json_message_delta_finish_tool_use() {
        let mut state = AnthropicSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: 0,
                    output_tokens: 3,
                }),
            }]
        );
    }

    #[test]
    fn map_sse_json_message_delta_finish_max_tokens() {
        let mut state = AnthropicSseState::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":10}}"#,
        )
        .unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert_eq!(
            events[0],
            StreamEvent::Finished {
                reason: FinishReason::Length,
                usage: Some(Usage {
                    input_tokens: 0,
                    output_tokens: 10
                }),
            }
        );
    }

    #[test]
    fn map_sse_json_ping_and_message_stop_are_ignored() {
        let mut state = AnthropicSseState::default();
        for payload in [
            r#"{"type":"ping"}"#,
            r#"{"type":"message_stop"}"#,
            r#"{"type":"content_block_stop","index":0}"#,
        ] {
            let v: Value = serde_json::from_str(payload).unwrap();
            let events = map_sse_json(&v, &mut state).unwrap();
            assert!(events.is_empty());
        }
    }

    #[test]
    fn map_sse_json_error_is_error() {
        let mut state = AnthropicSseState::default();
        let v: Value =
            serde_json::from_str(r#"{"type":"error","error":{"message":"overloaded"}}"#).unwrap();
        let err = map_sse_json(&v, &mut state).unwrap_err();
        match err {
            ModelError::Api { status, message } => {
                assert_eq!(status, 0);
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn map_sse_json_unknown_type_is_ignored() {
        let mut state = AnthropicSseState::default();
        let v: Value = serde_json::from_str(r#"{"type":"some_future_event"}"#).unwrap();
        let events = map_sse_json(&v, &mut state).unwrap();
        assert!(events.is_empty());
    }

    // --- auth load / refresh-needed / header selection ----------------------

    #[test]
    fn load_api_key_type() {
        let path = temp_file("auth.json", r#"{"type":"api","key":"sk-ant-test"}"#);
        let auth = load(&path).unwrap();
        match auth {
            AnthropicAuth::ApiKey(k) => assert_eq!(k, "sk-ant-test"),
            other => panic!("expected ApiKey, got {other:?}"),
        }
    }

    #[test]
    fn load_oauth_type() {
        let path = temp_file(
            "auth.json",
            r#"{"type":"oauth","access_token":"at","refresh_token":"rt","expires_at":123}"#,
        );
        let auth = load(&path).unwrap();
        match auth {
            AnthropicAuth::OAuth {
                access_token,
                refresh_token,
                expires_at,
            } => {
                assert_eq!(access_token, "at");
                assert_eq!(refresh_token, "rt");
                assert_eq!(expires_at, 123);
            }
            other => panic!("expected OAuth, got {other:?}"),
        }
    }

    #[test]
    fn load_oauth_missing_tokens_errors() {
        let path = temp_file("auth.json", r#"{"type":"oauth","access_token":""}"#);
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ModelError::Api { status: 0, .. }));
    }

    #[test]
    fn load_unknown_type_errors() {
        let path = temp_file("auth.json", r#"{"type":"mystery"}"#);
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("unknown type"));
    }

    #[test]
    fn load_missing_file_no_env_errors_actionable() {
        let _guard = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "kode-anthropic-test-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("auth.json");
        // Ensure the env var isn't set from a leaked prior test/environment.
        // SAFETY: test-only, serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let err = load(&path).unwrap_err();
        match err {
            ModelError::Api { status, message } => {
                assert_eq!(status, 0);
                assert!(message.contains("kode auth login anthropic"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn load_missing_file_falls_back_to_env_key() {
        let _guard = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "kode-anthropic-test-envfallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("auth.json");
        // SAFETY: test-only; serialized via ENV_LOCK.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-env-test");
        }
        let auth = load(&path).unwrap();
        match auth {
            AnthropicAuth::ApiKey(k) => assert_eq!(k, "sk-ant-env-test"),
            other => panic!("expected ApiKey, got {other:?}"),
        }
        // SAFETY: test-only cleanup, serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
    }

    #[test]
    fn needs_refresh_boundary() {
        let now = 10_000u64;
        assert!(!needs_refresh(now + REFRESH_BUFFER_SECS + 1, now));
        assert!(needs_refresh(now + REFRESH_BUFFER_SECS, now));
        assert!(needs_refresh(now, now));
        assert!(needs_refresh(now - 1, now));
    }

    #[test]
    fn auth_headers_api_key_sends_x_api_key_only() {
        let auth = AnthropicAuth::ApiKey("sk-ant-test".to_string());
        let headers = auth_headers(&auth);
        assert_eq!(headers, vec![("x-api-key", "sk-ant-test".to_string())]);
    }

    #[test]
    fn auth_headers_oauth_sends_bearer_and_beta_no_api_key() {
        let auth = AnthropicAuth::OAuth {
            access_token: "at-123".to_string(),
            refresh_token: "rt-456".to_string(),
            expires_at: 999,
        };
        let headers = auth_headers(&auth);
        assert_eq!(
            headers,
            vec![
                ("authorization", "Bearer at-123".to_string()),
                ("anthropic-beta", "oauth-2025-04-20".to_string()),
            ]
        );
        assert!(!headers.iter().any(|(name, _)| *name == "x-api-key"));
    }

    #[test]
    fn anthropic_model_new_loads_api_key_auth() {
        let path = temp_file("auth.json", r#"{"type":"api","key":"sk-ant-test"}"#);
        let model = AnthropicModel::new(path, "claude-sonnet-5".to_string()).unwrap();
        assert_eq!(model.capabilities().id, "claude-sonnet-5");
        assert!(model.capabilities().supports_tools);
        assert!(model.capabilities().supports_streaming);
    }

    #[test]
    fn anthropic_model_new_missing_auth_errors() {
        let _guard = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "kode-anthropic-test-nomodel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("auth.json");
        // SAFETY: test-only, serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let err = AnthropicModel::new(path, "claude-sonnet-5".to_string()).unwrap_err();
        assert!(err.to_string().contains("kode auth login anthropic"));
    }
}
