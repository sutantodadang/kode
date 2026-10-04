# CLI reference

## Synopsis

```
kode [OPTIONS] [COMMAND]
```

Running `kode` with no command launches the interactive TUI.

## Root options

| Flag | Description |
|---|---|
| `-v`, `-vv` | Increase log verbosity: `-v` for info, `-vv` for debug. Applies globally, including to subcommands. |
| `-c`, `--continue` | Resume the latest session. With no subcommand, restores the transcript and history into the TUI. |
| `-V`, `--version` | Print the version and exit. |
| `-h`, `--help` | Print help and exit. |

Example:

```
kode -c
kode -vv
```

## `kode` (no subcommand)

Launches the interactive TUI in the current directory. Add `-c`/`--continue` to restore the latest session's transcript and history first.

```
kode
kode --continue
```

## `kode auth`

Manage Kode's own credential store for `codex`, `anthropic`, `antigravity`, and opencode-family providers (`opencode-go`, `opencode`, `kilo`, `lmstudio`). Credentials are stored under `~/.kode/auth/`, never read from another tool's auth files.

### `kode auth login <provider>`

Log in to a provider. `codex` uses OAuth+PKCE via your browser; the opencode-family providers prompt you to paste an API key; `anthropic` lets you choose between an API key (default) and OAuth via a Claude Pro/Max subscription (EXPERIMENTAL); `antigravity` uses Google OAuth+PKCE via your browser (EXPERIMENTAL) — see [howto-auth-providers.md](./howto-auth-providers.md).

```
kode auth login codex
kode auth login anthropic
kode auth login antigravity
kode auth login opencode
```

### `kode auth status`

Show which providers currently have stored credentials.

```
kode auth status
```

### `kode auth logout <provider>`

Remove stored credentials for a provider.

```
kode auth logout codex
```

## `kode status`

Show Kode's current status for the repo in the current directory (provider/model, engine availability, session state).

```
kode status
```

## `kode exec <TASK>`

Run an agentic task against the configured model, non-interactively.

| Flag | Description |
|---|---|
| `--model <MODEL>` | Override the configured model for this run only. |
| `--effort <EFFORT>` | Override reasoning effort for this run only. One of: `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`. |
| `-c`, `--continue` | Send prior session turns as history and append this task to that session, instead of starting fresh. |
| `--plan` | Plan first: the model produces a numbered plan (no tools) and Kode asks `execute this plan? [y/N]` before running the task. Answering `N` exits without running it. |
| `--no-graph-answer` | Always use the model, never answer a structural question from the code graph alone. Use it when a script needs a consistent output shape. |
| `--propose-memory` | After the task, draft a memory if a memorable moment occurred; print `◇ remember? "…"` to stderr. |
| `--save-memory` | Save the drafted memory (personal) without asking. Requires `--propose-memory`. |
| `--image <PATH>` | Attach a PNG, JPEG, GIF, or WebP image. Repeat the flag to attach more than one image. |

Examples:

```
kode exec "add a doc comment to the config loader"
kode exec --model gpt-5.6-sol --effort high "refactor the auth module for testability"
kode exec -c "now add tests for that refactor"
kode exec --plan "add pagination to the search endpoint"
kode exec --image screenshot.png "fix the layout shown here"
kode exec --image before.png --image after.png "compare these screens"
```

`TASK` may also be a custom slash command (`/name [args]`) — see [howto-custom-commands.md](./howto-custom-commands.md). It's expanded from its `.md` template before the task runs; an unrecognized `/name` fails with an error listing the commands discovered in `.kode/commands/` and `~/.kode/commands/`.

Every `kode exec` run saves its turn to `.kode/sessions/` (previously only with `-c`/`--continue`), so `kode receipt` and session history work after headless and CI runs.

