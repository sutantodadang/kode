# Config reference

## File location

Kode reads project config from:

```
<project_root>/.kode/config.toml
```

A missing file is not an error: Kode falls back to defaults. An unreadable or malformed file raises a config error and Kode will not proceed with bad settings silently.

Related paths, for context:

- `~/.kode/auth/`: credential store (`codex.json`, `opencode.json`, etc.), `0600` on Unix. `USERPROFILE` on Windows, `HOME` elsewhere.
- `~/.kode/bin/` (Unix) or `%LOCALAPPDATA%\kode\bin` (Windows): managed engine binaries installed by `kode setup`.

Both are outside the project and never checked into a repo; `.kode/config.toml` and `.kode/sessions/` are per-project and usually belong in `.gitignore` unless you intend to share config with your team.

## `[model]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `provider` | string | `"openai"` | Active model provider id. Set via `/provider` in the TUI or by editing this key directly. |
| `model` | string | `""` (empty) | Active model name for the provider. Empty means Kode picks a default for that provider. Set via `/model` or `--model`. |
| `effort` | string | `""` (empty) | Reasoning effort. Empty means provider default. Valid values: `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`. Set via `/effort` or `--effort`. |

```toml
[model]
provider = "codex"
model = "gpt-5.6-sol"
effort = "high"
```

## `[zindeks]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `true` | Whether Kode uses zindeks for code-graph context at all. |
| `transport` | string | `"stdio"` | `"stdio"` spawns the binary named by `command` as a child process; `"tcp"` connects to `tcp_addr` instead. |
| `command` | string | `"zindeks"` | Binary spawned for stdio transport. |
| `tcp_addr` | string | `"127.0.0.1:7717"` | Address used when `transport = "tcp"`. |
| `watch` | bool | `true` | Enables zindeks's built-in poll-based file watcher (`ZINDEKS_WATCH=1`) on the spawned stdio child, so the index refreshes itself in the background. Only takes effect for `transport = "stdio"`: Kode doesn't control a TCP server's process, so it can't set its environment. |

**Watch semantics:** with `watch = true` (the default) and `transport = "stdio"`, the index polls for filesystem changes on its own every 2 seconds, so Kode's pipeline skips its own post-task refresh call: the watcher already has it covered. If `watch = false`, or the transport is `"tcp"` (Kode never controls a TCP server's watcher), Kode falls back to an explicit refresh after each task instead.

```toml
[zindeks]
enabled = true
transport = "stdio"
command = "zindeks"
tcp_addr = "127.0.0.1:7717"
watch = true
```

## `[ingat]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `true` | Whether Kode uses Ingat for engineering-memory context. |
| `url` | string | `"http://127.0.0.1:3200"` | Base URL of the Ingat REST API. |
| `autostart` | bool | `true` | When the Ingat service is unreachable at task start, automatically locate and start the installed service, then retry once before falling back to memory-less operation. At most one attempt per `kode` process. |

```toml
[ingat]
enabled = true
url = "http://127.0.0.1:3200"
autostart = true
```

## `[agent]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `max_tool_calls` | integer | `0` (unlimited) | Optional upper bound on total tool calls per task. Keep `0` for normal long-running work; set a non-zero value only when an external policy requires a hard cap. |
| `model_retries` | integer | `3` | Retries a transient model failure without consuming another agent iteration. Capped at 10; set to `0` to disable. A stream is retried only before any model delta, so partial responses and tool calls are never duplicated. |
| `model_retry_base_ms` | integer | `500` | Initial retry delay in milliseconds. Subsequent retries use exponential backoff, capped at 30 seconds, and remain immediately cancellable. |
| `max_context_tokens` | integer | `0` (auto) | Total model window Kode may use. Auto reads live provider metadata when available, then falls back conservatively. Set a non-zero value to override detection. |
| `context_budget_tokens` | integer | `0` (auto) | Repository knowledge budget. Auto uses roughly 10% of the resolved model window, bounded to 16k–64k (and never more than a quarter of small windows). |
| `history_budget_tokens` | integer | `0` (auto) | Verbatim recent-session budget. Auto uses roughly 30% of the resolved window, bounded to 6k–256k. Older turns remain stored in the session even when omitted from verbatim replay. |
| `auto_compact` | bool | `true` | At 80% of the usable window, ask the selected model for a structured continuation summary, then retain system/repository context, the newest task, and the latest tool protocol in bounded form. Failed compaction falls back to safe truncation instead of failing the task. |

