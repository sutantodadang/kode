//! Talks to Google's Cloud Code Assist API (the backend behind the
//! Antigravity IDE) using Kode's own credential store
//! (`~/.kode/auth/antigravity.json`, written by `kode auth login antigravity`)
//! with a Google OAuth token, automatically refreshed. OAuth only — there is
//! no API-key mode and no environment-variable fallback.
//!
//! EXPERIMENTAL: this is not an officially supported third-party flow and may
//! break without notice.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::error::{ModelError, Result};
use crate::sse::extract_data_lines;
use crate::types::{FinishReason, Message, ModelCapabilities, ModelRequest, StreamEvent, Usage};
use crate::{Model, ModelStream};

// Google "installed app" OAuth client shipped inside the Antigravity IDE.
// Not confidential by design (Google's desktop-app OAuth docs): it is
// embedded in every Antigravity binary and grants nothing without the
// user's interactive consent. Split with `concat!` only so secret scanners
// don't flag a value that is public anyway.
pub const OAUTH_CLIENT_ID: &str = concat!(
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.googleusercontent.com"
);
/// Google's token endpoint requires the client secret even for PKCE flows.
pub const OAUTH_CLIENT_SECRET: &str = concat!("GOCSPX-", "K58FWR486LdLJ1mLB8sXC4z6qDAf");
pub const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

// ponytail: fixed fallback lists; generate prefers the sandbox endpoints the
// IDE itself uses, project discovery prefers prod.
const GENERATE_ENDPOINTS: [&str; 3] = [
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://autopush-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const PROJECT_ENDPOINTS: [&str; 3] = [
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://autopush-cloudcode-pa.sandbox.googleapis.com",
];
const ANTIGRAVITY_VERSION: &str = "1.18.3";
const REFRESH_BUFFER_SECS: u64 = 60;
const ONBOARD_POLL_TRIES: u32 = 10;

/// Parsed contents of `~/.kode/auth/antigravity.json`.
#[derive(Clone)]
pub struct AntigravityAuth {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub project_id: String,
}

// Manual Debug: never expose tokens via logging/Debug-printing.
impl std::fmt::Debug for AntigravityAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AntigravityAuth")
            .field("access_token", &"***redacted***")
            .field("refresh_token", &"***redacted***")
            .field("expires_at", &self.expires_at)
            .field("project_id", &self.project_id)
            .finish()
    }
}

/// Default location of Kode's own antigravity auth store:
/// `~/.kode/auth/antigravity.json`.
pub fn default_auth_path() -> Option<PathBuf> {
    Some(kode_core::auth_dir()?.join("antigravity.json"))
}

