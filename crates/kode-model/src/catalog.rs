//! Best-effort model catalogs per provider, for the interactive model picker
//! and `kode models`. There is no universal "list models" endpoint, so each
//! provider is handled on its own terms; failures are returned as `Err`
//! strings rather than propagated as hard errors — callers keep working with
//! free-text model entry when a catalog can't be fetched.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const LMSTUDIO_URL: &str = "http://127.0.0.1:1234/v1/models";
const OPENAI_MODELS_URL: &str = "https://api.openai.com/v1/models";
const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";

static CONTEXT_WINDOW_CACHE: OnceLock<Mutex<HashMap<(String, String), u32>>> = OnceLock::new();

/// Last-resort candidates when both the account and public catalogs fail.
const ANTHROPIC_FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-haiku-4-5-20251001",
];

/// Static candidates used when the live `fetchAvailableModels` call fails
/// (not logged in, network error). Pro tiers are encoded in the id.
const ANTIGRAVITY_FALLBACK_MODELS: &[&str] = &[
    "gemini-3.1-pro-high",
    "gemini-3.1-pro-low",
    "gemini-3-flash",
    "claude-sonnet-4-6",
    "claude-opus-4-6-thinking",
];

/// Codex CLI release version reported as `client_version` when listing
/// models. The ChatGPT backend filters its response by this value — an
/// outdated version can silently return an empty model list — so this needs
/// occasional bumping to track the current Codex CLI release.
const CODEX_CLIENT_VERSION: &str = "0.155.0";

/// Static candidates used when the live codex model fetch fails or returns
/// nothing usable (no auth, network error, parse error, empty list).
const CODEX_FALLBACK_MODELS: &[&str] = &[
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];

#[derive(Debug)]
pub struct ModelCatalog {
    pub models: Vec<String>,
    /// Explains fallback data; live account catalogs have no warning.
    pub note: Option<String>,
}

fn catalog_or_fallback(
    result: Result<Vec<String>, String>,
    fallback: &[&str],
    label: &str,
) -> ModelCatalog {
    match result {
        Ok(models) => ModelCatalog { models, note: None },
        Err(error) => {
            tracing::debug!(%error, "using fallback model catalog");
            ModelCatalog {
                models: fallback.iter().map(|id| (*id).to_string()).collect(),
                note: Some(label.to_string()),
            }
        }
    }
}

/// Lists candidate model ids for `provider`. `api_key_env`, when given,
/// names an environment variable to read the API key from (used for the
/// "openai" provider); it falls back to `OPENAI_API_KEY`/`KODE_API_KEY`.
pub async fn list_models(
    provider: &str,
    api_key_env: Option<String>,
) -> Result<Vec<String>, String> {
    list_catalog(provider, api_key_env)
        .await
        .map(|catalog| catalog.models)
}

/// Always fetches again, including when the same provider is reselected.
pub async fn list_catalog(
    provider: &str,
    api_key_env: Option<String>,
) -> Result<ModelCatalog, String> {
    match provider {
        "codex" => Ok(catalog_or_fallback(
            fetch_codex_models().await,
            CODEX_FALLBACK_MODELS,
            "static fallback; account availability unverified",
        )),
        "anthropic" => match fetch_anthropic_models().await {
            Ok(models) => Ok(ModelCatalog { models, note: None }),
            Err(error) => {
                tracing::debug!(%error, "Anthropic account catalog unavailable");
                let mut catalog = catalog_or_fallback(
                    fetch_models_dev("anthropic").await,
                    ANTHROPIC_FALLBACK_MODELS,
                    "static fallback; account availability unverified",
                );
                if catalog.note.is_none() {
                    catalog.note = Some("public catalog; account availability unverified".into());
                }
                Ok(catalog)
            }
        },
        "antigravity" => Ok(catalog_or_fallback(
            fetch_antigravity_models().await,
            ANTIGRAVITY_FALLBACK_MODELS,
            "static fallback; account availability unverified",
        )),
        other => {
            let models = match other {
                "opencode-go" | "opencode" => fetch_opencode_models(other).await,
                "kilo" => fetch_models_dev(other).await,
                "lmstudio" => fetch_lmstudio().await,
                "openai" => fetch_openai(api_key_env).await,
                _ => Err(format!("no model catalog for provider '{other}'")),
            }?;
            Ok(ModelCatalog { models, note: None })
        }
    }
}

