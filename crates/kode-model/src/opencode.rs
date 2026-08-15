//! Talks to opencode-family OpenAI-compatible gateways (`opencode-go`,
//! `opencode`, `kilo`, `lmstudio`) using API keys stored in Kode's own
//! credential store (`~/.kode/auth/opencode.json`, written by `kode auth
//! login <provider>`), via the existing [`OpenAiModel`].

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{ModelError, Result};
use crate::openai::{OpenAiModel, OpenAiOptions};

/// Default location of Kode's own opencode-family auth store:
/// `~/.kode/auth/opencode.json`.
pub fn default_auth_path() -> Option<PathBuf> {
    Some(kode_core::auth_dir()?.join("opencode.json"))
}

/// Default location of opencode's user config directory:
/// `$USERPROFILE/.config/opencode` (or `$HOME/...` elsewhere).
pub fn default_config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".config").join("opencode"))
}

fn builtin_base_url(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "opencode-go" => Some("https://opencode.ai/zen/go/v1"),
        "opencode" => Some("https://opencode.ai/zen/v1"),
        "kilo" => Some("https://api.kilo.ai/api/gateway"),
        "lmstudio" => Some("http://127.0.0.1:1234/v1"),
        _ => None,
    }
}

/// Resolves `provider_id`'s stored API key and gateway base URL, and
/// constructs an [`OpenAiModel`] pointed at it. `config_dir`, when given, is
/// checked for a `provider.<id>.options.baseURL` override in
/// `opencode.json`/`opencode.jsonc` before falling back to the builtin
/// gateway table.
pub fn resolve(
    provider_id: &str,
    model: String,
    auth_path: &Path,
    config_dir: Option<&Path>,
) -> Result<OpenAiModel> {
    let key = load_api_key(provider_id, auth_path)?;
    let base_url = resolve_base_url(provider_id, config_dir)?;

    Ok(OpenAiModel::new(OpenAiOptions {
        base_url,
        api_key: key,
        model,
    }))
}

fn load_api_key(provider_id: &str, auth_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(auth_path).map_err(|e| ModelError::Api {
        status: 0,
        message: format!(
            "cannot read opencode auth file {}: {e} — run: kode auth login {provider_id}",
            auth_path.display()
        ),
    })?;
    let value: Value = serde_json::from_str(&content).map_err(|e| ModelError::Api {
        status: 0,
        message: format!("invalid opencode auth.json ({}): {e}", auth_path.display()),
    })?;
    let obj = value.as_object().ok_or_else(|| ModelError::Api {
        status: 0,
        message: "invalid opencode auth.json: expected a JSON object".to_string(),
    })?;

    let entry = obj.get(provider_id).ok_or_else(|| {
        let mut ids: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        ids.sort_unstable();
        ModelError::Api {
            status: 0,
            message: format!(
                "opencode provider '{provider_id}' not found in auth.json (available: [{}]) — run: kode auth login {provider_id}",
                ids.join(", ")
            ),
        }
    })?;

    let entry_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if entry_type != "api" {
        return Err(ModelError::Api {
            status: 0,
            message: format!(
                "opencode provider '{provider_id}' uses '{entry_type}' auth — not supported yet (only 'api' keys)"
            ),
        });
    }

    entry
        .get("key")
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(|k| k.to_string())
        .ok_or_else(|| ModelError::Api {
            status: 0,
            message: format!("opencode provider '{provider_id}' has no api key in auth.json"),
        })
}

fn resolve_base_url(provider_id: &str, config_dir: Option<&Path>) -> Result<String> {
    if let Some(dir) = config_dir {
        for name in ["opencode.json", "opencode.jsonc"] {
            let path = dir.join(name);
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let stripped = strip_jsonc_comments(&raw);
            let Ok(value) = serde_json::from_str::<Value>(&stripped) else {
                continue;
            };
            if let Some(url) = value
                .get("provider")
                .and_then(|p| p.get(provider_id))
                .and_then(|p| p.get("options"))
                .and_then(|o| o.get("baseURL"))
                .and_then(|u| u.as_str())
            {
                return Ok(url.to_string());
            }
        }
    }

    builtin_base_url(provider_id)
        .map(|s| s.to_string())
        .ok_or_else(|| ModelError::Api {
            status: 0,
            message: format!("unknown opencode provider '{provider_id}'"),
        })
}

