# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project follows [Semantic Versioning](https://semver.org/) (pre-1.0:
minor versions may contain breaking changes).

## [0.5.0 – 0.5.6] - 2026-09-27 to 2026-10-01

### Added

- **Embedded engines.** zindeks and Ingat now run inside the Kode process:
  Kode loads a pinned, checksum-verified zindeks shared library and links
  Ingat's headless core. No child process, service, or port to manage.
  `kode setup` downloads the library; `kode doctor` reports a missing or
  mismatched engine. The old HTTP/stdio engine transports are gone; config
  keys written for them are ignored.
- `kode index`: build or refresh the code index for the current repo.
- `kode memory import --from <file>`: import a legacy `ingat_export` JSONL
  file into the native memory store (idempotent, resumable).
- **Local router.** Before each task, Laya (a multilingual decision model on
  ONNX Runtime) picks the model tier, reasoning effort, and whether to plan
  first, with a calibrated confidence and an honest static fallback. Qwen3-
  Reranker-0.6B reorders Ingat memories and zindeks hits before the context
  budget is applied. Configure with `[router]` and `[router.tiers]`; install
  the models with `kode setup` (~4 GB, optional).
- **Router training for teams.** `[router.training]` collects
  hindsight-labeled records; `kode router status|correct|calibrate|train|publish`
  and `/router` let a team calibrate and fine-tune its own router, gated
  against the current model on a held-out split. See
  [howto-router-training.md](./docs/howto-router-training.md).
- **Prompt caching across providers.** Anthropic cache breakpoints, OpenAI
  and Codex `prompt_cache_key`, and Antigravity cached-content usage. The
  system prefix stays stable between turns, so repeated turns cost less and
  start faster. Cached input tokens show in the TUI receipt and the `exec`
  summary.
- Long-run context hygiene: stale tool outputs are masked, every tool result
  is clipped head-and-tail at the runtime, `read_file` returns bounded
  windows with a next offset, and the budget is enforced before each request.
- Read-only tool batches run concurrently.
- Backends (engines, MCP servers) are reused across tasks in one session.
- Uncommitted files are passed to retrieval as the working set, so context
  favors what you are editing.
- `[ui] theme = "light"` for light terminal backgrounds, with automatic
  256-color fallback.
- Linux arm64 (`aarch64-unknown-linux-gnu`) release binaries and CI.
- zindeks engine pinned to v0.10.3.

### Fixed

- The first rerank no longer times out: the reranker warms up in the
  background.
- TUI: wrapped rows keep the thread gutter, prose is capped at 100 columns,
  and an overlong first word no longer leaves a blank row.
- Distinct cache keys for tasks started within one clock tick.

## [0.2.0 – 0.4.12] - 2026-08-19 to 2026-09-01 (not recorded at release)

### Added

- `antigravity` provider (EXPERIMENTAL): Google OAuth to Cloud Code Assist
  (Gemini models).
- `web_search` and `fetch_url` agent tools; tool failure reasons shown in
  the TUI.
- Progressively loaded skills (`SKILL.md` packages). See
  [howto-skills.md](./docs/howto-skills.md).
- Image attachments: `kode exec --image` and `/image` in the TUI (PNG,
  JPEG, GIF, WebP), plus bracketed paste with compact attachments.
- Bounded sub-agent delegation through the native `delegate_task` tool.

The `[Unreleased]` section below was written during the 0.2.0 to 0.4.12
releases and has shipped. It is kept as written. The Ingat service and its
`[ingat] autostart` key it mentions were superseded by the embedded engine
in 0.5.0.

## [Unreleased]

### Added

