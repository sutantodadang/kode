# Kode

Local-first, code-intelligence-first coding agent CLI (Rust workspace, 10 crates).
Engines: zindeks (code graph) + ingat (engineering memory) — first-class adapters,
never reimplemented in Kode.

## Design System
Always read DESIGN.md before making any visual or TUI decisions.
Palette, pane layout, glyph vocabulary, motion rules, and anti-slop constraints
are defined there. Do not deviate without explicit user approval.
Preview of every surface: `.kode-design-preview.html`.

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