When the local router is confident a prompt is a pure structural lookup (where something is defined, who calls it, what it calls, what depends on it, how the repo is organized), Kode answers from the code graph with zero model tokens: facts print as plain lines and a final `graph answer · 0 tokens` line. In the TUI the same answer shows `╰▶ graph · <kind> · 0 tokens · Nms`, and Enter re-asks the model with the graph answer included as context. `--no-graph-answer` forces the model path.

```
kode exec "/review the auth module"
```

## `kode models`

List available models for the currently configured provider, fetched live from the backend where supported.

```
kode models
```

## `kode verify`

Detect the current project's type and run its verification pipeline (tests, lint, build, whatever applies). Skipped checks are reported as Skipped, never as Passed.

```
kode verify
```

## `kode index`

Build or refresh the code-intelligence index for the repository in the current directory, using the embedded zindeks engine, then print file, symbol, and edge counts. Run it once per repository: starting a task never creates a first index on its own. After that, the engine's watcher (`[zindeks] watch = true`) keeps the index current. Fails with a clear error if `[zindeks] enabled = false`.

```
kode index
```

## `kode doctor`

Run diagnostic checks across config, LLM auth, zindeks, Ingat, git, and environment. Useful right after install or when something feels wrong.

The **Local router** section checks `router.device`, each `[router.tiers]`
mapping, the ONNX Runtime install, and every pinned model file.

```
kode doctor
```

## `kode router`

| Command | Does |
|---|---|
| `kode router status` | dataset counts, splits, thresholds (calibrate 50, train 300), active team model |
| `kode router correct <id\|last> key=value…` | fix a record's labels (`tier`, `effort`, `plan`); a correction beats the teacher |
| `kode router calibrate [--write]` | fit the pinned model's temperatures on the team dataset (Rust, CPU); `--write` saves `.kode/router/team-model.json` |
| `kode router train [--remote]` | fine-tune Laya (`uv`, CUDA GPU; or HF Jobs with `--remote`), calibrate, and gate against the current model |
| `kode router publish <id> --to hf:<repo>\|path:<dir>\|lfs:<dir>` | share a candidate that passed the gate and write the team manifest |

In the TUI, `/router` shows the last record and `/router tier=heavy` corrects it.

## `kode setup`

Install the pinned zindeks shared library and initialize the native memory store. Consent-gated: prompts before downloading anything unless `--yes` is passed.

| Flag | Description |
|---|---|
| `--yes` | Skip confirmation prompts and proceed with all installs. |

```
kode setup
kode setup --yes
```

`kode setup` also offers the local router artefacts (~4 GB): ONNX Runtime
1.22.0 for your platform (DirectML on Windows, CUDA on Linux when `nvidia-smi`
is present, CoreML on macOS) plus the Laya and Qwen3-Reranker models, into
`~/.kode/`. Every file is sha256-verified; a failed download leaves nothing
behind. Declining keeps static routing.

## `kode update`

Self-update the `kode` binary from the latest GitHub release. Consent-gated: prompts before downloading anything unless `--yes` is passed. Downloads the release archive and its `.sha256` sidecar, verifies the checksum, extracts with `tar`, and replaces the running binary (a restart is needed to use the new version).

| Flag | Description |
|---|---|
| `--yes` | Skip the confirmation prompt and proceed with the update. |

```
kode update
kode update --yes
```

## `kode remember <TEXT>`

Save an explicit engineering memory to Ingat.

| Flag | Description |
|---|---|
| `--kind <KIND>` | Memory kind. One of: `project-rule` (default), `architecture-decision`, `convention`, `known-issue`, `build-knowledge`, `rejected-approach`, `user-preference`, `historical-solution`. |
| `--tag <TAG>` | Tag to attach; repeat the flag to attach multiple tags. |
| `--team` | Also share this memory with the team by appending it to the git-backed `.kode/memory/team.jsonl` file, in addition to the normal (personal) Ingat write. See [howto-team-memory.md](./howto-team-memory.md). |

Examples:

