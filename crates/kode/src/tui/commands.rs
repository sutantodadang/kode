use std::path::Path;

use crossterm::event::KeyCode;
use kode_core::config::KodeConfig;
use tokio::sync::mpsc;

use super::run::perform_copy;
use super::state::*;
use crate::custom_commands;

/// A parsed `/`-prefixed slash command.
#[derive(Debug, Clone, PartialEq)]
pub enum SlashCommand {
    /// `/model` (open picker) or `/model <name>` (set directly).
    Model(Option<String>),
    /// `/effort <value>`.
    Effort(String),
    /// `/provider` (open picker) or `/provider <name>` (set directly).
    Provider(Option<String>),
    /// `/copy` — copies `last_response` to the clipboard.
    Copy,
    /// `/resume` — opens the session picker.
    Resume,
    Status,
    Exit,
    /// `/plan` — toggles plan mode (session-only; see `AppState::plan_mode`).
    Plan,
    /// `/image <path>` — attaches an image file to the next user turn.
    Image(String),
    /// `/router [key=value…]` — show or correct the last router training record.
    Router(String),
    /// `/map` — zero-token repo map from the code graph.
    Map,
    /// `/index` — build or refresh the code index in the background.
    Index,
    /// `/why [N]` — show a turn's provenance from its ledger.
    Why(String),
    Help,
    /// `/name [args]` where `name` isn't a builtin. Resolved against
    /// discovered custom commands at handle time (not parse time) — an
    /// unmatched name falls back to the same "unknown command" transcript
    /// line `Unknown` produces.
    Custom {
        name: String,
        args: String,
    },
    Unknown(String),
}

/// Builtin command names (lowercase, no leading `/`) — matches
/// [`SLASH_COMMANDS`]. Custom commands never shadow these; discovery
/// filters them out up front.
pub const BUILTIN_COMMAND_NAMES: &[&str] = &[
    "model", "effort", "provider", "copy", "resume", "status", "exit", "plan", "image", "router",
    "map", "index", "why", "help",
];

/// The providers `/provider` accepts, in picker display order.
pub const VALID_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "antigravity",
    "codex",
    "opencode-go",
    "opencode",
    "kilo",
    "lmstudio",
];

/// Commands listed in the `/` hint menu: (name, one-line description).
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/model", "pick or set model"),
    ("/effort", "set reasoning effort (minimal|low|medium|high)"),
    ("/provider", "pick provider"),
    ("/copy", "copy last response to clipboard"),
    ("/resume", "resume a previous session"),
    ("/status", "show model, mode, and available context"),
    ("/exit", "exit Kode"),
    ("/image", "attach PNG, JPEG, GIF, or WebP"),
    (
        "/plan",
        "toggle plan mode (plan first, then approve to run)",
    ),
    ("/router", "show or correct the last router decision"),
    ("/map", "show the repo map (graph, 0 tokens)"),
    ("/index", "index this repo in the background"),
    ("/why", "show where the last answer came from"),
    ("/help", "list commands + shortcuts"),
];

/// Hint-menu rows for the current input. Non-empty only while the input is a
/// bare `/`-prefixed token with no whitespace (once arguments start, the menu
/// hides). Bare `/` lists every command; `/mo` narrows by prefix. Builtins
/// come first (in [`SLASH_COMMANDS`] order), then discovered custom
/// commands sorted by name — `custom` is gathered by the caller via
/// [`custom_commands::discover`] so this stays a pure, filesystem-free
/// function.
pub fn slash_hint_items(
    input: &str,
    custom: &[custom_commands::CustomCommand],
) -> Vec<(String, String)> {
    if !input.starts_with('/') || input.contains(char::is_whitespace) {
        return Vec::new();
    }
    let builtins = SLASH_COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(input))
        .map(|(name, desc)| (name.to_string(), desc.to_string()));
    let customs = custom
        .iter()
        .map(|c| (format!("/{}", c.name), c.description.clone()))
        .filter(|(name, _)| name.starts_with(input));
    builtins.chain(customs).collect()
}

