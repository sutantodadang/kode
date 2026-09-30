# Kode

Local-first, code-intelligence-first coding agent CLI (Rust workspace, 10 crates).
Engines: zindeks (code graph) + ingat (engineering memory) — first-class adapters,
never reimplemented in Kode.

## Design System
Always read DESIGN.md before making any visual or TUI decisions.
The approved layout (thread ledger), provenance colors, glyphs, event-driven
motion, interaction states, and reduced-motion rules are defined there. Do not
deviate without explicit user approval. Flag TUI code that does not match it.
`docs/tui-redesign-2026-09-28.md` covers the input/picker/permission contracts
that DESIGN.md carries over; its visual sections are superseded.

## Conventions
- Task pipeline (`crates/kode/src/pipeline.rs`) communicates ONLY via KodeEvent —
  no prints inside the pipeline.
- Core crates never depend on ratatui; TUI lives in the `kode` bin only.
- Verification honesty: a skipped check is Skipped, never Passed.
- Providers read credentials from Kode's own store (`~/.kode/auth/`) — never
  from other tools' auth files.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and
  `cargo test --workspace` must be clean before any commit.
- Sessions: completed turns persist to `.kode/sessions/` (JSONL, turn-level);
  resume via `kode --continue` or `/resume`.
- Zindeks watch: spawned `zindeks serve` runs with `ZINDEKS_WATCH=1` (2s poll);
  pipeline skips explicit refresh unless watch is off or transport is TCP.
