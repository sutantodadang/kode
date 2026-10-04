# Config reference

## File location

Kode reads project config from:

```
<project_root>/.kode/config.toml
```

A missing file is not an error: Kode falls back to defaults. An unreadable or malformed file raises a config error and Kode will not proceed with bad settings silently.

Related paths, for context:

- `~/.kode/auth/`: credential store (`codex.json`, `anthropic.json`, `opencode.json`, etc.), `0600` on Unix. `~` is `USERPROFILE` on Windows, `HOME` elsewhere.
- `~/.kode/runtime/zindeks/<revision>/`: the pinned zindeks engine library installed by `kode setup`. Override for development builds with the `KODE_ZINDEKS_DYLIB` environment variable.
- `~/.kode/zindeks/` and `~/.kode/ingat/memory.sqlite3`: the code index and memory store (see `[zindeks]` and `[ingat]` below).
- `~/.kode/commands/` and `~/.kode/skills/`: user-global custom commands and skills.
- `~/.kode/bin/` (Unix) or `%LOCALAPPDATA%\kode\bin` (Windows): the `kode` binary itself, when installed with the one-line installer.

These are outside the project and never checked into a repo. Inside the project, `.kode/sessions/` and `.kode/router-log.jsonl` are personal and belong in `.gitignore`. `.kode/config.toml`, `.kode/commands/`, `.kode/skills/`, `.kode/memory/team.jsonl`, and `.kode/router/` are meant to be committed when you want to share them with your team.

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
| `watch` | bool | `true` | Runs the in-process engine's background poll watcher, so the index refreshes itself instead of via Kode's explicit post-task refresh. |
| `store_root` | path | `~/.kode/zindeks/` | Where the in-process engine keeps its index (isolated from any standalone zindeks index). |

**Watch semantics:** with `watch = true` (the default), the engine polls for filesystem changes on its own every 2 seconds, so Kode's pipeline skips its own post-task refresh call: the watcher already has it covered. With `watch = false`, Kode falls back to an explicit refresh after each task instead.

```toml
[zindeks]
enabled = true
watch = true
# store_root = "/custom/zindeks"
```

## `[ingat]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `true` | Whether Kode uses Ingat for engineering-memory context. |
| `store_path` | path | `~/.kode/ingat/memory.sqlite3` | SQLite store for Kode's native, in-process memory. |

```toml
[ingat]
enabled = true
# store_path = "/custom/memory.sqlite3"
```

## `[agent]`

| Key | Type | Default | Effect |
|---|---|---|---|
| `model_retries` | integer | `3` | Retries a transient model failure without consuming another agent iteration. Capped at 10; set to `0` to disable. A stream is retried only before any model delta, so partial responses and tool calls are never duplicated. |
| `model_retry_base_ms` | integer | `500` | Initial retry delay in milliseconds. Subsequent retries use exponential backoff, capped at 30 seconds, and remain immediately cancellable. |
| `max_context_tokens` | integer | `0` (auto) | Total model window Kode may use. Auto reads live provider metadata when available, then falls back conservatively. Set a non-zero value to override detection. |
| `context_budget_tokens` | integer | `0` (auto) | Repository knowledge budget. Auto uses roughly 10% of the resolved model window, bounded to 16k–64k (and never more than a quarter of small windows). |
| `history_budget_tokens` | integer | `0` (auto) | Verbatim recent-session budget. Auto uses roughly 30% of the resolved window, bounded to 6k–256k. Older turns remain stored in the session even when omitted from verbatim replay. |
| `auto_compact` | bool | `true` | At 80% of the usable window, ask the selected model for a structured continuation summary, then retain system/repository context, the newest task, and the latest tool protocol in bounded form. Failed compaction falls back to safe truncation instead of failing the task. |

Agent iterations and tool calls have no count limit. Legacy `max_iterations` and `max_tool_calls` keys are ignored; Esc or Ctrl+C cancels an active TUI run immediately.

```toml
[agent]
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
| `models` | table of tables | `{}` | Named model tiers selectable per delegation via `delegate_task`'s `model` argument. Empty means every child runs on the root session model. |

```toml
[agent.subagents]
enabled = true
max_result_chars = 12000
```

Sub-agents are bounded leaf workers, not independent roots. They share the workspace but receive no parent conversation history, cannot delegate again, cannot run arbitrary commands, and never receive external MCP tools. Read-only tasks cannot mutate. A writable task must declare narrower workspace-relative ownership; `write_file` and `apply_patch` are rejected outside that scope. Kode runs one child at a time so workspace writes cannot overlap, and parent cancellation also cancels the active child. The root remains responsible for integration and final verification.

### `[agent.subagents.models.<tier>]`

Tiered model routing: map a short tier name to any provider/model pair. The
root agent then passes `"model": "<tier>"` to `delegate_task` to run that
child on the tier's model instead of the root session model — mechanical
work on a cheap executor, judgment stays on the root. Tiers may mix
providers. A tier that fails to resolve at startup (bad provider, missing
auth) degrades to a startup note; delegations targeting it fail with the
available-tier list.

| Key | Type | Default | Effect |
|---|---|---|---|
| `provider` | string | none: required | Same provider names as `[model]`: `openai`, `anthropic`, `antigravity`, `codex`, `opencode-go`, `opencode`, `kilo`, `lmstudio`. |
| `model` | string | none: required | Model id on that provider. |

```toml
[agent.subagents.models.terra]
provider = "opencode-go"
model = "terra-executor"