/// Parses `input` as a slash command. Returns `None` when `input` doesn't
/// start with `/` — slash commands are only recognized at the start of the
/// line, per design.
pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    Some(match cmd {
        "/model" => SlashCommand::Model(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        "/effort" => SlashCommand::Effort(rest.to_string()),
        "/provider" => SlashCommand::Provider(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        "/copy" => SlashCommand::Copy,
        "/resume" => SlashCommand::Resume,
        "/status" => SlashCommand::Status,
        "/exit" => SlashCommand::Exit,
        "/plan" => SlashCommand::Plan,
        "/image" => SlashCommand::Image(rest.to_string()),
        "/router" => SlashCommand::Router(rest.to_string()),
        "/map" => SlashCommand::Map,
        "/index" => SlashCommand::Index,
        "/why" => SlashCommand::Why(rest.to_string()),
        "/help" => SlashCommand::Help,
        other => {
            let name = other.trim_start_matches('/').to_lowercase();
            if name.is_empty() {
                SlashCommand::Unknown(other.to_string())
            } else {
                SlashCommand::Custom {
                    name,
                    args: rest.to_string(),
                }
            }
        }
    })
}

/// Auth-state annotation appended to a provider's name in the `/provider`
/// picker. `" ✓ logged in"` when credentials for that provider are on disk
/// (or in the environment for `openai`/`anthropic`); `" (local)"` always for
/// `lmstudio` (no login needed — it's a local server); `""` otherwise. Pure —
/// callers gather `codex_auth`/`opencode_keys`/`env_key`/`anthropic_auth`
/// from disk/env once per picker open.
pub fn provider_auth_state(
    provider: &str,
    codex_auth: bool,
    opencode_keys: &[String],
    env_key: bool,
    anthropic_auth: bool,
    antigravity_auth: bool,
) -> &'static str {
    match provider {
        "codex" => {
            if codex_auth {
                " ✓ logged in"
            } else {
                ""
            }
        }
        "opencode-go" | "opencode" | "kilo" => {
            if opencode_keys.iter().any(|k| k == provider) {
                " ✓ logged in"
            } else {
                " · login required"
            }
        }
        "openai" => {
            if env_key {
                " ✓ logged in"
            } else {
                ""
            }
        }
        "anthropic" => {
            if anthropic_auth {
                " ✓ logged in"
            } else {
                ""
            }
        }
        "antigravity" => {
            if antigravity_auth {
                " ✓ logged in"
            } else {
                ""
            }
        }
        "lmstudio" => " (local)",
        _ => "",
    }
}

/// Whether Kode's own codex OAuth credentials file exists (`kode auth login
/// codex`). Used only to power the `/provider` picker annotation and the
/// startup hint — not a validity check of the tokens inside.
pub(crate) fn codex_auth_exists() -> bool {
    kode_model::codex::default_auth_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Provider ids with a stored API key in Kode's opencode-family auth store
/// (`~/.kode/auth/opencode.json`). Empty when the file is missing/invalid.
pub(crate) fn opencode_key_ids() -> Vec<String> {
    let Some(path) = kode_model::opencode::default_auth_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default()
}

/// True when an OpenAI API key is available via environment variable
/// (`OPENAI_API_KEY` or `KODE_API_KEY`) — the credential path the `openai`
/// provider actually uses.
pub(crate) fn openai_env_key_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("KODE_API_KEY").is_ok()
}

