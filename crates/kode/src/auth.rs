//! `kode auth login|status|logout` — Kode's own credential store for the
//! `codex` and opencode-family (`opencode-go`, `opencode`, `kilo`,
//! `lmstudio`) providers. Writes to `~/.kode/auth/{codex,opencode}.json` in
//! the exact schemas `kode_model::codex` and `kode_model::opencode` already
//! parse. Never reads `~/.codex` or opencode's own data/auth directories.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CODEX_REDIRECT_URI_ENCODED: &str = "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback";
const CODEX_AUTHORIZE_BASE: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

const OPENCODE_PROVIDERS: [&str; 4] = ["opencode-go", "opencode", "kilo", "lmstudio"];

pub async fn login(provider: &str) -> anyhow::Result<()> {
    match provider {
        "codex" => login_codex().await,
        p if OPENCODE_PROVIDERS.contains(&p) => login_opencode_key(p).await,
        other => anyhow::bail!(
            "unsupported provider '{other}' (supported: codex, opencode-go, opencode, kilo, lmstudio)"
        ),
    }
}

pub async fn status() -> anyhow::Result<()> {
    let mut printed = false;

    if let Ok(path) = codex_auth_path()
        && path.exists()
    {
        printed = true;
        match kode_model::codex::load(&path) {
            Ok(auth) if !auth.account_id.is_empty() => {
                println!(
                    "codex: logged in (account \u{2026}{}, refreshed {})",
                    last4_chars(&auth.account_id),
                    auth.last_refresh
                );
            }
            _ => println!("codex: not logged in"),
        }
    }

    if let Ok(path) = opencode_auth_path() {
        let map = load_opencode_map(&path).unwrap_or_default();
        let mut ids: Vec<&String> = map.keys().collect();
        ids.sort();
        for id in ids {
            printed = true;
            let len = map[id.as_str()]
                .get("key")
                .and_then(|k| k.as_str())
                .map(|k| k.len())
                .unwrap_or(0);
            println!("{id}: key ({len} chars)");
        }
    }

    if !printed {
        println!("no credentials stored \u{2014} run: kode auth login <provider>");
    }
    Ok(())
}

pub async fn logout(provider: &str) -> anyhow::Result<()> {
    match provider {
        "codex" => {
            let path = codex_auth_path()?;
            if path.exists() {
                std::fs::remove_file(&path)?;
                println!("codex: logged out");
            } else {
                println!("nothing to remove");
            }
        }
        p if OPENCODE_PROVIDERS.contains(&p) => {
            let path = opencode_auth_path()?;
            let removed = remove_opencode_key(&path, p)?;
            if removed {
                println!("{p}: logged out");
            } else {
                println!("nothing to remove");
            }
        }
        other => anyhow::bail!(
            "unsupported provider '{other}' (supported: codex, opencode-go, opencode, kilo, lmstudio)"
        ),
    }
    Ok(())
}

// --- codex OAuth (PKCE, authorization-code) --------------------------------

async fn login_codex() -> anyhow::Result<()> {
    let verifier = random_hex(64, 0x5A17_C0DE_0001);
    let state = random_hex(32, 0x5A17_C0DE_0002);
    let challenge = base64url_nopad(&sha256(verifier.as_bytes()));
    let url = build_authorize_url(&challenge, &state);

    let listener = TcpListener::bind("127.0.0.1:1455").await.map_err(|_| {
        anyhow::anyhow!("port 1455 in use \u{2014} close the other login and retry")
    })?;

    open_browser(&url);

    let code = wait_for_callback(listener, &state).await?;
    let (id_token, access_token, refresh_token) = exchange_code(&code, &verifier).await?;
    let account_id = decode_jwt_account_id(&id_token).ok_or_else(|| {
        anyhow::anyhow!("no ChatGPT account in token \u{2014} is your plan active?")
    })?;

    let path = codex_auth_path()?;
    write_codex_auth(&path, &id_token, &access_token, &refresh_token, &account_id)?;

    println!(
        "codex: logged in (account \u{2026}{})",
        last4_chars(&account_id)
    );
    Ok(())
}