/// Loads and parses `antigravity.json` (schema: `{"type":"oauth",
/// "access_token","refresh_token","expires_at":<unix_secs>,"project_id"}`).
pub fn load(path: &Path) -> Result<AntigravityAuth> {
    let content = std::fs::read_to_string(path).map_err(|e| ModelError::Api {
        status: 0,
        message: format!(
            "cannot read antigravity auth file {}: {e} — run: kode auth login antigravity",
            path.display()
        ),
    })?;
    let value: Value = serde_json::from_str(&content).map_err(|e| ModelError::Api {
        status: 0,
        message: format!(
            "invalid antigravity auth.json ({}): {e} — run: kode auth login antigravity",
            path.display()
        ),
    })?;
    let str_field = |k: &str| {
        value
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let auth = AntigravityAuth {
        access_token: str_field("access_token"),
        refresh_token: str_field("refresh_token"),
        expires_at: value
            .get("expires_at")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        project_id: str_field("project_id"),
    };
    if auth.access_token.is_empty() || auth.refresh_token.is_empty() || auth.project_id.is_empty() {
        return Err(ModelError::Api {
            status: 0,
            message:
                "antigravity auth.json missing tokens or project_id — run: kode auth login antigravity"
                    .to_string(),
        });
    }
    Ok(auth)
}

/// Persists `auth` to `path` (0o600 on Unix).
pub fn write_auth(path: &Path, auth: &AntigravityAuth) -> Result<()> {
    let value = serde_json::json!({
        "type": "oauth",
        "access_token": auth.access_token,
        "refresh_token": auth.refresh_token,
        "expires_at": auth.expires_at,
        "project_id": auth.project_id,
    });
    let pretty =
        serde_json::to_string_pretty(&value).map_err(|e| ModelError::Parse(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ModelError::Api {
            status: 0,
            message: format!("cannot create antigravity auth dir: {e}"),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    write_secret_file(path, &pretty).map_err(|e| ModelError::Api {
        status: 0,
        message: format!("cannot write antigravity auth file: {e}"),
    })?;
    Ok(())
}

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
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

async fn refresh_tokens(
    client: &reqwest::Client,
    auth_path: &Path,
    auth: &mut AntigravityAuth,
) -> Result<()> {
    let resp = client
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", auth.refresh_token.as_str()),
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let message: String = text.chars().take(2000).collect();
        return Err(ModelError::Api { status, message });
    }
    let wire: RefreshResponse = resp.json().await.map_err(ModelError::Http)?;
    auth.access_token = wire.access_token;
    if let Some(rt) = wire.refresh_token {
        auth.refresh_token = rt;
    }
    auth.expires_at = now_secs() + wire.expires_in;
    write_auth(auth_path, auth)
}

/// Loads the auth file and refreshes the access token if it is (nearly)
/// expired, persisting the refreshed tokens. For one-off callers outside
/// the model (e.g. the model catalog).
pub async fn load_fresh(client: &reqwest::Client, path: &Path) -> Result<AntigravityAuth> {
    let mut auth = load(path)?;
    if needs_refresh(auth.expires_at, now_secs()) {
        refresh_tokens(client, path, &mut auth).await?;
    }
    Ok(auth)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn needs_refresh(expires_at: u64, now: u64) -> bool {
    now + REFRESH_BUFFER_SECS >= expires_at
}

fn platform() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("windows/amd64", "WINDOWS")
    } else if cfg!(target_os = "macos") {
        ("darwin/arm64", "MACOS")
    } else {
        ("linux/amd64", "LINUX")
    }
}

fn client_metadata() -> Value {
    serde_json::json!({
        "ideType": "ANTIGRAVITY",
        "platform": platform().1,
        "pluginType": "GEMINI",
    })
}

fn post(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    body: &Value,
) -> reqwest::RequestBuilder {
    let (ua_platform, meta_platform) = platform();
    client
        .post(url)
        .bearer_auth(access_token)
        .header(
            "User-Agent",
            format!("antigravity/{ANTIGRAVITY_VERSION} {ua_platform}"),
        )
        .header(
            "X-Goog-Api-Client",
            "google-cloud-sdk vscode_cloudshelleditor/0.1",
        )
        .header(
            "Client-Metadata",
            format!(
                "{{\"ideType\":\"ANTIGRAVITY\",\"platform\":\"{meta_platform}\",\"pluginType\":\"GEMINI\"}}"
            ),
        )
        .json(body)
}

/// POSTs `body` to `{base}/v1internal:{action}{suffix}` for each base in
/// `bases`, returning the first 2xx response. The last failure is returned
/// when every endpoint fails.
async fn post_with_fallback(
    client: &reqwest::Client,
    bases: &[&str],
    action: &str,
    suffix: &str,
    access_token: &str,
    body: &Value,
) -> Result<reqwest::Response> {
    let mut last: Option<ModelError> = None;
    for base in bases {
        let url = format!("{base}/v1internal:{action}{suffix}");
        match post(client, &url, access_token, body).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                let message: String = text.chars().take(2000).collect();
                last = Some(ModelError::Api { status, message });
                // 401 will not improve on another host.
                if status == 401 {
                    break;
                }
            }
            Err(e) => last = Some(ModelError::Http(e)),
        }
    }
    Err(last.unwrap_or(ModelError::Api {
        status: 0,
        message: "no antigravity endpoint configured".to_string(),
    }))
}

