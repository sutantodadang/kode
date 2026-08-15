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

/// Lists candidate model ids for `provider`. `api_key_env`, when given,
/// names an environment variable to read the API key from (used for the
/// "openai" provider); it falls back to `OPENAI_API_KEY`/`KODE_API_KEY`.
pub async fn list_models(
    provider: &str,
    api_key_env: Option<String>,
) -> Result<Vec<String>, String> {
    match provider {
        // No list endpoint for the ChatGPT/Codex backend. These are
        // best-effort candidates verified to work on at least one account;
        // the backend rejects unknown model ids with a clear 400 at request
        // time, so a stale/wrong entry here is not silently harmful.
        "codex" => Ok(vec![
            "gpt-5.6-sol".to_string(),
            "gpt-5.6-codex".to_string(),
            "gpt-5.5-codex".to_string(),
            "codex-mini-latest".to_string(),
        ]),
        "opencode-go" | "opencode" | "kilo" => fetch_models_dev(provider).await,
        "lmstudio" => fetch_lmstudio().await,
        "openai" => fetch_openai(api_key_env).await,
        other => Err(format!("no model catalog for provider '{other}'")),
    }
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
    async fn list_models_codex_returns_builtin_candidates() {
        let ids = list_models("codex", None).await.unwrap();
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
}
