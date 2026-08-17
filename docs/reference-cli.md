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

Manage Kode's own credential store for `codex` and opencode-family providers (`opencode-go`, `opencode`, `kilo`, `lmstudio`). Credentials are stored under `~/.kode/auth/`, never read from another tool's auth files.

### `kode auth login <provider>`

Log in to a provider. `codex` uses OAuth+PKCE via your browser; the opencode-family providers prompt you to paste an API key.

```
kode auth login codex
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

Examples:

```
kode exec "add a doc comment to the config loader"
kode exec --model gpt-5.6-sol --effort high "refactor the auth module for testability"
kode exec -c "now add tests for that refactor"
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

## `kode doctor`

Run diagnostic checks across config, LLM auth, zindeks, Ingat, git, and environment. Useful right after install or when something feels wrong.

```
kode doctor
```

## `kode setup`

Install or bootstrap the zindeks and Ingat engines. Consent-gated: prompts before downloading anything unless `--yes` is passed.

| Flag | Description |
|---|---|
| `--yes` | Skip confirmation prompts and proceed with all installs. |

```
kode setup
kode setup --yes
```

## `kode remember <TEXT>`

Save an explicit engineering memory to Ingat.

| Flag | Description |
|---|---|
| `--kind <KIND>` | Memory kind. One of: `project-rule` (default), `architecture-decision`, `convention`, `known-issue`, `build-knowledge`, `rejected-approach`, `user-preference`, `historical-solution`. |
| `--tag <TAG>` | Tag to attach; repeat the flag to attach multiple tags. |

Examples:

```
kode remember "always run cargo fmt before committing"
kode remember "chose AGPL over MIT for copyleft protection" --kind architecture-decision --tag licensing
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
| `/help` | Show available commands. |

The breadcrumb at the top of the TUI shows the current provider/model/effort and a context meter (`ctx X/Yk`), which tracks the knowledge-context budget (`[agent] context_budget_tokens`), not the model's context window.

## Related

- [tutorial-getting-started.md](./tutorial-getting-started.md): commands in a real walkthrough
- [reference-config.md](./reference-config.md): config keys behind these flags
- [howto-auth-providers.md](./howto-auth-providers.md): `auth` subcommand in depth
- [../README.md](../README.md)
