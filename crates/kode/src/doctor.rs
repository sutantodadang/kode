use std::path::Path;
use std::time::Duration;

use kode_core::config::{KodeConfig, McpServerConfig, ModelConfig, ZindeksConfig};
use kode_intel::{CodeIntelligence, ZindeksAdapter};
use kode_memory::{EngineeringMemory, IngatAdapter};

const TIMEOUT: Duration = Duration::from_secs(5);
const NAME_WIDTH: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    fn symbol(self) -> &'static str {
        match self {
            CheckStatus::Pass => "\u{2713}", // ✓
            CheckStatus::Warn => "!",
            CheckStatus::Fail => "\u{2717}", // ✗
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub section: &'static str,
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    fn pass(section: &'static str, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(
        section: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        let fix = fix.into();
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Warn,
            detail: detail.into(),
            fix: if fix.is_empty() { None } else { Some(fix) },
        }
    }

    fn fail(
        section: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        let fix = fix.into();
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
            fix: if fix.is_empty() { None } else { Some(fix) },
        }
    }
}

/// Runs `kode doctor`: a sectioned diagnostic sweep over config, LLM
/// settings, zindeks, Ingat, git, and the working environment. Prints a
/// human-readable report; returns `Ok` when no check `Fail`ed, and bails
/// (after printing) otherwise.
pub async fn run(cwd: &Path) -> anyhow::Result<()> {
    let mut checks: Vec<Check> = Vec::new();

    let config_path = KodeConfig::config_path(cwd);
    let config_exists = config_path.is_file();
    let load_result: Result<KodeConfig, String> = KodeConfig::load(cwd).map_err(|e| e.to_string());
    checks.push(config_check(&load_result, config_exists));
    let config = load_result.clone().unwrap_or_default();

    checks.push(provider_check(&config.model));
    checks.push(model_check(&config.model));
    match config.model.provider.as_str() {
        "openai" => {
            checks.push(api_key_check(
                env_present("OPENAI_API_KEY"),
                env_present("KODE_API_KEY"),
            ));
        }
        "codex" => {
            checks.push(codex_check(&codex_auth_result()));
        }
        "opencode-go" | "opencode" | "kilo" | "lmstudio" => {
            checks.push(opencode_check(
                &config.model.provider,
                &opencode_auth_result(&config.model.provider),
            ));
        }
        "anthropic" => {
            checks.push(anthropic_check(&anthropic_auth_result()));
        }
        "antigravity" => {
            checks.push(antigravity_check(&antigravity_auth_result()));
        }
        _ => {}
    }

    if config.zindeks.enabled {
        collect_zindeks_checks(&mut checks, cwd, &config).await;
    } else {
        checks.push(Check::warn("Zindeks", "zindeks", "disabled in config", ""));
    }

    if config.ingat.enabled {
        collect_ingat_checks(&mut checks, &config).await;
    } else {
        checks.push(Check::warn("Ingat", "ingat", "disabled in config", ""));
    }

    collect_mcp_checks(&mut checks, &config).await;

    checks.push(git_binary_check().await);
    checks.push(git_repository_check(cwd));

    checks.push(environment_writable_check(cwd));

    let report = render(&checks);
    println!("{report}");

    let problems = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .count();
    if problems == 0 {
        Ok(())
    } else {
        anyhow::bail!("{problems} problem(s) found")
    }
}

fn env_present(key: &str) -> bool {
    std::env::var(key).map(|v| !v.is_empty()).unwrap_or(false)
}

// --- pure check helpers (unit-testable, no I/O) ---------------------------

fn config_check(load: &Result<KodeConfig, String>, exists: bool) -> Check {
    match load {
        Ok(_) if exists => Check::pass("Config", ".kode/config.toml", "loaded"),
        Ok(_) => Check::warn(
            "Config",
            ".kode/config.toml",
            "using defaults",
            "create .kode/config.toml",
        ),
        Err(err) => Check::fail(
            "Config",
            ".kode/config.toml",
            err.clone(),
            "fix the TOML syntax",
        ),
    }
}

