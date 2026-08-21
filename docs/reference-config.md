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
| `max_iterations` | integer | `80` | Upper bound on agent loop iterations per task, as a runaway guard. |
| `max_tool_calls` | integer | `100` | Upper bound on total tool calls per task. |
| `max_context_tokens` | integer | `100000` | Upper bound on the model's total context window Kode will fill, across system prompt, history, and compiled knowledge context. |
| `context_budget_tokens` | integer | `16000` | Token budget for knowledge context compiled each turn from zindeks (code graph) and Ingat (memory). This is what the TUI breadcrumb's `ctx X/Yk` meter tracks: it is not the model's context window, which is `max_context_tokens`. |
| `history_budget_tokens` | integer | `6000` | Token budget for replaying prior session turns when resuming (`--continue`/`-c`, `/resume`). Oldest turns are dropped first if history doesn't fit, with a truncation marker shown in the transcript. |

```toml
[agent]
max_iterations = 80
max_tool_calls = 100
max_context_tokens = 100000
context_budget_tokens = 16000
history_budget_tokens = 6000
```

## `[permissions]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `default` | string (`allow`\|`ask`\|`deny`) | `"ask"` | Default permission mode for tool actions the agent wants to take. `allow` runs without prompting, `ask` prompts each time, `deny` blocks. |

Note: the TOML key is `default`, not `default_mode` (the Rust field is renamed via `#[serde(rename = "default")]`).

```toml
[permissions]
default = "ask"
```

## `[ui]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `reduced_motion` | bool | `false` | When `true`, kills the TUI's motion set: spinner glyph animation (static frame instead), the knowledge-band evidence-row dim→normal fade, and the Ledger active-marker pulse. Streaming coalescing (buffering model output before it hits the transcript) stays active regardless — it's buffering, not motion. |

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
max_iterations = 80
max_tool_calls = 100
max_context_tokens = 100000
context_budget_tokens = 16000
history_budget_tokens = 6000

[permissions]
default = "ask"

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