/// Builds the codex OAuth authorize URL with the given PKCE challenge and
/// CSRF state.
fn build_authorize_url(challenge: &str, state: &str) -> String {
    format!(
        "{CODEX_AUTHORIZE_BASE}?response_type=code&client_id={CODEX_CLIENT_ID}&redirect_uri={CODEX_REDIRECT_URI_ENCODED}&scope=openid%20profile%20email%20offline_access&code_challenge={challenge}&code_challenge_method=S256&state={state}&id_token_add_organizations=true&codex_cli_simplified_flow=true"
    )
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        // `cmd /C start "" <url>` re-parses the whole line and treats `&` in
        // the query string as a command separator, breaking multi-param
        // URLs. `explorer.exe <url>` takes the URL as a single argv element
        // (no shell re-parsing) and opens it in the default browser;
        // explorer's exit code is unreliable so a spawn success is enough.
        let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    println!("open this url if the browser didn't launch:\n{url}");
}

/// Accepts connections on `listener` until a valid `/auth/callback` request
/// arrives (returns the authorization code), a bad/mismatched callback
/// arrives (returns an error immediately \u{2014} no hanging), or 300s elapse.
/// Requests to any other path get a 404 and the loop keeps waiting.
async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> anyhow::Result<String> {
    let result = tokio::time::timeout(CALLBACK_TIMEOUT, async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let request = read_request_head(&mut stream).await.unwrap_or_default();
            let Some(line) = request.lines().next() else {
                respond(&mut stream, 400, "bad request").await;
                continue;
            };
            let Some((path, query)) = parse_request_line(line) else {
                respond(&mut stream, 400, "bad request").await;
                continue;
            };
            if !path.starts_with("/auth/callback") {
                respond(&mut stream, 404, "not found").await;
                continue;
            }

            let params = parse_query_params(query);
            match validate_callback(&params, expected_state) {
                Ok(code) => {
                    respond(
                        &mut stream,
                        200,
                        "Kode login complete \u{2014} you can close this tab.",
                    )
                    .await;
                    return Ok(code);
                }
                Err(msg) => {
                    respond(&mut stream, 400, msg).await;
                    return Err(anyhow::anyhow!("codex login callback error: {msg}"));
                }
            }
        }
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!("login timed out after 300s waiting for browser callback"),
    }
}

async fn read_request_head(stream: &mut tokio::net::TcpStream) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn respond(stream: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let html = format!("<html><body>{body}</body></html>");
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn exchange_code(code: &str, verifier: &str) -> anyhow::Result<(String, String, String)> {
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        id_token: String,
        access_token: String,
        refresh_token: String,
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": CODEX_REDIRECT_URI,
        "client_id": CODEX_CLIENT_ID,
        "code_verifier": verifier,
    });
    let resp = client.post(CODEX_TOKEN_URL).json(&body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(500).collect();
        anyhow::bail!("token exchange failed ({status}): {snippet}");
    }
    let wire: TokenResponse = resp.json().await?;
    Ok((wire.id_token, wire.access_token, wire.refresh_token))
}

fn write_codex_auth(
    path: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
    account_id: &str,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        // ponytail: plaintext at rest like codex/opencode; OS file ACLs only.
        std::fs::create_dir_all(parent)?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last_refresh = kode_model::codex::format_rfc3339_secs(now);
    let value = serde_json::json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": refresh_token,
            "account_id": account_id,
        },
        "last_refresh": last_refresh,
    });
    let pretty = serde_json::to_string_pretty(&value)?;
    std::fs::write(path, pretty)?;
    Ok(())
}

// --- opencode-family api-key login ------------------------------------------

async fn login_opencode_key(provider: &str) -> anyhow::Result<()> {
    println!("create an api key at https://opencode.ai (or your gateway's console) and paste it:");
    let key = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    })
    .await??;
    let key = key.trim().to_string();
    if key.is_empty() {
        anyhow::bail!("no key entered");
    }
    if key.len() < 8 {
        anyhow::bail!("key looks too short \u{2014} paste the full key");
    }

    let path = opencode_auth_path()?;
    upsert_opencode_key(&path, provider, &key)?;
    println!("{provider}: key saved");
    Ok(())
}

fn load_opencode_map(path: &Path) -> anyhow::Result<serde_json::Map<String, Value>> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let content = std::fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&content)?;
    Ok(value.as_object().cloned().unwrap_or_default())
}

fn save_opencode_map(path: &Path, map: &serde_json::Map<String, Value>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pretty = serde_json::to_string_pretty(&Value::Object(map.clone()))?;
    std::fs::write(path, pretty)?;
    Ok(())
}