const SUPPORTED_PROVIDERS: [&str; 7] = [
    "openai",
    "anthropic",
    "codex",
    "opencode-go",
    "opencode",
    "kilo",
    "lmstudio",
];

fn provider_check(model: &ModelConfig) -> Check {
    if SUPPORTED_PROVIDERS.contains(&model.provider.as_str()) {
        Check::pass("LLM", "provider", model.provider.clone())
    } else {
        Check::fail(
            "LLM",
            "provider",
            format!("provider={}", model.provider),
            format!(
                "set [model] provider to one of: {}",
                SUPPORTED_PROVIDERS.join(", ")
            ),
        )
    }
}

/// Loads codex auth via the default path, collapsing errors to `String` so
/// the pass/fail mapping (`codex_check`) stays a pure, unit-testable
/// function independent of file I/O.
fn codex_auth_result() -> Result<kode_model::CodexAuth, String> {
    let path = kode_model::codex::default_auth_path()
        .ok_or_else(|| "cannot resolve home directory".to_string())?;
    kode_model::codex::load(&path).map_err(|e| e.to_string())
}

fn codex_check(result: &Result<kode_model::CodexAuth, String>) -> Check {
    match result {
        Ok(auth) if auth.auth_mode == "apikey" => {
            if auth.api_key.is_some() {
                Check::pass("LLM", "codex auth", "apikey mode, key present")
            } else {
                Check::fail(
                    "LLM",
                    "codex auth",
                    "apikey mode but no OPENAI_API_KEY in auth.json",
                    "run: kode auth login codex",
                )
            }
        }
        Ok(auth) => Check::pass(
            "LLM",
            "codex auth",
            format!(
                "chatgpt auth (account ...{})",
                last4_chars(&auth.account_id)
            ),
        ),
        Err(err) => Check::fail(
            "LLM",
            "codex auth",
            err.clone(),
            "run: kode auth login codex",
        ),
    }
}