```toml
[agent]
max_tool_calls = 0
model_retries = 3
model_retry_base_ms = 500
max_context_tokens = 0
context_budget_tokens = 0
history_budget_tokens = 0
auto_compact = true
```

## `[agent.subagents]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `true` | Exposes the native `delegate_task` tool to the root agent. Set `false` to disable delegation entirely. |
| `max_result_chars` | integer | `12000` | Maximum child summary size returned to the root. Values are clamped to 1,000–50,000 characters. |

```toml
[agent.subagents]
enabled = true
max_result_chars = 12000
```

Sub-agents are bounded leaf workers, not independent roots. They share the selected model and workspace but receive no parent conversation history, cannot delegate again, cannot run arbitrary commands, and never receive external MCP tools. Read-only tasks cannot mutate. A writable task must declare narrower workspace-relative ownership; `write_file` and `apply_patch` are rejected outside that scope. Kode runs one child at a time so workspace writes cannot overlap, and parent cancellation also cancels the active child. The root remains responsible for integration and final verification.

## `[permissions]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `default` | string (`allow`\|`ask`\|`deny`) | `"ask"` | Default permission mode for tool actions the agent wants to take. `allow` runs without prompting, `ask` prompts each time, `deny` blocks. |

Note: the TOML key is `default`, not `default_mode` (the Rust field is renamed via `#[serde(rename = "default")]`).

```toml
[permissions]
default = "ask"
```

## `[verify]`

Verification is auto-detected by default, including Rust/Go/Node/Python subprojects in polyglot repositories. Lockfiles select pnpm, Yarn, Bun, or npm. Explicit `[[verify.steps]]` entries replace auto-detection.

| Key | Type | Default | Effect |
|---|---|---|---|
| `timeout_seconds` | integer | `600` | Default timeout for each verification step. |
| `fail_fast` | bool | `true` | Skip remaining steps after a required step fails. |
| `steps` | array of tables | `[]` | Explicit commands; when non-empty, disables auto-detection. |

Each step accepts `name`, `command`, `args`, workspace-relative `cwd`, `required`, and an optional per-step `timeout_seconds` override.

```toml
[verify]
timeout_seconds = 600
fail_fast = true

[[verify.steps]]
name = "rust"
command = "cargo"
args = ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]
required = true

[[verify.steps]]
name = "frontend"
command = "pnpm"
args = ["test"]
cwd = "web"
timeout_seconds = 900
required = true
```

## `[ui]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `reduced_motion` | bool | `false` | When `true`, freezes the TUI's low-frequency spinner, evidence-row fade, and Run Map active-marker pulse. Streaming coalescing (buffering model output before it hits the transcript) stays active regardless — it is buffering, not motion. |

```toml
[ui]
reduced_motion = false
```

## `[mcp.servers.<name>]`

User-defined external MCP servers, distinct from the first-class `zindeks`/`ingat` integrations above. Each server is keyed by an arbitrary name under `[mcp.servers]`; its tools register into the tool runtime as `{server}__{tool}`.

| Key | Type | Default | Effect |
|---|---|---|---|
| `command` | string | none: required | Binary to spawn for this MCP server. No default; must be set per server. |
| `args` | array of strings | `[]` | Arguments passed to `command`. |
| `enabled` | bool | `true` | Whether this server is active. |

```toml
[mcp.servers.everything]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
enabled = true
```

## Complete annotated example

```toml
[model]
provider = "codex"
model = "gpt-5.6-sol"
effort = "high"

[zindeks]
enabled = true
transport = "stdio"
command = "zindeks"
tcp_addr = "127.0.0.1:7717"
watch = true

[ingat]
enabled = true
url = "http://127.0.0.1:3200"
autostart = true

[agent]
max_tool_calls = 0
model_retries = 3
model_retry_base_ms = 500
max_context_tokens = 0
context_budget_tokens = 0
history_budget_tokens = 0
auto_compact = true

[agent.subagents]
enabled = true
max_result_chars = 12000

[permissions]
default = "ask"

[verify]
timeout_seconds = 600
fail_fast = true

[mcp.servers.everything]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
enabled = true

[ui]
reduced_motion = false
```

Every section is optional. Any key you omit falls back to the default listed above; `kode` writes back only the keys it changes (for example `/model` or `/effort` in the TUI), preserving everything else already in the file, including unknown keys from a future version.

## Related

- [reference-cli.md](./reference-cli.md): flags that override these values per run
- [howto-resume-sessions.md](./howto-resume-sessions.md): `history_budget_tokens` in practice
- [explanation-architecture.md](./explanation-architecture.md): why context and history are separate budgets
- [../README.md](../README.md)