/// Resolves the selected model's input context window from the same live
/// catalogs used by the model picker. Results are cached for the process.
/// `None` lets callers use a conservative fallback without blocking a run.
pub async fn context_window_tokens(provider: &str, model: &str) -> Option<u32> {
    let key = (provider.to_string(), model.to_string());
    let cache = CONTEXT_WINDOW_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(tokens) = cache.lock().ok()?.get(&key).copied() {
        return Some(tokens);
    }

    let detected = match provider {
        "codex" => fetch_codex_models_json()
            .await
            .ok()
            .and_then(|json| parse_codex_context_window(&json, model)),
        "openai" | "anthropic" | "opencode-go" | "opencode" | "kilo" => fetch_models_dev_json()
            .await
            .ok()
            .and_then(|json| parse_models_dev_context_window(&json, provider, model)),
        "antigravity" => inferred_context_window(model),
        "lmstudio" => None,
        _ => None,
    }
    .or_else(|| inferred_context_window(model));

    if let Some(tokens) = detected
        && let Ok(mut cache) = cache.lock()
    {
        cache.insert(key, tokens);
    }
    detected
}

fn inferred_context_window(model: &str) -> Option<u32> {
    let model = model.to_ascii_lowercase();
    if model.starts_with("gemini-3") || model.starts_with("gemini-2.5") {
        Some(1_000_000)
    } else if model.starts_with("claude-") {
        Some(200_000)
    } else if model.starts_with("gpt-") || model.starts_with('o') {
        Some(256_000)
    } else {
        None
    }
}