/// Last 4 characters only — never surface a full account id/token in
/// diagnostic output.
fn last4_chars(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// Checks whether `provider_id` has an `"api"`-type key in opencode's
/// auth.json, collapsing errors to `String` for the same reason as
/// `codex_auth_result`.
fn opencode_auth_result(provider_id: &str) -> Result<bool, String> {
    let path = kode_model::opencode::default_auth_path()
        .ok_or_else(|| "cannot resolve home directory".to_string())?;
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    let entry = value.get(provider_id);
    let is_api = entry.and_then(|e| e.get("type")).and_then(|t| t.as_str()) == Some("api");
    let has_key = entry
        .and_then(|e| e.get("key"))
        .and_then(|k| k.as_str())
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    Ok(is_api && has_key)
}

fn opencode_check(provider_id: &str, result: &Result<bool, String>) -> Check {
    match result {
        Ok(true) => Check::pass("LLM", "opencode auth", format!("key found ({provider_id})")),
        Ok(false) => Check::fail(
            "LLM",
            "opencode auth",
            format!("no api key for '{provider_id}'"),
            format!("run: kode auth login {provider_id}"),
        ),
        Err(err) => Check::fail(
            "LLM",
            "opencode auth",
            err.clone(),
            format!("run: kode auth login {provider_id}"),
        ),
    }
}

/// Loads anthropic auth via the default path (falling back to
/// `ANTHROPIC_API_KEY` when no auth file exists, same as
/// [`kode_model::anthropic::load`]), collapsing errors to `String` for the
/// same reason as `codex_auth_result`.
fn anthropic_auth_result() -> Result<kode_model::AnthropicAuth, String> {
    let path = kode_model::anthropic::default_auth_path()
        .ok_or_else(|| "cannot resolve home directory".to_string())?;
    kode_model::anthropic::load(&path).map_err(|e| e.to_string())
}

fn anthropic_check(result: &Result<kode_model::AnthropicAuth, String>) -> Check {
    match result {
        Ok(kode_model::AnthropicAuth::ApiKey(_)) => {
            Check::pass("LLM", "anthropic auth", "api key present")
        }
        Ok(kode_model::AnthropicAuth::OAuth { .. }) => {
            Check::pass("LLM", "anthropic auth", "oauth token present")
        }
        Err(err) => Check::fail(
            "LLM",
            "anthropic auth",
            err.clone(),
            "run: kode auth login anthropic (or set ANTHROPIC_API_KEY)",
        ),
    }
}

fn antigravity_auth_result() -> Result<kode_model::AntigravityAuth, String> {
    let path = kode_model::antigravity::default_auth_path()
        .ok_or_else(|| "cannot resolve home directory".to_string())?;
    kode_model::antigravity::load(&path).map_err(|e| e.to_string())
}

fn antigravity_check(result: &Result<kode_model::AntigravityAuth, String>) -> Check {
    match result {
        Ok(_) => Check::pass("LLM", "antigravity auth", "oauth token present"),
        Err(err) => Check::fail(
            "LLM",
            "antigravity auth",
            err.clone(),
            "run: kode auth login antigravity",
        ),
    }
}

fn model_check(model: &ModelConfig) -> Check {
    if model.model.is_empty() {
        Check::fail(
            "LLM",
            "model",
            "not set",
            "set model.model in .kode/config.toml",
        )
    } else {
        Check::pass("LLM", "model", model.model.clone())
    }
}

fn api_key_check(openai_present: bool, kode_present: bool) -> Check {
    if openai_present || kode_present {
        Check::pass("LLM", "OPENAI_API_KEY", "present")
    } else {
        Check::fail("LLM", "OPENAI_API_KEY", "not set", "set OPENAI_API_KEY")
    }
}

// --- async collectors -------------------------------------------------------

async fn zindeks_binary_check(cfg: &ZindeksConfig) -> Check {
    if let Some(version) = crate::setup::probe_version(&cfg.command).await {
        return Check::pass("Zindeks", "binary", version);
    }

    if let Some(managed_dir) = kode_core::managed_bin_dir() {
        let managed_bin = managed_dir.join(if cfg!(windows) {
            "zindeks.exe"
        } else {
            "zindeks"
        });
        if let Some(version) = crate::setup::probe_version(&managed_bin).await {
            return Check::pass("Zindeks", "binary", version);
        }
    }

    Check::fail("Zindeks", "binary", "not found", "run: kode setup")
}

async fn collect_zindeks_checks(checks: &mut Vec<Check>, cwd: &Path, config: &KodeConfig) {
    checks.push(zindeks_binary_check(&config.zindeks).await);

    let adapter =
        match tokio::time::timeout(TIMEOUT, ZindeksAdapter::connect(&config.zindeks, cwd)).await {
            Ok(Ok(adapter)) => {
                checks.push(Check::pass("Zindeks", "service", "handshake ok"));
                adapter
            }
            Ok(Err(err)) => {
                checks.push(Check::fail(
                    "Zindeks",
                    "service",
                    err.to_string(),
                    "run: kode setup",
                ));
                return;
            }
            Err(_) => {
                checks.push(Check::fail(
                    "Zindeks",
                    "service",
                    "timed out",
                    "run: kode setup",
                ));
                return;
            }
        };

    let indexed = match tokio::time::timeout(TIMEOUT, adapter.is_indexed()).await {
        Ok(Ok(indexed)) => indexed,
        Ok(Err(err)) => {
            checks.push(Check::fail(
                "Zindeks",
                "index",
                err.to_string(),
                "run: kode setup",
            ));
            return;
        }
        Err(_) => {
            checks.push(Check::fail(
                "Zindeks",
                "index",
                "timed out",
                "run: kode setup",
            ));
            return;
        }
    };

    if !indexed {
        checks.push(Check::warn(
            "Zindeks",
            "index",
            "not indexed",
            "run: zindeks index . (or let kode exec bind it)",
        ));
        return;
    }
    checks.push(Check::pass("Zindeks", "index", "repository indexed"));

    let bound = match tokio::time::timeout(TIMEOUT, adapter.ensure_bound()).await {
        Ok(result) => result,
        Err(_) => Err(kode_intel::IntelError::Timeout),
    };
    if let Err(err) = bound {
        checks.push(Check::fail(
            "Zindeks",
            "health",
            err.to_string(),
            "run: kode setup",
        ));
        return;
    }

    match tokio::time::timeout(TIMEOUT, adapter.health()).await {
        Ok(Ok(health)) => checks.push(Check::pass(
            "Zindeks",
            "health",
            format!(
                "{} docs, {} symbols indexed",
                health.documents, health.symbols
            ),
        )),
        Ok(Err(err)) => checks.push(Check::fail(
            "Zindeks",
            "health",
            err.to_string(),
            "run: kode setup",
        )),
        Err(_) => checks.push(Check::fail(
            "Zindeks",
            "health",
            "timed out",
            "run: kode setup",
        )),
    }
}

async fn collect_ingat_checks(checks: &mut Vec<Check>, config: &KodeConfig) {
    let adapter = IngatAdapter::new(&config.ingat);
    const INGAT_FIX: &str = "run: kode setup (or start the Ingat app)";

    let healthy = match tokio::time::timeout(TIMEOUT, adapter.health()).await {
        Ok(Ok(())) => {
            checks.push(Check::pass("Ingat", "service", "reachable"));
            true
        }
        Ok(Err(err)) => {
            checks.push(Check::fail("Ingat", "service", err.to_string(), INGAT_FIX));
            false
        }
        Err(_) => {
            checks.push(Check::fail("Ingat", "service", "timed out", INGAT_FIX));
            false
        }
    };

    if !healthy {
        checks.push(Check::warn("Ingat", "memory", "skipped (service down)", ""));
        return;
    }

    match tokio::time::timeout(TIMEOUT, adapter.stats()).await {
        Ok(Ok(stats)) => checks.push(Check::pass(
            "Ingat",
            "memory",
            format!("{} memories (v{})", stats.total, stats.version),
        )),
        Ok(Err(err)) => checks.push(Check::fail("Ingat", "memory", err.to_string(), INGAT_FIX)),
        Err(_) => checks.push(Check::fail("Ingat", "memory", "timed out", INGAT_FIX)),
    }
}

const MCP_TIMEOUT: Duration = Duration::from_secs(10);

async fn collect_mcp_checks(checks: &mut Vec<Check>, config: &KodeConfig) {
    if config.mcp.servers.is_empty() {
        checks.push(Check::pass("MCP", "servers", "none configured"));
        return;
    }

    for (name, server) in &config.mcp.servers {
        if !server.enabled {
            checks.push(Check::warn("MCP", name.clone(), "disabled", ""));
            continue;
        }
        checks.push(mcp_server_check(name, server).await);
    }
}

async fn mcp_server_check(name: &str, cfg: &McpServerConfig) -> Check {
    match tokio::time::timeout(MCP_TIMEOUT, probe_mcp_server(cfg)).await {
        Ok(Ok(n)) => Check::pass("MCP", name.to_string(), format!("{n} tools")),
        Ok(Err(err)) => Check::fail(
            "MCP",
            name.to_string(),
            err.to_string(),
            "check command/args",
        ),
        Err(_) => Check::fail("MCP", name.to_string(), "timed out", "check command/args"),
    }
}

/// Spawns `cfg.command`, performs the MCP handshake, and lists its tools,
/// returning the tool count. The child is killed on drop.
async fn probe_mcp_server(cfg: &McpServerConfig) -> kode_mcp::Result<usize> {
    let mut cmd = tokio::process::Command::new(&cfg.command);
    cmd.args(&cfg.args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        kode_mcp::McpError::Unavailable(format!("cannot start {}: {e}", cfg.command))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| kode_mcp::McpError::Unavailable("mcp child missing stdin".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| kode_mcp::McpError::Unavailable("mcp child missing stdout".to_string()))?;

    let mut client = kode_mcp::McpClient::new(stdout, stdin);
    client.initialize().await?;
    let tools = client.list_tools().await?;
    Ok(tools.len())
}

async fn git_binary_check() -> Check {
    let output = tokio::time::timeout(
        TIMEOUT,
        tokio::process::Command::new("git")
            .arg("--version")
            .output(),
    )
    .await;

    match output {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let line = text.lines().next().unwrap_or("git").trim().to_string();
            Check::pass("Git", "binary", line)
        }
        Ok(Ok(_)) => Check::fail("Git", "binary", "git --version failed", "install git"),
        Ok(Err(err)) => Check::fail("Git", "binary", err.to_string(), "install git"),
        Err(_) => Check::fail("Git", "binary", "timed out", "install git"),
    }
}

fn git_repository_check(cwd: &Path) -> Check {
    if cwd.join(".git").exists() {
        Check::pass("Git", "repository", "found")
    } else {
        Check::warn("Git", "repository", "no .git directory", "run: git init")
    }
}

fn environment_writable_check(cwd: &Path) -> Check {
    let probe = cwd.join(".kode-doctor-probe");
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::pass("Environment", "writable", "cwd is writable")
        }
        Err(err) => Check::fail(
            "Environment",
            "writable",
            err.to_string(),
            "check directory permissions",
        ),
    }
}

