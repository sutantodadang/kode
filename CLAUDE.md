# Kode

Local-first, code-intelligence-first coding agent CLI (Rust workspace, 11 crates;
see the crate table in CONTRIBUTING.md). Engines: zindeks (code graph) + Ingat
(engineering memory) — first-class in-process adapters, never reimplemented in Kode.
User docs live in `docs/` (index: `docs/README.md`).

## Design System
Always read DESIGN.md before making any visual or TUI decisions.
The approved layout (thread ledger), provenance colors, glyphs, event-driven
motion, interaction states, and reduced-motion rules are defined there. Do not
deviate without explicit user approval. Flag TUI code that does not match it.

## Conventions
- Task pipeline (`crates/kode/src/pipeline.rs`) communicates ONLY via KodeEvent —
  no prints inside the pipeline.
- Core crates never depend on ratatui; TUI lives in the `kode` bin only.
- Verification honesty: a skipped check is Skipped, never Passed.
- Providers read credentials from Kode's own store (`~/.kode/auth/`) — never
  from other tools' auth files.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  and `cargo test --workspace` must be clean before any commit (CI runs the same).
- Sessions: completed turns persist to `.kode/sessions/` (JSONL, turn-level);
  resume via `kode --continue` or `/resume`.
- Engines are in-process: zindeks is a pinned shared library loaded over FFI
  (`~/.kode/runtime/zindeks/<rev>/`, dev override `KODE_ZINDEKS_DYLIB`); Ingat is
  the linked `ingat-core` crate. No spawned engine processes.
- Zindeks watch: with `[zindeks] watch = true` the engine's own 2s poll watcher
  refreshes the index, so the pipeline skips its explicit post-task refresh.
  First index is only created by `kode index`.
- User-facing change: update the matching `docs/` page and `CHANGELOG.md` in the
  same PR.