/// Fetches the live Antigravity model list using the stored OAuth token.
/// Errors when not logged in or the call fails; callers fall back to
/// [`ANTIGRAVITY_FALLBACK_MODELS`].
async fn fetch_antigravity_models() -> Result<Vec<String>, String> {
    let path = crate::antigravity::default_auth_path()
        .ok_or_else(|| "cannot resolve home directory".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let auth = crate::antigravity::load_fresh(&client, &path)
        .await
        .map_err(|e| e.to_string())?;
    crate::antigravity::fetch_available_models(&client, &auth.access_token, &auth.project_id)
        .await
        .map_err(|e| e.to_string())
}

/// Tries the account catalog with the same auth as inference. If the account
/// cannot access this endpoint, the caller labels its public/static fallback.
async fn fetch_anthropic_models() -> Result<Vec<String>, String> {
    let path = crate::anthropic::default_auth_path()
        .ok_or_else(|| "no anthropic auth path".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    tokio::time::timeout(FETCH_TIMEOUT, async {
        let auth = crate::anthropic::load_fresh(&client, &path)
            .await
            .map_err(|e| e.to_string())?;
        fetch_anthropic_pages(&client, ANTHROPIC_MODELS_URL, &auth).await
    })
    .await
    .map_err(|_| "anthropic models fetch timed out".to_string())?
}

async fn fetch_anthropic_pages(
    client: &reqwest::Client,
    url: &str,
    auth: &crate::anthropic::AnthropicAuth,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    let mut after = None;
    loop {
        let mut request = client
            .get(url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .query(&[("limit", "1000")]);
        for (name, value) in crate::anthropic::auth_headers(auth) {
            request = request.header(name, value);
        }
        if let Some(cursor) = &after {
            request = request.query(&[("after_id", cursor)]);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| format!("anthropic models fetch failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("anthropic models returned {}", resp.status()));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| format!("anthropic models read failed: {e}"))?;
        for id in parse_anthropic_models(&text)? {
            if seen_ids.insert(id.clone()) {
                ids.push(id);
            }
        }
        let page: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if page.get("has_more").and_then(Value::as_bool) != Some(true) {
            break;
        }
        let cursor = page
            .get("last_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "anthropic pagination missing last_id".to_string())?;
        if !cursors.insert(cursor.to_string()) {
            return Err("anthropic pagination repeated cursor".to_string());
        }
        after = Some(cursor.to_string());
    }
    if ids.is_empty() {
        return Err("anthropic models list empty".to_string());
    }
    Ok(ids)
}

/// Extracts model ids (in response order) from an Anthropic
/// `{"data":[{"id":...},...]}` `/v1/models` response body.
fn parse_anthropic_models(json: &str) -> Result<Vec<String>, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid anthropic models JSON: {e}"))?;
    let data = value
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "missing 'data' array in anthropic models response".to_string())?;
    let ids: Vec<String> = data
        .iter()
        .filter_map(|item| {
            item.get("id")
                .and_then(|i| i.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    Ok(ids)
}

/// Fetches the account's live codex model list from the ChatGPT backend.
/// Requires codex auth (`~/.kode/auth/codex.json`, refreshed if stale). On
/// any failure — missing auth, network error, non-2xx, parse error — or an
/// empty filtered list, returns `Err` so the caller falls back to
/// [`CODEX_FALLBACK_MODELS`].
async fn fetch_codex_models() -> Result<Vec<String>, String> {
    let text = fetch_codex_models_json().await?;
    let models = parse_codex_models(&text)?;
    if models.is_empty() {
        return Err("codex models list empty".to_string());
    }
    Ok(models)
}

async fn fetch_codex_models_json() -> Result<String, String> {
    let auth_path =
        crate::codex::default_auth_path().ok_or_else(|| "no codex auth path".to_string())?;
    let auth = crate::codex::load_fresh(&auth_path)
        .await
        .map_err(|e| e.to_string())?;

    fetch_codex_models_json_at(CODEX_MODELS_URL, &auth).await
}

async fn fetch_codex_models_json_at(
    url: &str,
    auth: &crate::codex::CodexAuth,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .query(&[("client_version", CODEX_CLIENT_VERSION)])
        .bearer_auth(&auth.access_token)
        .header("chatgpt-account-id", &auth.account_id)
        .header("originator", "codex_cli_rs")
        .header("accept", "application/json")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("codex models fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("codex models returned {}", resp.status()));
    }
    resp.text()
        .await
        .map_err(|e| format!("codex models read failed: {e}"))
}

#[derive(serde::Deserialize, Default)]
struct CodexModelInfo {
    slug: String,
    #[serde(default)]
    #[allow(dead_code)]
    display_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    max_context_window: Option<u32>,
    #[serde(default)]
    effective_context_window_percent: Option<u32>,
}

#[derive(serde::Deserialize, Default)]
struct CodexModelsResponse {
    #[serde(default)]
    models: Vec<CodexModelInfo>,
}

/// Parses a `/backend-api/codex/models` response body into a slug list:
/// drops `visibility == "hide"` entries, then sorts by `priority` ascending
/// (missing priority sorts last), tie-broken by slug. Unknown/extra JSON
/// fields are ignored.
fn parse_codex_models(json: &str) -> Result<Vec<String>, String> {
    let parsed: CodexModelsResponse =
        serde_json::from_str(json).map_err(|e| format!("invalid codex models JSON: {e}"))?;
    let mut models: Vec<CodexModelInfo> = parsed
        .models
        .into_iter()
        .filter(|m| m.visibility.as_deref() != Some("hide"))
        .collect();
    models.sort_by(|a, b| {
        a.priority
            .unwrap_or(i64::MAX)
            .cmp(&b.priority.unwrap_or(i64::MAX))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    Ok(models.into_iter().map(|m| m.slug).collect())
}

fn parse_codex_context_window(json: &str, model: &str) -> Option<u32> {
    let parsed: CodexModelsResponse = serde_json::from_str(json).ok()?;
    let info = parsed
        .models
        .into_iter()
        .find(|entry| entry.slug == model)?;
    let raw = info.max_context_window.or(info.context_window)?;
    let percent = info
        .effective_context_window_percent
        .unwrap_or(100)
        .min(100);
    Some((u64::from(raw) * u64::from(percent) / 100).min(u64::from(u32::MAX)) as u32)
}

async fn fetch_models_dev_json() -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(MODELS_DEV_URL)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("models.dev fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("models.dev returned {}", resp.status()));
    }
    resp.text()
        .await
        .map_err(|e| format!("models.dev read failed: {e}"))
}

fn parse_models_dev_context_window(json: &str, provider: &str, model: &str) -> Option<u32> {
    let value: Value = serde_json::from_str(json).ok()?;
    value
        .get(provider)?
        .get("models")?
        .get(model)?
        .get("limit")?
        .get("context")?
        .as_u64()
        .and_then(|tokens| u32::try_from(tokens).ok())
}

async fn fetch_models_dev(provider: &str) -> Result<Vec<String>, String> {
    let text = fetch_models_dev_json().await?;
    parse_models_dev(&text, provider)
}

/// Extracts sorted model ids from a models.dev `api.json` payload for one
/// provider id: `{ "<provider>": { "models": { "<model-id>": {...} } } }`.
fn parse_models_dev(json: &str, provider: &str) -> Result<Vec<String>, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid models.dev JSON: {e}"))?;
    let models = value
        .get(provider)
        .and_then(|p| p.get("models"))
        .and_then(|m| m.as_object())
        .ok_or_else(|| format!("provider '{provider}' not found in models.dev registry"))?;
    let mut ids: Vec<String> = models.keys().cloned().collect();
    ids.sort();
    Ok(ids)
}

async fn fetch_lmstudio() -> Result<Vec<String>, String> {
    fetch_openai_style_models(LMSTUDIO_URL, "lmstudio").await
}

fn opencode_models_url(provider: &str) -> Option<String> {
    match provider {
        "opencode-go" | "opencode" => Some(format!(
            "{}/models",
            crate::opencode::builtin_base_url(provider)?
        )),
        _ => None,
    }
}

async fn fetch_opencode_models(provider: &str) -> Result<Vec<String>, String> {
    let url = opencode_models_url(provider)
        .ok_or_else(|| format!("no OpenCode catalog for provider '{provider}'"))?;
    fetch_openai_style_models(&url, provider).await
}

async fn fetch_openai_style_models(url: &str, label: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("{label} fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{label} returned {}", resp.status()));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("{label} read failed: {e}"))?;
    parse_openai_models(&text)
}

async fn fetch_openai(api_key_env: Option<String>) -> Result<Vec<String>, String> {
    let key = api_key_env
        .and_then(|env| std::env::var(env).ok())
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .or_else(|| std::env::var("KODE_API_KEY").ok())
        .ok_or_else(|| "no API key found (set OPENAI_API_KEY or KODE_API_KEY)".to_string())?;

    let client = reqwest::Client::new();
    let resp = client
        .get(OPENAI_MODELS_URL)
        .bearer_auth(key)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("openai fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("openai returned {}", resp.status()));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("openai read failed: {e}"))?;
    let ids = parse_openai_models(&text)?;
    Ok(ids
        .into_iter()
        .filter(|id| id.starts_with("gpt-") || id.starts_with('o'))
        .collect())
}

/// Extracts sorted, de-duplicated model ids from an OpenAI-style
/// `{"data":[{"id":...},...]}` response body (used by both `lmstudio` and
/// `openai`).
fn parse_openai_models(json: &str) -> Result<Vec<String>, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("invalid models JSON: {e}"))?;
    let data = value
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "missing 'data' array in models response".to_string())?;
    let ids: BTreeSet<String> = data
        .iter()
        .filter_map(|item| {
            item.get("id")
                .and_then(|i| i.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn serve_pages(
        pages: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for page in pages {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0 && request.len() < 8192);
                    request.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (url, task)
    }

    #[tokio::test]
    async fn codex_request_uses_current_version_and_account_auth() {
        let (url, server) = serve_pages(vec![r#"{"models":[{"slug":"gpt-6-sol"}]}"#]).await;
        let auth = crate::codex::CodexAuth {
            access_token: "test-token".into(),
            refresh_token: String::new(),
            account_id: "test-account".into(),
            last_refresh: String::new(),
            api_key: None,
            auth_mode: "chatgpt".into(),
        };
        let json = fetch_codex_models_json_at(&url, &auth).await.unwrap();
        assert_eq!(parse_codex_models(&json).unwrap(), ["gpt-6-sol"]);
        let requests = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        let request = requests[0].to_ascii_lowercase();
        assert!(request.contains("client_version=0.155.0"));
        assert!(request.contains("authorization: bearer test-token"));
        assert!(request.contains("chatgpt-account-id: test-account"));
    }

    #[tokio::test]
    async fn anthropic_catalog_paginates_with_api_key_and_oauth() {
        use crate::anthropic::AnthropicAuth;
        for auth in [
            AnthropicAuth::ApiKey("test-key".into()),
            AnthropicAuth::OAuth {
                access_token: "test-token".into(),
                refresh_token: String::new(),
                expires_at: u64::MAX,
            },
        ] {
            let (url, server) = serve_pages(vec![
                r#"{"data":[{"id":"newest"}],"has_more":true,"last_id":"newest"}"#,
                r#"{"data":[{"id":"newest"},{"id":"older"}],"has_more":false}"#,
            ])
            .await;
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let ids = fetch_anthropic_pages(&client, &url, &auth).await.unwrap();
            assert_eq!(ids, ["newest", "older"]);
            let requests = tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
            assert!(!requests[0].contains("after_id"));
            assert!(requests[1].contains("after_id=newest"));
            for request in requests {
                let request = request.to_ascii_lowercase();
                assert!(request.contains("limit=1000"));
                assert!(request.contains("anthropic-version: 2023-06-01"));
                match &auth {
                    AnthropicAuth::ApiKey(_) => assert!(request.contains("x-api-key: test-key")),
                    AnthropicAuth::OAuth { .. } => {
                        assert!(request.contains("authorization: bearer test-token"));
                        assert!(request.contains("anthropic-beta: oauth-2025-04-20"));
                        assert!(!request.contains("x-api-key"));
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn anthropic_catalog_rejects_repeated_pagination_cursor() {
        let page = r#"{"data":[{"id":"one"}],"has_more":true,"last_id":"one"}"#;
        let (url, server) = serve_pages(vec![page, page]).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let err = fetch_anthropic_pages(
            &client,
            &url,
            &crate::anthropic::AnthropicAuth::ApiKey("test".into()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("repeated cursor"));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn codex_catalog_advertises_gpt6_capable_client() {
        assert_eq!(CODEX_CLIENT_VERSION, "0.155.0");
        assert!(CODEX_FALLBACK_MODELS.contains(&"gpt-6-sol"));
    }

    #[test]
    fn failed_catalog_is_explicitly_marked_as_fallback() {
        let catalog = catalog_or_fallback(Err("offline".into()), &["candidate"], "static fallback");
        assert_eq!(catalog.models, ["candidate"]);
        assert_eq!(catalog.note.as_deref(), Some("static fallback"));
    }

    #[tokio::test]
    #[ignore = "requires a live Codex account; run explicitly for catalog QA"]
    async fn list_models_codex_returns_nonempty_list() {
        let catalog = list_catalog("codex", None).await.unwrap();
        assert!(catalog.note.is_none(), "{:?}", catalog.note);
        assert!(!catalog.models.is_empty());
    }

    #[test]
    fn codex_fallback_models_used_when_fetch_fails() {
        let ids: Vec<String> = CODEX_FALLBACK_MODELS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(ids.contains(&"gpt-5.6-sol".to_string()));
        assert!(ids.contains(&"gpt-6-sol".to_string()));
    }

    #[tokio::test]
    async fn list_models_unknown_provider_errors() {
        let err = list_models("mystery", None).await.unwrap_err();
        assert!(err.contains("mystery"));
    }

    #[test]
    fn parse_models_dev_extracts_sorted_ids() {
        let json = r#"{"opencode-go": {"models": {"kimi-k3": {}, "abc-model": {}}}}"#;
        let ids = parse_models_dev(json, "opencode-go").unwrap();
        assert_eq!(ids, vec!["abc-model".to_string(), "kimi-k3".to_string()]);
    }

    #[test]
    fn parse_models_dev_missing_provider_errors() {
        let json = r#"{"other": {"models": {}}}"#;
        let err = parse_models_dev(json, "opencode-go").unwrap_err();
        assert!(err.contains("opencode-go"));
    }

    #[test]
    fn opencode_uses_its_live_gateway_catalogs() {
        assert_eq!(
            opencode_models_url("opencode-go").as_deref(),
            Some("https://opencode.ai/zen/go/v1/models")
        );
        assert_eq!(
            opencode_models_url("opencode").as_deref(),
            Some("https://opencode.ai/zen/v1/models")
        );
    }

    #[test]
    fn parse_openai_models_extracts_sorted_unique_ids() {
        let json = r#"{"data": [{"id": "gpt-4o"}, {"id": "gpt-4o"}, {"id": "o1"}]}"#;
        let ids = parse_openai_models(json).unwrap();
        assert_eq!(ids, vec!["gpt-4o".to_string(), "o1".to_string()]);
    }

    #[test]
    fn parse_openai_models_missing_data_errors() {
        let err = parse_openai_models("{}").unwrap_err();
        assert!(err.contains("data"));
    }

    #[test]
    fn parse_codex_models_filters_hidden_and_sorts_by_priority_then_slug() {
        let json = r#"{"models": [
            {"slug": "gpt-5.5", "priority": 2},
            {"slug": "codex-auto-review", "priority": 0, "visibility": "hide"},
            {"slug": "gpt-5.6-terra", "priority": 1},
            {"slug": "gpt-5.6-sol", "priority": 1},
            {"slug": "gpt-5.4", "visibility": "list"}
        ]}"#;
        let ids = parse_codex_models(json).unwrap();
        assert_eq!(
            ids,
            vec![
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-5.5".to_string(),
                "gpt-5.4".to_string(),
            ]
        );
    }

    #[test]
    fn parse_codex_models_tolerates_unknown_fields() {
        let json = r#"{"models": [
            {"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "priority": 1,
             "supported_reasoning_levels": [{"effort": "low"}], "some_future_field": true}
        ], "unrelated_top_level": 42}"#;
        let ids = parse_codex_models(json).unwrap();
        assert_eq!(ids, vec!["gpt-5.6-sol".to_string()]);
    }

    #[test]
    fn parse_codex_models_empty_list_yields_empty() {
        let ids = parse_codex_models(r#"{"models": []}"#).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn parse_codex_models_invalid_json_errors() {
        let err = parse_codex_models("not json").unwrap_err();
        assert!(err.contains("codex models"));
    }

    #[test]
    fn codex_context_window_uses_live_max_and_safety_percentage() {
        let json = r#"{"models": [
            {"slug": "gpt-5.4", "context_window": 272000,
             "max_context_window": 1000000,
             "effective_context_window_percent": 95}
        ]}"#;

        assert_eq!(parse_codex_context_window(json, "gpt-5.4"), Some(950_000));
        assert_eq!(parse_codex_context_window(json, "missing"), None);
    }

    #[test]
    fn models_dev_context_window_reads_provider_metadata() {
        let json = r#"{"anthropic":{"models":{"claude-opus-5":{"limit":{"context":256000}}}}}"#;
        assert_eq!(
            parse_models_dev_context_window(json, "anthropic", "claude-opus-5"),
            Some(256_000)
        );
    }

    #[test]
    fn parse_anthropic_models_extracts_ids_in_order() {
        let json = r#"{"data": [{"id": "claude-opus-5"}, {"id": "claude-sonnet-5"}]}"#;
        let ids = parse_anthropic_models(json).unwrap();
        assert_eq!(
            ids,
            vec!["claude-opus-5".to_string(), "claude-sonnet-5".to_string()]
        );
    }

    #[test]
    fn parse_anthropic_models_missing_data_errors() {
        let err = parse_anthropic_models("{}").unwrap_err();
        assert!(err.contains("data"));
    }

    #[test]
    fn anthropic_fallback_models_used_when_no_key() {
        let ids: Vec<String> = ANTHROPIC_FALLBACK_MODELS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(ids.contains(&"claude-sonnet-5".to_string()));
        assert!(ids.contains(&"claude-opus-5".to_string()));
        assert!(ids.contains(&"claude-fable-5".to_string()));
        assert!(ids.contains(&"claude-haiku-4-5-20251001".to_string()));
    }

    #[tokio::test]
    #[ignore = "requires network or local Anthropic credentials; run explicitly for catalog QA"]
    async fn list_models_anthropic_falls_back_without_key() {
        let ids = list_models("anthropic", None).await.unwrap();
        assert!(!ids.is_empty());
    }
}