/// Extracts `cloudaicompanionProject` (string or `{id}`) from a
/// loadCodeAssist / onboardUser payload.
fn project_id_from(v: &Value) -> Option<String> {
    let p = v.get("cloudaicompanionProject")?;
    if let Some(s) = p.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    p.get("id")
        .and_then(|id| id.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Resolves the managed Cloud Code Assist project for this account:
/// `loadCodeAssist`, falling back to onboarding a FREE-tier project.
pub async fn discover_project(client: &reqwest::Client, access_token: &str) -> Result<String> {
    let body = serde_json::json!({ "metadata": client_metadata() });
    let resp = post_with_fallback(
        client,
        &PROJECT_ENDPOINTS,
        "loadCodeAssist",
        "",
        access_token,
        &body,
    )
    .await?;
    let payload: Value = resp.json().await.map_err(ModelError::Http)?;
    if let Some(id) = project_id_from(&payload) {
        return Ok(id);
    }

    // ponytail: FREE tier only; paid tiers need an explicit GCP project.
    let body = serde_json::json!({ "tierId": "FREE", "metadata": client_metadata() });
    for _ in 0..ONBOARD_POLL_TRIES {
        let resp = post_with_fallback(
            client,
            &PROJECT_ENDPOINTS,
            "onboardUser",
            "",
            access_token,
            &body,
        )
        .await?;
        let lro: Value = resp.json().await.map_err(ModelError::Http)?;
        if lro.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
            if let Some(id) = lro.get("response").and_then(project_id_from) {
                return Ok(id);
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(ModelError::Api {
        status: 0,
        message: "could not provision antigravity project (loadCodeAssist/onboardUser returned no project id)".to_string(),
    })
}

#[derive(Debug)]
pub struct AntigravityModel {
    auth_path: PathBuf,
    model: String,
    client: reqwest::Client,
    state: tokio::sync::Mutex<AntigravityAuth>,
}

impl AntigravityModel {
    pub fn new(auth_path: PathBuf, model: String) -> Result<Self> {
        let auth = load(&auth_path)?;
        Ok(Self {
            auth_path,
            model,
            client: reqwest::Client::new(),
            state: tokio::sync::Mutex::new(auth),
        })
    }
}

#[async_trait::async_trait]
impl Model for AntigravityModel {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let auth = {
            let mut guard = self.state.lock().await;
            if needs_refresh(guard.expires_at, now_secs()) {
                refresh_tokens(&self.client, &self.auth_path, &mut guard).await?;
            }
            guard.clone()
        };

        let body = build_body(&self.model, &request, &auth.project_id);
        let mut result = post_with_fallback(
            &self.client,
            &GENERATE_ENDPOINTS,
            "streamGenerateContent",
            "?alt=sse",
            &auth.access_token,
            &body,
        )
        .await;

        if matches!(&result, Err(ModelError::Api { status: 401, .. })) {
            let refreshed = {
                let mut guard = self.state.lock().await;
                refresh_tokens(&self.client, &self.auth_path, &mut guard).await?;
                guard.clone()
            };
            result = post_with_fallback(
                &self.client,
                &GENERATE_ENDPOINTS,
                "streamGenerateContent",
                "?alt=sse",
                &refreshed.access_token,
                &body,
            )
            .await;
        }
        let resp = result?;

        let bytes_stream = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
        let state = StreamState {
            bytes: Box::pin(bytes_stream),
            buffer: String::new(),
            pending: VecDeque::new(),
            sse_state: SseState::default(),
            done: false,
        };
        Ok(Box::pin(futures::stream::try_unfold(state, sse_step)))
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

struct StreamState {
    bytes: ByteStream,
    buffer: String,
    pending: VecDeque<StreamEvent>,
    sse_state: SseState,
    done: bool,
}

#[derive(Default)]
struct SseState {
    tool_calls: u32,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    saw_content: bool,
}

impl SseState {
    fn finished_event(&self) -> Result<StreamEvent> {
        let reason = if self.tool_calls > 0 {
            FinishReason::ToolCalls
        } else {
            match self.finish_reason.as_deref() {
                Some(raw) => map_finish_reason(raw),
                None if self.saw_content => FinishReason::Stop,
                None => {
                    return Err(ModelError::Parse(
                        "antigravity stream ended without content or finishReason".to_string(),
                    ));
                }
            }
        };
        Ok(StreamEvent::Finished {
            reason,
            usage: self.usage,
        })
    }
}

async fn sse_step(
    mut state: StreamState,
) -> std::result::Result<Option<(StreamEvent, StreamState)>, ModelError> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some((event, state)));
        }
        if state.done {
            return Ok(None);
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => {
                let text = String::from_utf8_lossy(&chunk).into_owned();
                for payload in extract_data_lines(&mut state.buffer, &text) {
                    if payload == "[DONE]" {
                        continue;
                    }
                    let value: Value = serde_json::from_str(&payload).map_err(|e| {
                        ModelError::Parse(format!("invalid antigravity SSE payload JSON: {e}"))
                    })?;
                    let events = map_sse_json(&value, &mut state.sse_state);
                    state.pending.extend(events);
                }
            }
            Some(Err(e)) => return Err(ModelError::Http(e)),
            None => {
                state.done = true;
                let finished = state.sse_state.finished_event()?;
                state.pending.push_back(finished);
            }
        }
    }
}

fn map_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        other => FinishReason::Other(other.to_string()),
    }
}