fn upsert_opencode_key(path: &Path, provider_id: &str, key: &str) -> anyhow::Result<()> {
    let mut map = load_opencode_map(path)?;
    map.insert(
        provider_id.to_string(),
        serde_json::json!({"type": "api", "key": key}),
    );
    save_opencode_map(path, &map)
}

fn remove_opencode_key(path: &Path, provider_id: &str) -> anyhow::Result<bool> {
    let mut map = load_opencode_map(path)?;
    let removed = map.remove(provider_id).is_some();
    if removed {
        if map.is_empty() {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        } else {
            save_opencode_map(path, &map)?;
        }
    }
    Ok(removed)
}

// --- store paths -------------------------------------------------------------

fn codex_auth_path() -> anyhow::Result<PathBuf> {
    kode_model::codex::default_auth_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory for kode auth store"))
}

fn opencode_auth_path() -> anyhow::Result<PathBuf> {
    kode_model::opencode::default_auth_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory for kode auth store"))
}

// --- small self-contained helpers (no new deps) -----------------------------

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Generates a pseudo-random lowercase-hex string of `len` hex characters
/// (`len` capped at 64) by hashing a mix of the current time, process id,
/// and a stack address, salted with `salt`.
// ponytail: not a CSPRNG; PKCE verifier secrecy window is seconds and S256
// protects the channel; swap to getrandom if ever needed.
fn random_hex(len: usize, salt: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let local = 0u8;
    let addr = std::ptr::addr_of!(local) as usize as u128;

    let mut hasher_input = Vec::with_capacity(64);
    hasher_input.extend_from_slice(&nanos.to_le_bytes());
    hasher_input.extend_from_slice(&pid.to_le_bytes());
    hasher_input.extend_from_slice(&addr.to_le_bytes());
    hasher_input.extend_from_slice(&salt.to_le_bytes());
    let digest = sha256(&hasher_input);

    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    hex.chars().take(len.min(64)).collect()
}

/// Base64url (RFC 4648 §5) encoding, no padding.
fn base64url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// Base64url (RFC 4648 §5) decoding; tolerant of missing padding, rejects
/// non-alphabet bytes.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for b in s.bytes().filter(|&b| b != b'=') {
        let v = val(b)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Percent-decodes only `%XX` escapes (used for the callback's `code` query
/// param); everything else passes through unchanged.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
}

/// Parses an HTTP request line (`"GET /path?query HTTP/1.1"`) into
/// `(path, query)`. Returns `None` for anything not a well-formed `GET`
/// request line.
fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.trim_end().splitn(3, ' ');
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    let target = parts.next()?;
    Some(target.split_once('?').unwrap_or((target, "")))
}

fn parse_query_params(query: &str) -> CallbackParams {
    let mut params = CallbackParams::default();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => params.code = Some(percent_decode(v)),
                "state" => params.state = Some(percent_decode(v)),
                _ => {}
            }
        }
    }
    params
}

/// Validates a parsed callback: requires both `code` and `state`, and that
/// `state` matches `expected_state` (CSRF check). Returns the code on
/// success.
fn validate_callback(
    params: &CallbackParams,
    expected_state: &str,
) -> Result<String, &'static str> {
    let code = params.code.as_deref().ok_or("missing code")?;
    let state = params.state.as_deref().ok_or("missing state")?;
    if state != expected_state {
        return Err("state mismatch");
    }
    Ok(code.to_string())
}

/// Decodes an unsigned-inspection of a JWT's payload segment (no signature
/// verification \u{2014} that's the OAuth server's job) to pull
/// `["https://api.openai.com/auth"]["chatgpt_account_id"]`.
fn decode_jwt_account_id(id_token: &str) -> Option<String> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_bytes = base64url_decode(parts[1])?;
    let value: Value = serde_json::from_slice(&payload_bytes).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(|s| s.to_string())
}