```
kode remember "always run cargo fmt before committing"
kode remember "chose AGPL over MIT for copyleft protection" --kind architecture-decision --tag licensing
kode remember "staging deploys go through the release branch, not main" --kind convention --team
```

## `kode memory status`

Show the git-backed team-memory file's entry count and how many lines failed to parse (corrupt/skipped), for the repo in the current directory. See [howto-team-memory.md](./howto-team-memory.md).

```
kode memory status
```

## `kode memory import --from <PATH>`

Import a legacy `ingat_export` JSONL file (from the standalone Ingat service used before Kode 0.5) into Kode's native memory store. Idempotent by record id, so an interrupted import can be rerun to resume.

```
kode memory import --from ingat-export.jsonl
```

## TUI slash commands

Available inside the interactive `kode` TUI, with a live hint menu as you type `/`:

| Command | Description |
|---|---|
| `/model` | Switch the active model for the current provider. |
| `/effort` | Switch reasoning effort (`minimal`\|`low`\|`medium`\|`high`\|`xhigh`\|`max`\|`ultra`). |
| `/provider` | Switch the active provider. |
| `/copy` | Copy the last response or selection. |
| `/resume` | Open a picker over sessions in `.kode/sessions/` and resume one. |
| `/status` | Show the model, authority mode, and available context. |
| `/plan` | Toggle plan mode: the next task produces a plan first and waits for your approval. Session-only. |
| `/image <path>` | Attach a PNG, JPEG, GIF, or WebP image to the next message. You can also paste or drag an image path into the composer. |
| `/router [key=value…]` | Show the last local-router decision, or correct it (`/router tier=heavy`). See [howto-router-training.md](./howto-router-training.md). |
| `/map` | Show a zero-token repo map (totals, core orchestrators, hot symbols) from the code graph. |
| `/index` | Build or refresh the code index in the background; tasks submitted while it runs say `graph warming`. |
| `/why [N]` | Show where a turn's answer came from (route, graph/memory/git facts, changes, checks, cost) from its persisted ledger. Defaults to the last turn. |
| `/remember [--team] <text>` | Save an engineering memory directly; `--team` shares it via `.kode/memory/team.jsonl`. |
| `/help` | Show available commands and shortcuts. |
| `/exit` | Exit Kode. |
| `/name [args]` | Custom command — expands the `.kode/commands/name.md` or `~/.kode/commands/name.md` template and submits it as a task. See [howto-custom-commands.md](./howto-custom-commands.md). |

The scope rail at the top of the TUI shows the repo, branch, dirty state, authority mode (`BUILD`, `PLAN`, or `AUTO`), and model. Provider, effort, and token-budget detail remain available through status/config commands instead of occupying permanent chrome.

The contextual work surface changes with the run: context receipt, active tool or verification, permission decision, completion receipt, or recovery receipt. `Ctrl+K` expands real context evidence, `Ctrl+L` opens the full **Run Map**, `?` opens the shortcut sheet, and `Esc` closes an overlay. During a run, the first `Esc` arms interruption and the second interrupts it.

The composer label states what Enter will do (`ASK KODE`, `STEER ACTIVE RUN`, `QUEUE NEXT TASK`, or `ASK A FOLLOW-UP`). `Shift+Enter` inserts a newline. Images are validated by file content, shown as compact composer attachments, sent to the provider as native multimodal input, and retained when a session is resumed. The two newest text/image attachments are shown inline; `Ctrl+A` opens the complete attachment inspector. Each image is limited to 7 MiB; a turn may contain up to 20 images and 20 MiB total.

## Related

- [tutorial-getting-started.md](./tutorial-getting-started.md): commands in a real walkthrough
- [reference-config.md](./reference-config.md): config keys behind these flags
- [howto-auth-providers.md](./howto-auth-providers.md): `auth` subcommand in depth
- [howto-custom-commands.md](./howto-custom-commands.md): user-defined slash commands
- [../README.md](../README.md)
