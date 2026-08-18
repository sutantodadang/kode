# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

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

### Fixed

- CI on Linux/macOS: Windows-only Ingat setup helpers are now
  `#[cfg(windows)]`-gated instead of tripping `-D dead-code`.

## [0.1.0] - 2026-08-17

### Added

- TUI with slash commands and session resume.
- Agentic `exec` with retry and a verification pipeline.
- codex OAuth (PKCE) and opencode-family auth with a live model catalog.
- zindeks code-graph context with a file watcher.
- Ingat memory integration.
- Cross-platform support for Windows, Linux, and macOS.
- `kode doctor` and consent-gated `kode setup`.