fn last4_chars(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("kode-auth-test-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn base64url_nopad_matches_rfc7636_vector() {
        // RFC 7636 appendix B example verifier/challenge pair.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = sha256(verifier.as_bytes());
        let challenge = base64url_nopad(&digest);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn base64url_roundtrips_through_decode() {
        let bytes = b"hello world, this is a test payload!";
        let encoded = base64url_nopad(bytes);
        let decoded = base64url_decode(&encoded).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn percent_decode_handles_encoded_and_plain() {
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("100%25done"), "100%done");
    }

    #[test]
    fn parse_request_line_extracts_path_and_query() {
        let line = "GET /auth/callback?code=abc123&state=xyz789 HTTP/1.1";
        let (path, query) = parse_request_line(line).unwrap();
        assert_eq!(path, "/auth/callback");
        let params = parse_query_params(query);
        assert_eq!(params.code.as_deref(), Some("abc123"));
        assert_eq!(params.state.as_deref(), Some("xyz789"));
    }

    #[test]
    fn parse_request_line_rejects_non_get() {
        assert!(parse_request_line("POST /auth/callback HTTP/1.1").is_none());
    }

    #[test]
    fn validate_callback_accepts_matching_state() {
        let params = parse_query_params("code=abc&state=expected");
        assert_eq!(validate_callback(&params, "expected").unwrap(), "abc");
    }

    #[test]
    fn validate_callback_rejects_state_mismatch() {
        let params = parse_query_params("code=abc&state=wrong");
        let err = validate_callback(&params, "expected").unwrap_err();
        assert_eq!(err, "state mismatch");
    }

    #[test]
    fn validate_callback_rejects_missing_code() {
        let params = parse_query_params("state=expected");
        let err = validate_callback(&params, "expected").unwrap_err();
        assert_eq!(err, "missing code");
    }

    #[test]
    fn build_authorize_url_contains_required_params() {
        let url = build_authorize_url("chal-abc", "state-xyz");
        assert!(url.starts_with(CODEX_AUTHORIZE_BASE));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={CODEX_CLIENT_ID}")));
        assert!(url.contains(CODEX_REDIRECT_URI_ENCODED));
        assert!(url.contains("code_challenge=chal-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-xyz"));
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
    }

    #[test]
    fn decode_jwt_account_id_extracts_claim() {
        let header = base64url_nopad(br#"{"alg":"none"}"#);
        let payload = base64url_nopad(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-test-1234"}}"#,
        );
        let token = format!("{header}.{payload}.sig");
        assert_eq!(
            decode_jwt_account_id(&token).as_deref(),
            Some("acct-test-1234")
        );
    }

    #[test]
    fn decode_jwt_account_id_missing_claim_is_none() {
        let header = base64url_nopad(br#"{"alg":"none"}"#);
        let payload = base64url_nopad(br#"{"sub":"someone"}"#);
        let token = format!("{header}.{payload}.sig");
        assert!(decode_jwt_account_id(&token).is_none());
    }

    #[test]
    fn decode_jwt_account_id_malformed_token_is_none() {
        assert!(decode_jwt_account_id("not-a-jwt").is_none());
    }

    #[test]
    fn random_hex_is_lowercase_hex_of_requested_length() {
        let h = random_hex(64, 1);
        assert_eq!(h.len(), 64);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        let h2 = random_hex(32, 2);
        assert_eq!(h2.len(), 32);
    }

    #[test]
    fn opencode_store_upsert_and_logout_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("opencode.json");

        upsert_opencode_key(&path, "opencode-go", "sk-test-key-12345678").unwrap();
        let map = load_opencode_map(&path).unwrap();
        assert_eq!(
            map["opencode-go"]["key"],
            serde_json::json!("sk-test-key-12345678")
        );
        assert_eq!(map["opencode-go"]["type"], serde_json::json!("api"));

        let removed = remove_opencode_key(&path, "opencode-go").unwrap();
        assert!(removed);
        assert!(!path.exists());
    }

    #[test]
    fn opencode_store_keeps_other_entries_on_removal() {
        let dir = temp_dir();
        let path = dir.join("opencode.json");

        upsert_opencode_key(&path, "kilo", "sk-key-aaaaaaaa").unwrap();
        upsert_opencode_key(&path, "opencode", "sk-key-bbbbbbbb").unwrap();

        let removed = remove_opencode_key(&path, "kilo").unwrap();
        assert!(removed);
        assert!(path.exists());
        let map = load_opencode_map(&path).unwrap();
        assert!(!map.contains_key("kilo"));
        assert!(map.contains_key("opencode"));
    }

    #[test]
    fn opencode_store_removal_of_missing_key_is_false() {
        let dir = temp_dir();
        let path = dir.join("opencode.json");
        let removed = remove_opencode_key(&path, "kilo").unwrap();
        assert!(!removed);
    }
}