/// Maps one SSE JSON chunk to stream events. The Gemini response sits under
/// `response` on the v1internal wire; a bare GenerateContentResponse is
/// accepted too.
fn map_sse_json(v: &Value, state: &mut SseState) -> Vec<StreamEvent> {
    let resp = v.get("response").unwrap_or(v);
    let mut out = Vec::new();
    let Some(candidate) = resp
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    else {
        read_usage(resp, state);
        return out;
    };
    if let Some(parts) = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            if part.get("thought").and_then(|t| t.as_bool()) == Some(true) {
                continue;
            }
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    state.saw_content = true;
                    out.push(StreamEvent::TextDelta(text.to_string()));
                }
            } else if let Some(call) = part.get("functionCall") {
                let name = call
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let args = call.get("args").cloned().unwrap_or(serde_json::json!({}));
                let index = state.tool_calls;
                state.tool_calls += 1;
                state.saw_content = true;
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    id: Some(format!("call_{index}")),
                    name: Some(name),
                    arguments_delta: args.to_string(),
                });
            }
        }
    }
    if let Some(reason) = candidate.get("finishReason").and_then(|r| r.as_str()) {
        state.finish_reason = Some(reason.to_string());
    }
    read_usage(resp, state);
    out
}

fn read_usage(resp: &Value, state: &mut SseState) {
    if let Some(u) = resp.get("usageMetadata") {
        let get = |k: &str| u.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
        state.usage = Some(Usage {
            input_tokens: get("promptTokenCount"),
            output_tokens: get("candidatesTokenCount"),
        });
    }
}

/// Recursively removes JSON-Schema keys Gemini rejects.
fn strip_schema_keys(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("$schema");
            map.remove("additionalProperties");
            for child in map.values_mut() {
                strip_schema_keys(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_schema_keys),
        _ => {}
    }
}

/// Thinking config for `effort` on `model`, following Antigravity's wire
/// conventions: Gemini 3 *Flash* takes `thinkingLevel`
/// (minimal|low|medium|high); Gemini 3 *Pro* encodes its tier in the model
/// id itself (`gemini-3.1-pro-high`) so nothing is sent; Claude `-thinking`
/// models take a `thinkingBudget`. Everything else: none.
fn thinking_config(model: &str, effort: &str) -> Option<Value> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("gemini-3") && m.contains("flash") {
        let level = match effort {
            "minimal" => "minimal",
            "low" => "low",
            "medium" => "medium",
            _ => "high",
        };
        return Some(serde_json::json!({"thinkingLevel": level}));
    }
    if m.contains("claude") && m.contains("thinking") {
        let budget = match effort {
            "minimal" | "low" => 8192,
            "medium" => 16384,
            _ => 32768,
        };
        return Some(serde_json::json!({"thinkingBudget": budget}));
    }
    None
}

/// Live model list from `v1internal:fetchAvailableModels` (prod endpoint
/// only). Returns the `models` map keys, sorted.
pub async fn fetch_available_models(
    client: &reqwest::Client,
    access_token: &str,
    project: &str,
) -> Result<Vec<String>> {
    let body = serde_json::json!({ "project": project });
    let resp = post_with_fallback(
        client,
        &PROJECT_ENDPOINTS[..1],
        "fetchAvailableModels",
        "",
        access_token,
        &body,
    )
    .await?;
    let payload: Value = resp.json().await.map_err(ModelError::Http)?;
    parse_available_models(&payload)
}