/// Naive but string-aware stripper for `//` line comments in JSONC. Tracks
/// whether we're inside a JSON string (respecting `\"` escapes) so `//`
/// occurring inside a string value (e.g. `"https://host/v1"`) is not
/// mistaken for a comment start.
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }

        if c == '/' && chars.peek() == Some(&'/') {
            chars.next(); // consume the second '/'
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }

        out.push(c);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Model as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-opencode-test-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_auth(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("auth.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn strip_jsonc_comments_preserves_urls_in_strings() {
        let input = r#"{"baseURL": "https://x/v1" // note
}"#;
        let out = strip_jsonc_comments(input);
        assert!(out.contains("\"https://x/v1\""));
        assert!(!out.contains("note"));
        // Still valid JSON after stripping.
        let _: Value = serde_json::from_str(&out).unwrap();
    }

    #[test]
    fn strip_jsonc_comments_leaves_plain_json_untouched_by_parse() {
        let input = r#"{"a": 1, "b": "text // not a comment"}"#;
        let out = strip_jsonc_comments(input);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["b"], serde_json::json!("text // not a comment"));
    }

    #[test]
    fn resolve_happy_path_uses_builtin_base_url() {
        let dir = temp_dir();
        let auth_path = write_auth(
            &dir,
            r#"{"opencode-go": {"type": "api", "key": "sk-secret"}}"#,
        );

        let model = resolve("opencode-go", "kimi-k3".to_string(), &auth_path, None).unwrap();
        assert_eq!(model.capabilities().id, "kimi-k3");
    }

    #[test]
    fn resolve_override_precedence_wins_over_builtin() {
        let dir = temp_dir();
        let auth_path = write_auth(&dir, r#"{"kilo": {"type": "api", "key": "sk-secret"}}"#);
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.join("opencode.json"),
            r#"{"provider": {"kilo": {"options": {"baseURL": "https://override.example/v1"}}}}"#,
        )
        .unwrap();

        // We can't inspect OpenAiModel's private base_url field directly, so
        // verify resolve_base_url (the pure helper) picks the override.
        let url = resolve_base_url("kilo", Some(&config_dir)).unwrap();
        assert_eq!(url, "https://override.example/v1");

        // And resolve() itself succeeds end-to-end with the override dir.
        let model = resolve("kilo", "model-x".to_string(), &auth_path, Some(&config_dir)).unwrap();
        assert_eq!(model.capabilities().id, "model-x");
    }

    #[test]
    fn resolve_override_precedence_jsonc_with_comment() {
        let config_dir = temp_dir();
        std::fs::write(
            config_dir.join("opencode.jsonc"),
            "{\n  // comment before\n  \"provider\": { \"lmstudio\": { \"options\": { \"baseURL\": \"https://jsonc.example/v1\" // trailing\n } } }\n}\n",
        )
        .unwrap();

        let url = resolve_base_url("lmstudio", Some(&config_dir)).unwrap();
        assert_eq!(url, "https://jsonc.example/v1");
    }

    #[test]
    fn resolve_missing_provider_error_does_not_contain_key() {
        let dir = temp_dir();
        let auth_path = write_auth(
            &dir,
            r#"{"kilo": {"type": "api", "key": "super-secret-value"}}"#,
        );

        let err = resolve("opencode-go", "m".to_string(), &auth_path, None).unwrap_err();
        let message = err.to_string();
        assert!(!message.contains("super-secret-value"));
        assert!(message.contains("opencode-go"));
    }

    #[test]
    fn resolve_oauth_type_is_rejected() {
        let dir = temp_dir();
        let auth_path = write_auth(&dir, r#"{"opencode": {"type": "oauth"}}"#);

        let err = resolve("opencode", "m".to_string(), &auth_path, None).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("oauth"));
    }

    #[test]
    fn resolve_unknown_provider_no_override_errors() {
        let dir = temp_dir();
        let auth_path = write_auth(&dir, r#"{"mystery": {"type": "api", "key": "sk-secret"}}"#);

        let err = resolve("mystery", "m".to_string(), &auth_path, None).unwrap_err();
        assert!(err.to_string().contains("unknown opencode provider"));
    }

    #[test]
    fn resolve_missing_key_errors() {
        let dir = temp_dir();
        let auth_path = write_auth(&dir, r#"{"kilo": {"type": "api"}}"#);

        let err = resolve("kilo", "m".to_string(), &auth_path, None).unwrap_err();
        assert!(err.to_string().contains("no api key"));
    }
}
