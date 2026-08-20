//! Best-effort model catalogs per provider, for the interactive model picker
//! and `kode models`. There is no universal "list models" endpoint, so each
//! provider is handled on its own terms; failures are returned as `Err`
//! strings rather than propagated as hard errors — callers keep working with
//! free-text model entry when a catalog can't be fetched.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::Value;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const LMSTUDIO_URL: &str = "http://127.0.0.1:1234/v1/models";
const OPENAI_MODELS_URL: &str = "https://api.openai.com/v1/models";
const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Static candidates used when no Anthropic API key is available (OAuth-only
/// auth) or the live `/v1/models` fetch fails.
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
const CODEX_CLIENT_VERSION: &str = "0.147.0";

/// Static candidates used when the live codex model fetch fails or returns
/// nothing usable (no auth, network error, parse error, empty list).
const CODEX_FALLBACK_MODELS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-codex",
    "gpt-5.5-codex",
    "codex-mini-latest",
];

/// Lists candidate model ids for `provider`. `api_key_env`, when given,
/// names an environment variable to read the API key from (used for the
/// "openai" provider); it falls back to `OPENAI_API_KEY`/`KODE_API_KEY`.
pub async fn list_models(
    provider: &str,
    api_key_env: Option<String>,
) -> Result<Vec<String>, String> {
    match provider {
        "codex" => Ok(fetch_codex_models().await.unwrap_or_else(|_| {
            CODEX_FALLBACK_MODELS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })),
        "opencode-go" | "opencode" | "kilo" => fetch_models_dev(provider).await,
        "lmstudio" => fetch_lmstudio().await,
        "openai" => fetch_openai(api_key_env).await,
        "anthropic" => Ok(fetch_anthropic_models().await.unwrap_or_else(|_| {
            ANTHROPIC_FALLBACK_MODELS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })),
        "antigravity" => Ok(fetch_antigravity_models().await.unwrap_or_else(|_| {
            ANTIGRAVITY_FALLBACK_MODELS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })),
        other => Err(format!("no model catalog for provider '{other}'")),
    }
}

/// Resolves an Anthropic API key: `ANTHROPIC_API_KEY` from the environment,
/// else an `"api"`-type key from Kode's own anthropic auth store. Returns
/// `None` for OAuth-only auth (or no auth at all) — callers fall back to
/// [`ANTHROPIC_FALLBACK_MODELS`] in that case.
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

fn anthropic_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    let path = crate::anthropic::default_auth_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    if value.get("type").and_then(|t| t.as_str()) != Some("api") {
        return None;
    }
    value
        .get("key")
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
}

/// Fetches the live Anthropic model list from `GET /v1/models`. Requires an
/// API key (env or Kode's own auth store) — OAuth-only auth has no
/// equivalent endpoint, so callers fall back to
/// [`ANTHROPIC_FALLBACK_MODELS`] when this errors.
async fn fetch_anthropic_models() -> Result<Vec<String>, String> {
    let key = anthropic_api_key().ok_or_else(|| "no anthropic api key found".to_string())?;
    let client = reqwest::Client::new();
    let resp = client
        .get(ANTHROPIC_MODELS_URL)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .timeout(FETCH_TIMEOUT)
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
    let ids = parse_anthropic_models(&text)?;
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
    let auth_path =
        crate::codex::default_auth_path().ok_or_else(|| "no codex auth path".to_string())?;
    let auth = crate::codex::load_fresh(&auth_path)
        .await
        .map_err(|e| e.to_string())?;

    let url = format!("{CODEX_MODELS_URL}?client_version={CODEX_CLIENT_VERSION}");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
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
    let text = resp
        .text()
        .await
        .map_err(|e| format!("codex models read failed: {e}"))?;
    let models = parse_codex_models(&text)?;
    if models.is_empty() {
        return Err("codex models list empty".to_string());
    }
    Ok(models)
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

async fn fetch_models_dev(provider: &str) -> Result<Vec<String>, String> {
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
    let text = resp
        .text()
        .await
        .map_err(|e| format!("models.dev read failed: {e}"))?;
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
    let client = reqwest::Client::new();
    let resp = client
        .get(LMSTUDIO_URL)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("lmstudio fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("lmstudio returned {}", resp.status()));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("lmstudio read failed: {e}"))?;
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

    #[tokio::test]
    async fn list_models_codex_returns_nonempty_list() {
        // May hit the live codex backend if this machine has codex auth
        // configured, or fall back to CODEX_FALLBACK_MODELS otherwise —
        // "gpt-5.6-sol" is present in both, so it's a safe assertion either
        // way. `list_models` never errors for "codex".
        let ids = list_models("codex", None).await.unwrap();
        assert!(ids.contains(&"gpt-5.6-sol".to_string()));
    }

    #[test]
    fn codex_fallback_models_used_when_fetch_fails() {
        let ids: Vec<String> = CODEX_FALLBACK_MODELS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(ids.contains(&"gpt-5.6-sol".to_string()));
        assert!(ids.contains(&"codex-mini-latest".to_string()));
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
    async fn list_models_anthropic_falls_back_without_key() {
        {
            let _guard = crate::test_support::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // SAFETY: test-only; ensures a clean slate for this assertion,
            // serialized via ENV_LOCK. Released before the `.await` below —
            // clippy (rightly) flags a std Mutex guard held across an await.
            unsafe {
                std::env::remove_var("ANTHROPIC_API_KEY");
            }
        }
        let ids = list_models("anthropic", None).await.unwrap();
        // Never errors for "anthropic" — falls back to the static list when
        // no key/network is available.
        assert!(!ids.is_empty());
    }
}
