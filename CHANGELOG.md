# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added

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

## [0.1.0] - 2026-08-17

### Added

- TUI with slash commands and session resume.
- Agentic `exec` with retry and a verification pipeline.
- codex OAuth (PKCE) and opencode-family auth with a live model catalog.
- zindeks code-graph context with a file watcher.
- Ingat memory integration.
- Cross-platform support for Windows, Linux, and macOS.
- `kode doctor` and consent-gated `kode setup`.