/// True when anthropic credentials are available: either Kode's own auth
/// store (`~/.kode/auth/anthropic.json`, api key or oauth) exists, or
/// `ANTHROPIC_API_KEY` is set in the environment.
pub(crate) fn anthropic_auth_present() -> bool {
    let file_present = kode_model::anthropic::default_auth_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    file_present
        || std::env::var("ANTHROPIC_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
}

/// True when Kode's own antigravity auth store
/// (`~/.kode/auth/antigravity.json`) exists.
pub(crate) fn antigravity_auth_present() -> bool {
    kode_model::antigravity::default_auth_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Whether `provider` has usable credentials (same sources the `/provider`
/// picker annotates).
pub(crate) fn provider_logged_in(provider: &str) -> bool {
    !matches!(
        provider_auth_state(
            provider,
            codex_auth_exists(),
            &opencode_key_ids(),
            openai_env_key_present(),
            anthropic_auth_present(),
            antigravity_auth_present(),
        ),
        "" | " · login required"
    )
}

pub(crate) fn setup_card_item(card: crate::first_run::SetupCard, provider: &str) -> String {
    use crate::first_run::SetupCard;
    match card {
        SetupCard::Provider => "choose a provider and model".to_string(),
        SetupCard::Login => format!("log in to {provider} (leaves the TUI, opens your browser)"),
        SetupCard::Engine => format!(
            "download the code-graph engine (zindeks v{}, checksum-verified)",
            crate::engine_assets::ZINDEKS_VERSION
        ),
        SetupCard::Index => "index this repo in the background".to_string(),
    }
}

/// Shows one setup card through the picker overlay (Enter = do it, Esc =
/// skip for this launch).
pub(crate) fn open_setup_card(
    state: &mut AppState,
    card: crate::first_run::SetupCard,
    provider: &str,
    position: usize,
    total: usize,
) {
    state.picker.request_id = state.picker.request_id.wrapping_add(1);
    state.picker.open = true;
    state.picker.kind = PickerKind::Setup;
    state.picker.filter.clear();
    state.picker.selected = 0;
    state.picker.items = vec![setup_card_item(card, provider)];
    state.picker.note = Some(format!(
        "setup {position}/{total} · Enter to do it · Esc to skip"
    ));
    state.setup_card = Some(card);
}

/// Validates a reasoning-effort value against
/// [`kode_core::config::VALID_EFFORTS`]. Returns the value on success or an
/// error message listing the valid values.
pub fn validate_effort(value: &str) -> Result<String, String> {
    if kode_core::config::VALID_EFFORTS.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(format!(
            "invalid effort '{value}' (valid: {})",
            kode_core::config::VALID_EFFORTS.join(", ")
        ))
    }
}

/// Filters `items` by `filter` using a case-insensitive substring match.
/// Empty `filter` yields all items, unchanged order.
pub fn picker_filtered_items(items: &[String], filter: &str) -> Vec<String> {
    if filter.is_empty() {
        return items.to_vec();
    }
    let needle = filter.to_lowercase();
    items
        .iter()
        .filter(|item| item.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// Decides what Enter selects in the picker. Unmatched text is never
/// interpreted as a provider or model; `/model <name>` is the explicit
/// custom-model path.
pub fn picker_enter_selection(
    filtered: &[String],
    _filter: &str,
    selected: usize,
) -> Option<String> {
    filtered.get(selected).cloned()
}

/// Effect requested by a key press while the picker is open.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerOutcome {
    Continue,
    Select(String),
    Cancel,
}

/// Handles one key press while `state.picker.open`. Mutates the picker's
/// filter/selection in place; returns the effect (if any) the caller must
/// apply (selecting a model persists it and closes the picker).
pub fn handle_picker_key(state: &mut AppState, code: KeyCode) -> PickerOutcome {
    match code {
        KeyCode::Esc => PickerOutcome::Cancel,
        KeyCode::Enter => {
            let filtered = picker_filtered_items(&state.picker.items, &state.picker.filter);
            match picker_enter_selection(&filtered, &state.picker.filter, state.picker.selected) {
                Some(model) => PickerOutcome::Select(model),
                None => PickerOutcome::Continue,
            }
        }
        KeyCode::Up => {
            state.picker.selected = state.picker.selected.saturating_sub(1);
            PickerOutcome::Continue
        }
        KeyCode::Down => {
            let len = picker_filtered_items(&state.picker.items, &state.picker.filter).len();
            if len > 0 {
                state.picker.selected = (state.picker.selected + 1).min(len - 1);
            }
            PickerOutcome::Continue
        }
        KeyCode::Char(c) => {
            state.picker.filter.push(c);
            state.picker.selected = 0;
            PickerOutcome::Continue
        }
        KeyCode::Backspace => {
            state.picker.filter.pop();
            state.picker.selected = 0;
            PickerOutcome::Continue
        }
        _ => PickerOutcome::Continue,
    }
}

pub(crate) fn append_picker_filter(state: &mut AppState, pasted: &str) {
    if state.pending.is_empty() {
        state
            .picker
            .filter
            .extend(pasted.chars().filter(|c| !c.is_control()));
        state.picker.selected = 0;
    }
}

/// Opens the picker (clearing prior filter/items) and spawns a best-effort
/// catalog fetch for `provider`, delivered back via `tx`.
pub(crate) fn open_picker(
    state: &mut AppState,
    provider: String,
    tx: &mpsc::UnboundedSender<PickerLoaded>,
) {
    state.picker.request_id = state.picker.request_id.wrapping_add(1);
    let request_id = state.picker.request_id;
    state.picker.open = true;
    state.picker.kind = PickerKind::Model;
    state.picker.provider = provider.clone();
    state.picker.filter.clear();
    state.picker.selected = 0;
    state.picker.items.clear();
    state.picker.note = Some("loading models...".to_string());

    let tx = tx.clone();
    tokio::spawn(async move {
        let msg = match kode_model::catalog::list_catalog(&provider, None).await {
            Ok(catalog) => PickerLoaded {
                request_id,
                items: catalog.models,
                note: catalog.note,
            },
            Err(e) => PickerLoaded {
                request_id,
                items: vec![],
                note: Some(e),
            },
        };
        let _ = tx.send(msg);
    });
}

pub(crate) fn apply_picker_loaded(state: &mut AppState, loaded: PickerLoaded) {
    if state.picker.open
        && state.picker.kind == PickerKind::Model
        && state.picker.request_id == loaded.request_id
    {
        state.picker.items = loaded.items;
        state.picker.note = loaded.note;
        state.picker.selected = 0;
    }
}

/// Opens the `/provider` picker: a static list of [`VALID_PROVIDERS`], each
/// annotated with its auth state via [`provider_auth_state`]. Synchronous —
/// no catalog fetch, just local disk/env reads.
pub(crate) fn open_provider_picker(state: &mut AppState) {
    state.picker.request_id = state.picker.request_id.wrapping_add(1);
    state.picker.open = true;
    state.picker.kind = PickerKind::Provider;
    state.picker.filter.clear();
    state.picker.selected = 0;
    state.picker.note = None;

    let codex_auth = codex_auth_exists();
    let opencode_keys = opencode_key_ids();
    let env_key = openai_env_key_present();
    let anthropic_auth = anthropic_auth_present();
    let antigravity_auth = antigravity_auth_present();
    state.picker.items = VALID_PROVIDERS
        .iter()
        .map(|p| {
            format!(
                "{p}{}",
                provider_auth_state(
                    p,
                    codex_auth,
                    &opencode_keys,
                    env_key,
                    anthropic_auth,
                    antigravity_auth
                )
            )
        })
        .collect();
}

pub(crate) fn open_command_picker(state: &mut AppState, cwd: &Path) {
    state.picker.request_id = state.picker.request_id.wrapping_add(1);
    state.picker.open = true;
    state.picker.kind = PickerKind::Command;
    state.picker.filter.clear();
    state.picker.selected = 0;
    state.picker.note = None;
    let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
    state.picker.items = slash_hint_items("/", &custom)
        .into_iter()
        .map(|(name, desc)| format!("{name}  · {desc}"))
        .collect();
}

/// Browses a provider's models without changing the active pair. Selection
/// commits provider and model together; Esc leaves both as they were.
pub(crate) fn apply_provider_selection(
    state: &mut AppState,
    picker_tx: &mpsc::UnboundedSender<PickerLoaded>,
    provider: &str,
) {
    open_picker(state, provider.to_string(), picker_tx);
}

pub(crate) fn commit_model_selection(
    state: &mut AppState,
    cwd: &Path,
    config: &mut KodeConfig,
    model: &str,
) -> bool {
    let provider = if state.picker.open && state.picker.kind == PickerKind::Model {
        state.picker.provider.clone()
    } else {
        config.model.provider.clone()
    };
    if let Err(err) = KodeConfig::update_model_config(cwd, Some(&provider), Some(model), None) {
        state.transcript.push(TranscriptLine::new(
            Gutter::Note,
            format!("could not save model selection: {err}"),
        ));
        return false;
    }
    state.status.provider = provider.clone();
    state.status.model = model.to_string();
    config.model.provider = provider.clone();
    config.model.model = model.to_string();
    state.picker.open = false;
    state.transcript.push(TranscriptLine::new(
        Gutter::Note,
        format!("model set: {provider} / {model}"),
    ));
    true
}

/// Applies a parsed slash command to `state`/`config`. `/model` with no
/// argument opens the picker (async catalog fetch via `picker_tx`); every
/// other successful set persists immediately to
/// `<cwd>/.kode/config.toml` via [`KodeConfig::update_model_selection`].
///
/// Returns `Some(expanded_prompt)` only for `SlashCommand::Custom` once
/// resolved against a discovered `.kode/commands`/`~/.kode/commands`
/// template — the caller (`tui::run`) submits it as an ordinary task
/// through the exact same path a typed non-slash prompt takes. Every other
/// variant returns `None`; its side effects (state/config mutation,
/// transcript notes, picker opens) are applied in place.
pub(crate) fn handle_slash_command(
    state: &mut AppState,
    cwd: &Path,
    config: &mut KodeConfig,
    picker_tx: &mpsc::UnboundedSender<PickerLoaded>,
    cmd: SlashCommand,
) -> Option<String> {
    let mut submit = None;
    match cmd {
        SlashCommand::Model(None) => {
            open_picker(state, config.model.provider.clone(), picker_tx);
        }
        SlashCommand::Model(Some(name)) => {
            if commit_model_selection(state, cwd, config, &name) {
                state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    "custom model name was not catalog-validated",
                ));
            }
        }
        SlashCommand::Effort(value) => match validate_effort(&value) {
            Ok(v) => {
                state.status.effort = v.clone();
                config.model.effort = v.clone();
                let _ = KodeConfig::update_model_selection(cwd, None, Some(&v));
                state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    format!("effort set: {v}"),
                ));
            }
            Err(msg) => state
                .transcript
                .push(TranscriptLine::new(Gutter::Note, msg)),
        },
        SlashCommand::Provider(None) => {
            open_provider_picker(state);
        }
        SlashCommand::Provider(Some(name)) => {
            if VALID_PROVIDERS.contains(&name.as_str()) {
                apply_provider_selection(state, picker_tx, &name);
            } else {
                state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    format!(
                        "invalid provider '{name}' (valid: {})",
                        VALID_PROVIDERS.join(", ")
                    ),
                ));
            }
        }
        SlashCommand::Copy => perform_copy(state),
        SlashCommand::Status => {
            let mode = match (state.plan_mode, state.auto_mode) {
                (true, true) => "PLAN · AUTO",
                (true, false) => "PLAN · manual",
                (false, true) => "BUILD · AUTO",
                (false, false) => "BUILD · manual",
            };
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!(
                    "status: {mode} · {} / {}",
                    state.status.provider, state.status.model
                ),
            ));
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                match &state.knowledge {
                    Some(k) => format!(
                        "context: {} code facts · {} memories · {} git facts",
                        k.zindeks.len(),
                        k.ingat.len(),
                        k.git.len()
                    ),
                    None => "context: not compiled for this session".to_string(),
                },
            ));
        }
        SlashCommand::Exit => {}
        SlashCommand::Map | SlashCommand::Index => {}
        SlashCommand::Why(arg) => match crate::why::parse_turn_arg(&arg, state.history.len()) {
            Ok(index) => {
                state.why_lines = Some(crate::why::why_lines(&state.history[index], index + 1))
            }
            Err(message) => state
                .transcript
                .push(TranscriptLine::new(Gutter::Note, message)),
        },
        SlashCommand::Router(args) => {
            let lines = if args.trim().is_empty() {
                crate::router_cmd::describe_last(cwd)
            } else {
                let parts: Vec<String> = args.split_whitespace().map(str::to_string).collect();
                vec![crate::router_cmd::correct(cwd, "last", &parts).unwrap_or_else(|e| e)]
            };
            for line in lines {
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::Note, line));
            }
        }
        SlashCommand::Resume => {
            if state.running {
                state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    "finish the current run before resuming another session",
                ));
                return None;
            }
            let metas = crate::session::list(cwd, 20);
            if metas.is_empty() {
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::Note, "no sessions to resume"));
            } else {
                state.picker = PickerState {
                    request_id: state.picker.request_id.wrapping_add(1),
                    open: true,
                    kind: PickerKind::Session,
                    provider: String::new(),
                    items: metas
                        .iter()
                        .map(|m| {
                            format!(
                                "{}  {}  · {} turns · {}",
                                m.id,
                                truncate_chars(&m.first_task, 40),
                                m.turns,
                                m.created.get(..16).unwrap_or(&m.created)
                            )
                        })
                        .collect(),
                    filter: String::new(),
                    selected: 0,
                    note: None,
                };
            }
        }
        SlashCommand::Plan => {
            state.plan_mode = !state.plan_mode;
            let text = if state.plan_mode {
                "plan mode on — tasks are planned first, then wait for your approval to run"
            } else {
                "plan mode off"
            };
            state
                .transcript
                .push(TranscriptLine::new(Gutter::Note, text));
        }
        SlashCommand::Image(path) => {
            if path.is_empty() {
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::Note, "usage: /image <path>"));
            } else {
                match state.add_image_path(cwd, &path) {
                    Ok(()) => state.transcript.push(TranscriptLine::new(
                        Gutter::Note,
                        "image attached to the next message",
                    )),
                    Err(err) => state.transcript.push(TranscriptLine::new(
                        Gutter::Error,
                        format!("image attachment failed: {err}"),
                    )),
                }
            }
        }
        SlashCommand::Help => {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                "commands: /model [name], /effort <minimal|low|medium|high|xhigh|max|ultra>, \
                 /provider [name], /resume, /status, /exit, /image <path>, /copy, /plan, /map, /index, /help · shift+tab toggles auto mode (tools run \
                 without asking) · shift+enter adds a newline · long paste becomes a compact \
                 attachment; paste or drag an image path to attach it · Enter follows the composer label: send, steer, or queue · Alt+Enter explicitly queues · \
                 ? opens shortcuts · Ctrl+P opens commands · Ctrl+A inspects attachments · Ctrl+Y copies the last response · Ctrl+T toggles select mode \
                 (releases mouse capture for native text selection) · Ctrl+K expands evidence, \
                 Ctrl+L opens the Run Map, Esc cancels an active run or closes an overlay · skills are auto-discovered; name one as $skill · \
                 click a tool-group header to expand/collapse it",
            ));
        }
        SlashCommand::Custom { name, args } => {
            let matched = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES)
                .into_iter()
                .find(|c| c.name == name);
            match matched {
                Some(found) => match custom_commands::expand(&found.path, &args) {
                    Ok(expanded) => submit = Some(expanded),
                    Err(e) => state.transcript.push(TranscriptLine::new(
                        Gutter::Error,
                        format!("custom command '/{name}' failed to expand: {e}"),
                    )),
                },
                None => state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    format!("unknown command: /{name}"),
                )),
            }
        }
        SlashCommand::Unknown(cmd) => {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!("unknown command: {cmd}"),
            ));
        }
    }
    submit
}