// --- rendering --------------------------------------------------------------

fn render_line(c: &Check) -> String {
    let symbol = c.status.symbol();
    let mut detail = c.detail.clone();
    if matches!(c.status, CheckStatus::Fail | CheckStatus::Warn)
        && let Some(fix) = &c.fix
        && !fix.is_empty()
    {
        detail.push_str(" \u{2014} "); // " — "
        detail.push_str(fix);
    }
    format!("  {:<NAME_WIDTH$}{symbol} {detail}", c.name)
}

/// Renders a full doctor report as text. Pure: takes checks in the order
/// they were collected (already grouped by section) and groups adjacent
/// runs sharing the same `section` under one header.
pub fn render(checks: &[Check]) -> String {
    let mut out = String::from("Kode Doctor\n\n");
    let mut current_section: Option<&str> = None;
    for c in checks {
        if current_section != Some(c.section) {
            out.push_str(c.section);
            out.push('\n');
            current_section = Some(c.section);
        }
        out.push_str(&render_line(c));
        out.push('\n');
    }

    out.push('\n');
    let problems = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .count();
    if problems == 0 {
        out.push_str("all checks passed.\n");
    } else {
        out.push_str(&format!("{problems} problem(s) found.\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn temp_project_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-doctor-test-{}-{}-{}",
            std::process::id(),
            nanos(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_check_missing_file_is_warn() {
        let dir = temp_project_dir();
        let exists = KodeConfig::config_path(&dir).is_file();
        let load: Result<KodeConfig, String> = KodeConfig::load(&dir).map_err(|e| e.to_string());

        let check = config_check(&load, exists);
        assert_eq!(check.status, CheckStatus::Warn);
        assert_eq!(check.detail, "using defaults");
        assert_eq!(check.fix.as_deref(), Some("create .kode/config.toml"));
    }

    #[test]
    fn config_check_invalid_toml_is_fail() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(kode_dir.join("config.toml"), "not = [valid toml").unwrap();

        let exists = KodeConfig::config_path(&dir).is_file();
        let load: Result<KodeConfig, String> = KodeConfig::load(&dir).map_err(|e| e.to_string());

        let check = config_check(&load, exists);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(!check.detail.is_empty());
        assert_eq!(check.fix.as_deref(), Some("fix the TOML syntax"));
    }

    #[test]
    fn config_check_valid_file_is_pass() {
        let dir = temp_project_dir();
        let kode_dir = dir.join(".kode");
        std::fs::create_dir_all(&kode_dir).unwrap();
        std::fs::write(
            kode_dir.join("config.toml"),
            "[model]\nprovider = \"openai\"\n",
        )
        .unwrap();

        let exists = KodeConfig::config_path(&dir).is_file();
        let load: Result<KodeConfig, String> = KodeConfig::load(&dir).map_err(|e| e.to_string());

        let check = config_check(&load, exists);
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, "loaded");
        assert_eq!(check.fix, None);
    }

    #[test]
    fn provider_check_openai_is_pass() {
        let model = ModelConfig {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            effort: String::new(),
        };
        assert_eq!(provider_check(&model).status, CheckStatus::Pass);
    }

    #[test]
    fn provider_check_other_is_fail() {
        let model = ModelConfig {
            provider: "mystery".to_string(),
            model: "gpt-4".to_string(),
            effort: String::new(),
        };
        let check = provider_check(&model);
        assert_eq!(check.status, CheckStatus::Fail);
        let fix = check.fix.as_deref().unwrap();
        assert!(fix.contains("openai"));
        assert!(fix.contains("anthropic"));
        assert!(fix.contains("codex"));
        assert!(fix.contains("opencode-go"));
    }

    #[test]
    fn provider_check_codex_and_opencode_family_are_pass() {
        for provider in [
            "anthropic",
            "codex",
            "opencode-go",
            "opencode",
            "kilo",
            "lmstudio",
        ] {
            let model = ModelConfig {
                provider: provider.to_string(),
                model: "m".to_string(),
                effort: String::new(),
            };
            assert_eq!(provider_check(&model).status, CheckStatus::Pass);
        }
    }

    #[test]
    fn codex_check_ok_chatgpt_mode_shows_last4_account_id_only() {
        let auth = kode_model::CodexAuth {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            account_id: "abcd1234wxyz".to_string(),
            last_refresh: "2026-08-15T00:00:00Z".to_string(),
            api_key: None,
            auth_mode: "chatgpt".to_string(),
        };
        let check = codex_check(&Ok(auth));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("wxyz"));
        assert!(!check.detail.contains("abcd1234wxyz"));
    }

    #[test]
    fn codex_check_apikey_mode_missing_key_is_fail() {
        let auth = kode_model::CodexAuth {
            access_token: String::new(),
            refresh_token: String::new(),
            account_id: String::new(),
            last_refresh: String::new(),
            api_key: None,
            auth_mode: "apikey".to_string(),
        };
        let check = codex_check(&Ok(auth));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.fix.as_deref(), Some("run: kode auth login codex"));
    }

    #[test]
    fn codex_check_err_is_fail() {
        let check = codex_check(&Err("boom".to_string()));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.detail, "boom");
    }

    #[test]
    fn opencode_check_true_is_pass_with_provider_id() {
        let check = opencode_check("kilo", &Ok(true));
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.detail.contains("kilo"));
    }

    #[test]
    fn opencode_check_false_is_fail() {
        let check = opencode_check("kilo", &Ok(false));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.fix.as_deref(), Some("run: kode auth login kilo"));
    }

    #[test]
    fn anthropic_check_api_key_is_pass() {
        let check = anthropic_check(&Ok(kode_model::AnthropicAuth::ApiKey(
            "sk-ant-test".to_string(),
        )));
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, "api key present");
    }

    #[test]
    fn anthropic_check_oauth_is_pass() {
        let check = anthropic_check(&Ok(kode_model::AnthropicAuth::OAuth {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            expires_at: 123,
        }));
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, "oauth token present");
    }

    #[test]
    fn antigravity_check_ok_is_pass() {
        let check = antigravity_check(&Ok(kode_model::AntigravityAuth {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1,
            project_id: "p".into(),
        }));
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn antigravity_check_err_is_fail() {
        let check = antigravity_check(&Err("boom".to_string()));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(
            check.fix.as_deref(),
            Some("run: kode auth login antigravity")
        );
    }

    #[test]
    fn anthropic_check_err_is_fail() {
        let check = anthropic_check(&Err("boom".to_string()));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.detail, "boom");
        assert_eq!(
            check.fix.as_deref(),
            Some("run: kode auth login anthropic (or set ANTHROPIC_API_KEY)")
        );
    }

    #[test]
    fn last4_chars_short_string_returns_whole_string() {
        assert_eq!(last4_chars("ab"), "ab");
        assert_eq!(last4_chars("abcdef"), "cdef");
    }

    #[test]
    fn model_check_empty_is_fail() {
        let model = ModelConfig {
            provider: "openai".to_string(),
            model: String::new(),
            effort: String::new(),
        };
        let check = model_check(&model);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(
            check.fix.as_deref(),
            Some("set model.model in .kode/config.toml")
        );
    }

    #[test]
    fn model_check_nonempty_is_pass() {
        let model = ModelConfig {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            effort: String::new(),
        };
        let check = model_check(&model);
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, "gpt-4");
    }

    #[test]
    fn api_key_check_present_via_either_var_is_pass() {
        assert_eq!(api_key_check(true, false).status, CheckStatus::Pass);
        assert_eq!(api_key_check(false, true).status, CheckStatus::Pass);
        assert_eq!(api_key_check(true, true).status, CheckStatus::Pass);
    }

    #[test]
    fn api_key_check_absent_is_fail() {
        let check = api_key_check(false, false);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.fix.as_deref(), Some("set OPENAI_API_KEY"));
    }

    #[test]
    fn render_formats_each_status_with_padding_and_fix_suffix() {
        let checks = vec![
            Check::pass("Section", "passing", "ok"),
            Check::warn("Section", "warning", "meh", "do the fix"),
            Check::fail("Section", "failing", "broken", "fix it"),
        ];
        let out = render(&checks);

        assert!(out.starts_with("Kode Doctor\n\n"));
        assert!(out.contains("Section\n"));

        let pass_line = format!("  {:<24}{} {}", "passing", "\u{2713}", "ok");
        let warn_line = format!("  {:<24}{} {}", "warning", "!", "meh \u{2014} do the fix");
        let fail_line = format!(
            "  {:<24}{} {}",
            "failing", "\u{2717}", "broken \u{2014} fix it"
        );
        assert!(out.contains(&pass_line));
        assert!(out.contains(&warn_line));
        assert!(out.contains(&fail_line));

        assert!(out.ends_with("1 problem(s) found.\n"));
    }

    #[test]
    fn render_no_fails_reports_all_passed() {
        let checks = vec![
            Check::pass("Section", "a", "ok"),
            Check::warn("Section", "b", "meh", ""),
        ];
        let out = render(&checks);
        assert!(out.ends_with("all checks passed.\n"));
    }

    #[test]
    fn render_counts_only_fails_for_footer() {
        let checks = vec![
            Check::pass("Section", "a", "ok"),
            Check::warn("Section", "b", "meh", "fix b"),
            Check::fail("Section", "c", "broken", "fix c"),
            Check::fail("Section", "d", "broken2", "fix d"),
        ];
        let out = render(&checks);
        assert!(out.ends_with("2 problem(s) found.\n"));
    }

    #[test]
    fn render_groups_adjacent_same_section_checks_under_one_header() {
        let checks = vec![
            Check::pass("A", "one", "ok"),
            Check::pass("A", "two", "ok"),
            Check::pass("B", "three", "ok"),
        ];
        let out = render(&checks);
        assert_eq!(out.matches("A\n").count(), 1);
        assert_eq!(out.matches("B\n").count(), 1);
    }
}