[agent.subagents.models.luna]
provider = "kilo"
model = "luna-cheap"
```

Delegations that fail mid-run return an `activity_tail` (the child's last
tool activities) alongside the error; feed it back as `context` on a
re-delegate so the replacement resumes instead of re-exploring from zero.
The bundled `tiered-exec` skill documents the routing conventions.

## `[router]`

Local, in-process routing and context reranking. Runs Laya (a multilingual
decision model) and Qwen3-Reranker-0.6B on ONNX Runtime; install them with
`kode setup` (~4 GB). Without them Kode routes statically and says so.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | `false` = static routing, local models never loaded. Builds with no pinned models behave as `false` |
| `min_confidence` | `0.6` | per answer; below it the static value is used (`static: low confidence …`) |
| `graph_threshold` | `0.8` | minimum Laya confidence on `answer=graph` before Kode answers a structural question from the code graph with zero model tokens |
| `device` | `"auto"` | reranker device: `auto` \| `cpu` \| `directml` \| `cuda` \| `coreml`; a failing GPU falls back to CPU. Laya always runs on CPU (faster than GPU for its short sequences) |
| `rerank` | `true` | rerank up to 24 candidates (16 Ingat memories, 8 zindeks search hits) before budgeting; keeps the best 12 memories and 6 hits |
| `rerank_on_cpu` | `false` | the reranker is slow on CPU (~0.8 s per candidate); it is skipped there unless this is set (then max 10 candidates — raise `rerank_timeout_ms` to ~10000 as well) |
| `rerank_timeout_ms` | `2000` | past this, engine order is kept (`rerank: skipped: timeout`) |
| `log_text` | `false` | `.kode/router-log.jsonl` stores a sha256 of the task unless this is set |

### `[router.tiers]`

Maps the router's `light` / `standard` / `heavy` answer to a model tier name
from `[agent.subagents.models.<name>]`. An unmapped answer runs the root
`[model]`.

```toml
[router.tiers]
light = "luna"
heavy = "terra"
```

Effort: the router adjusts `[model].effort` per task only when you have set
it (proof the provider accepts effort). Plan: the router can turn plan mode
on for a task, never off, and only when someone can approve the plan (the
TUI, or `kode exec` on an interactive terminal — never piped/CI runs). Pin
everything with `enabled = false`.

Only tasks the model actually routed are written to `.kode/router-log.jsonl`;
static-only decisions (models missing, disabled, cancelled) are not logged.

### `[router.training]`

Team opt-in to improve the router from real tasks. Off by default because
it commits task text to the repo and adds one short model call per task.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | after each routed, completed task, ask the task's model for hindsight labels and append a record to `.kode/router/dataset.jsonl` |
| `hf_dataset` | `""` | private HF dataset repo for `kode router train --remote` |

Records whose text looks like a credential are never stored.

```toml
[router.training]
enabled = true
hf_dataset = "my-team/kode-router-data"
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

## `[verify]`

Verification is auto-detected by default, including Rust/Go/Node/Python subprojects in polyglot repositories. Lockfiles select pnpm, Yarn, Bun, or npm. Explicit `[[verify.steps]]` entries replace auto-detection.

| Key | Type | Default | Effect |
|---|---|---|---|
| `timeout_seconds` | integer | `600` | Default timeout for each verification step. |
| `fail_fast` | bool | `true` | Skip remaining steps after a required step fails. |
| `steps` | array of tables | `[]` | Explicit commands; when non-empty, disables auto-detection. |
| `targeted` | string | `"first"` | How graph-selected tests run: `"first"` runs them before the full test step and fails fast (a targeted failure reports the full suite Skipped, `targeted tests failed`); `"only"` runs only them and reports the full test step Skipped (`targeted mode`); `"off"` disables targeting. No covering tests found means the targeted step is Skipped (`no covering tests found`) and the full suite runs. |

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
| `theme` | string | `"dark"` | `"dark"` keeps the default palette; `"light"` swaps to darker, higher-contrast colors for light terminal backgrounds. Kode falls back to a 256-color palette automatically unless `COLORTERM` contains `truecolor`/`24bit` or `WT_SESSION` is set. Kode never sets the background; the terminal's own is used. |

```toml
[ui]
reduced_motion = false
theme = "dark"
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
watch = true
# store_root = "/custom/zindeks"

[ingat]
enabled = true
# store_path = "/custom/memory.sqlite3"

[agent]
model_retries = 3
model_retry_base_ms = 500
max_context_tokens = 0
context_budget_tokens = 0
history_budget_tokens = 0
auto_compact = true

[agent.subagents]
enabled = true
max_result_chars = 12000

[agent.subagents.models.terra]
provider = "opencode-go"
model = "terra-executor"

[agent.subagents.models.luna]
provider = "kilo"
model = "luna-cheap"

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
theme = "dark"
```

Every section is optional. Any key you omit falls back to the default listed above; `kode` writes back only the keys it changes (for example `/model` or `/effort` in the TUI), preserving everything else already in the file, including unknown keys from a future version.

## Related

- [reference-cli.md](./reference-cli.md): flags that override these values per run
- [howto-resume-sessions.md](./howto-resume-sessions.md): `history_budget_tokens` in practice
- [explanation-architecture.md](./explanation-architecture.md): why context and history are separate budgets
- [../README.md](../README.md)