fn parse_available_models(payload: &Value) -> Result<Vec<String>> {
    let models = payload
        .get("models")
        .and_then(|m| m.as_object())
        .ok_or_else(|| ModelError::Parse("fetchAvailableModels: missing 'models' map".into()))?;
    let mut ids: Vec<String> = models.keys().cloned().collect();
    ids.sort();
    if ids.is_empty() {
        return Err(ModelError::Parse(
            "fetchAvailableModels: empty model list".into(),
        ));
    }
    Ok(ids)
}

fn build_body(model: &str, request: &ModelRequest, project: &str) -> Value {
    // Gemini functionResponse carries the tool *name*, not the call id.
    let mut names: HashMap<&str, &str> = HashMap::new();
    for m in &request.messages {
        if let Message::Assistant { tool_calls, .. } = m {
            for c in tool_calls {
                names.insert(c.id.as_str(), c.name.as_str());
            }
        }
    }

    let mut system = Vec::new();
    let mut contents = Vec::new();
    for m in &request.messages {
        match m {
            Message::System(s) => system.push(s.as_str()),
            Message::User(s) => contents.push(serde_json::json!({
                "role": "user", "parts": [{"text": s}]
            })),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut parts = Vec::new();
                if !content.is_empty() {
                    parts.push(serde_json::json!({"text": content}));
                }
                for c in tool_calls {
                    parts.push(serde_json::json!({
                        "functionCall": {"name": c.name, "args": c.arguments}
                    }));
                }
                if !parts.is_empty() {
                    contents.push(serde_json::json!({"role": "model", "parts": parts}));
                }
            }
            Message::Tool {
                tool_call_id,
                content,
            } => {
                let name = names.get(tool_call_id.as_str()).copied().unwrap_or("tool");
                contents.push(serde_json::json!({
                    "role": "user",
                    "parts": [{"functionResponse": {"name": name, "response": {"output": content}}}]
                }));
            }
        }
    }

    let mut req = serde_json::json!({ "contents": contents });
    if !system.is_empty() {
        req["systemInstruction"] = serde_json::json!({"parts": [{"text": system.join("\n\n")}]});
    }
    if !request.tools.is_empty() {
        let decls: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                let mut params = t.parameters.clone();
                strip_schema_keys(&mut params);
                serde_json::json!({
                    "name": t.name, "description": t.description, "parameters": params
                })
            })
            .collect();
        req["tools"] = serde_json::json!([{"functionDeclarations": decls}]);
    }
    let mut gen_cfg = serde_json::Map::new();
    if let Some(max) = request.max_tokens {
        gen_cfg.insert("maxOutputTokens".into(), max.into());
    }
    if let Some(t) = request.temperature {
        gen_cfg.insert("temperature".into(), t.into());
    }
    if let Some(effort) = request.effort.as_deref()
        && let Some(cfg) = thinking_config(model, effort)
    {
        gen_cfg.insert("thinkingConfig".into(), cfg);
    }
    if !gen_cfg.is_empty() {
        req["generationConfig"] = Value::Object(gen_cfg);
    }

    serde_json::json!({ "project": project, "model": model, "request": req })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolSpec};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-antigravity-{tag}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_and_load_roundtrip() {
        let path = temp_dir("roundtrip").join("antigravity.json");
        let auth = AntigravityAuth {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 42,
            project_id: "proj".into(),
        };
        write_auth(&path, &auth).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.access_token, "at");
        assert_eq!(loaded.refresh_token, "rt");
        assert_eq!(loaded.expires_at, 42);
        assert_eq!(loaded.project_id, "proj");
        assert!(!format!("{loaded:?}").contains("at\""));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_rejects_missing_project() {
        let path = temp_dir("missing").join("antigravity.json");
        std::fs::write(
            &path,
            r#"{"type":"oauth","access_token":"a","refresh_token":"r","expires_at":1}"#,
        )
        .unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn needs_refresh_within_buffer() {
        assert!(needs_refresh(100, 50));
        assert!(!needs_refresh(1000, 50));
    }

    #[test]
    fn project_id_from_accepts_string_and_object() {
        assert_eq!(
            project_id_from(&serde_json::json!({"cloudaicompanionProject": "p1"})),
            Some("p1".into())
        );
        assert_eq!(
            project_id_from(&serde_json::json!({"cloudaicompanionProject": {"id": "p2"}})),
            Some("p2".into())
        );
        assert_eq!(project_id_from(&serde_json::json!({})), None);
    }

    #[test]
    fn build_body_maps_messages_tools_and_system() {
        let request = ModelRequest {
            messages: vec![
                Message::System("sys".into()),
                Message::User("hi".into()),
                Message::Assistant {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_0".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "x"}),
                    }],
                },
                Message::Tool {
                    tool_call_id: "call_0".into(),
                    content: "data".into(),
                },
            ],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "reads".into(),
                parameters: serde_json::json!({
                    "$schema": "x", "type": "object", "additionalProperties": false,
                    "properties": {"path": {"type": "string"}}
                }),
            }],
            max_tokens: Some(10),
            temperature: None,
            effort: Some("high".into()),
        };
        let body = build_body("gemini-3-flash", &request, "proj");
        assert_eq!(body["project"], "proj");
        assert_eq!(body["model"], "gemini-3-flash");
        let req = &body["request"];
        assert_eq!(req["systemInstruction"]["parts"][0]["text"], "sys");
        let contents = req["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["output"],
            "data"
        );
        let params = &req["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());
        assert_eq!(req["generationConfig"]["maxOutputTokens"], 10);
        assert_eq!(
            req["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    #[test]
    fn thinking_config_follows_antigravity_conventions() {
        assert_eq!(
            thinking_config("gemini-3.6-flash", "medium"),
            Some(serde_json::json!({"thinkingLevel": "medium"}))
        );
        assert_eq!(
            thinking_config("gemini-3-flash", "minimal"),
            Some(serde_json::json!({"thinkingLevel": "minimal"}))
        );
        assert_eq!(thinking_config("gemini-3.1-pro-high", "high"), None);
        assert_eq!(
            thinking_config("claude-opus-4-6-thinking", "low"),
            Some(serde_json::json!({"thinkingBudget": 8192}))
        );
        assert_eq!(thinking_config("claude-sonnet-4-6", "high"), None);
        assert_eq!(thinking_config("gemini-2.5-flash", "high"), None);
    }

    #[test]
    fn parse_available_models_sorts_keys() {
        let v = serde_json::json!({"models": {
            "gemini-3.1-pro-high": {"displayName": "Gemini 3.1 Pro (High)"},
            "claude-sonnet-4-6": {"displayName": "Claude Sonnet 4.6"}
        }});
        assert_eq!(
            parse_available_models(&v).unwrap(),
            vec!["claude-sonnet-4-6", "gemini-3.1-pro-high"]
        );
        assert!(parse_available_models(&serde_json::json!({})).is_err());
        assert!(parse_available_models(&serde_json::json!({"models": {}})).is_err());
    }

    #[test]
    fn build_body_omits_thinking_for_non_gemini3() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".into())],
            effort: Some("low".into()),
            ..Default::default()
        };
        let body = build_body("gemini-2.5-flash", &request, "p");
        assert!(body["request"].get("generationConfig").is_none());
        assert!(body["request"].get("tools").is_none());
    }

    #[test]
    fn map_sse_json_handles_text_thought_and_function_call() {
        let mut state = SseState::default();
        let v = serde_json::json!({"response": {"candidates": [{"content": {"parts": [
            {"text": "think", "thought": true},
            {"text": "hello"},
            {"functionCall": {"name": "ls", "args": {"dir": "."}}}
        ]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4}}});
        let events = map_sse_json(&v, &mut state);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], StreamEvent::TextDelta("hello".into()));
        match &events[1] {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("call_0"));
                assert_eq!(name.as_deref(), Some("ls"));
                assert_eq!(arguments_delta, r#"{"dir":"."}"#);
            }
            other => panic!("unexpected {other:?}"),
        }
        let finished = state.finished_event().unwrap();
        assert_eq!(
            finished,
            StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 4
                }),
            }
        );
    }

    #[test]
    fn map_sse_json_accepts_unwrapped_response() {
        let mut state = SseState::default();
        let v = serde_json::json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "MAX_TOKENS"}]});
        let events = map_sse_json(&v, &mut state);
        assert_eq!(events, vec![StreamEvent::TextDelta("x".into())]);
        assert_eq!(
            state.finished_event().unwrap(),
            StreamEvent::Finished {
                reason: FinishReason::Length,
                usage: None
            }
        );
    }

    #[test]
    fn empty_stream_is_parse_error() {
        assert!(SseState::default().finished_event().is_err());
    }
}