- Team memory (phase 1): `kode remember --team` / `RememberTool`'s `team:
  true` share a memory via the git-backed `.kode/memory/team.jsonl` wire
  file, in addition to the normal personal Ingat write. Every session start
  (TUI and `exec`) imports the file into local Ingat once Ingat's health
  check passes (`N new team memories` note); an Ingat build without the
  `/import` endpoint degrades to a "run `kode setup`" note instead of
  failing. `kode memory status` reports the file's entry/corrupt-line
  counts. See `docs/howto-team-memory.md`.
- One-command install: `scripts/install.sh` (`curl | sh`, Linux/macOS) and
  `scripts/install.ps1` (`irm | iex`, Windows) resolve the latest (or a
  pinned) GitHub release, verify the sha256 checksum, and install `kode` to
  `~/.kode/bin` / `%LOCALAPPDATA%\kode\bin` with no `sudo`/admin required.
- `kode update`: self-update from the latest GitHub release with consent
  prompt, sha256 verification, and in-place binary swap.
- Transcript provenance gutter: knowledge-derived lines show `Z` (zindeks),
  `I` (Ingat), or `G` (git) markers.
- Ingat memory confidence shown as a dim suffix in the knowledge band.
- Breadcrumb dirty-worktree indicator (`*` after the branch name).
- Ledger shows real `git diff --numstat` rows instead of a change counter.
- `[ui] reduced_motion` config to disable all TUI animation.
- Stream coalescing (word-boundary or 120ms flush) with a frozen spinner
  while tokens stream; evidence-row fade-in; ledger active-marker pulse.
- Transcript scrollbar (auto-hides when content fits) with proper scroll
  clamping, and mouse wheel scrolling (3 lines per notch). Terminal text
  selection is superseded by `/copy` while mouse capture is on.
- Ingat service autostart: when the memory service is unreachable, Kode
  starts the installed service once and retries (`[ingat] autostart`,
  default true). zindeks already autostarts via its stdio child.
- Plan mode: `/plan` in the TUI (breadcrumb `PLAN` badge) and `kode exec
  --plan` produce a numbered plan first (a tools-disabled model turn) and
  ask for approval before running the task; approving injects the plan into
  the task prompt, rejecting cancels cleanly with the plan left in the
  transcript. Session-only — never persisted to config.
- `anthropic` model provider: streaming Messages API client, API-key auth
  (default, `ANTHROPIC_API_KEY` env fallback) plus OAuth via a Claude
  Pro/Max subscription (EXPERIMENTAL, paste-back PKCE — not an officially
  supported third-party flow). Wired into `kode auth login|status|logout
  anthropic`, the task pipeline, the TUI `/provider` picker, the model
  catalog, and `kode doctor`.
- User-defined custom slash commands: markdown prompt templates discovered
  from `.kode/commands/*.md` (repo) and `~/.kode/commands/*.md`
  (user-global), expanded (with `$ARGUMENTS` substitution) into task
  prompts in both the TUI and `kode exec`. See
  [howto-custom-commands.md](./docs/howto-custom-commands.md).
- Tiered delegation: `[agent.subagents.models.<tier>]` maps a short tier
  name to any provider/model pair, and `delegate_task`'s new `model`
  argument runs that child on the tier's model instead of the root session
  model — mechanical work on a cheap executor, judgment on the root. Tiers
  may mix providers; unresolvable tiers degrade to a startup note. Bundled
  `tiered-exec` skill documents the routing conventions.
- Failed delegations now return an `activity_tail` (the child's last tool
  activities) alongside the error, so a re-delegate can resume from the
  partial progress instead of re-exploring from zero.

### Fixed

- Truncated model streams no longer stop the run: streams cut mid-generation
  (dropped connection before the finish event, or tool-call argument JSON
  truncated by an output-token limit) are retried like transient failures,
  including after partial output was already observed, and error messages
  now name the affected tool and byte count.
- Reasoning-heavy models no longer truncate mid-tool-call from a starved
  output budget: the `max_tokens` reserve now scales with the context window
  (up to 16,384; previously a flat 4,096), and a length-finish truncation
  escalates the budget on retry (doubled, capped at 32,768 and half the
  window) instead of regenerating an identical doomed request.
- OpenAI-compatible provider: reasoning-generation models (gpt-5*, o1/o3/o4*)
  now receive `max_completion_tokens` instead of the legacy `max_tokens`
  parameter they reject with a 400.
- Anthropic provider: `max_tokens` is clamped to the model's ceiling
  (8,192 for claude-3.5-generation models, 65,536 otherwise) so escalated
  retry budgets can't be rejected with a 400 before generating.
- Plan-quota 429s (`usage_limit_reached`-style messages) fail fast instead
  of burning the retry budget on 500ms–2s backoffs against a limit that
  resets in hours; per-minute rate limits still retry.
- `run_command` hang with detached grandchildren: `cmd /c start /b <server>`
  exits its parent immediately while the grandchild inherits the pipe
  handles, leaving the tool blocked forever in the output drain — past the
  timeout and past Esc. The timeout and cancellation guards now cover the
  drain, and the whole process tree is killed.

- CI on Linux/macOS: Windows-only Ingat setup helpers are now
  `#[cfg(windows)]`-gated instead of tripping `-D dead-code`.
- `run_command`/git tool subprocesses: timeout, cancellation, and drop now
  kill the whole process tree (Job Object on Windows, process-group `SIGKILL`
  on Unix) instead of only the direct child — a timed-out `cargo build` no
  longer orphans `rustc`. Spawned children also no longer inherit credential
  environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  `KODE_API_KEY`, and any `*_API_KEY`/`*_TOKEN`/`*_SECRET` variable).

## [0.1.0] - 2026-08-17

### Added

- TUI with slash commands and session resume.
- Agentic `exec` with retry and a verification pipeline.
- codex OAuth (PKCE) and opencode-family auth with a live model catalog.
- zindeks code-graph context with a file watcher.
- Ingat memory integration.
- Cross-platform support for Windows, Linux, and macOS.
- `kode doctor` and consent-gated `kode setup`.
