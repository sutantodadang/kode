# Contributing to Kode

Thanks for helping. Bug reports, docs fixes, and focused PRs are all welcome.

## Ways to contribute

- **Report a bug** with the [bug report template](https://github.com/sutantodadang/kode/issues/new?template=bug_report.yml). Include `kode --version` and the output of `kode doctor`.
- **Propose a feature** with the [feature request template](https://github.com/sutantodadang/kode/issues/new?template=feature_request.yml). For anything large, open an issue before writing code, so we can agree on the shape first.
- **Fix docs.** Docs live in [`docs/`](docs/README.md) and follow the Diátaxis split (tutorial, how-to, reference, explanation). A docs PR that corrects a stale command or flag is always welcome.
- **Security issues** go privately to the address in [SECURITY.md](SECURITY.md), never to a public issue.

## Prerequisites

- Rust stable, 1.85 or newer (the workspace uses edition 2024)
- git

## Build and run

```bash
git clone https://github.com/sutantodadang/kode.git
cd kode
cargo build
cargo run -p kode -- doctor
```

The embedded zindeks engine is a separate shared library. To exercise code-graph features from a dev build, run `cargo run -p kode -- setup` once, or point `KODE_ZINDEKS_DYLIB` at a locally built library.

## Before you commit

These three checks must be clean. CI runs the same ones on Linux, macOS, and Windows:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests that need real engine or model artifacts are marked `#[ignore]` and are opt-in (for example `cargo test -p kode --test native_engines -- --ignored` with `KODE_ZINDEKS_DYLIB` set).

## Workspace layout

| Crate | Purpose |
|---|---|
| `kode-core` | Config, events (`KodeEvent`), errors, cancellation |
| `kode-model` | Model provider clients and the model catalog |
| `kode-tools` | Sandboxed tool implementations |
| `kode-agent` | Agent loop, compaction, sub-agent delegation |
| `kode-intel` | zindeks adapter (embedded shared library over FFI) |
| `kode-context` | Context compiler and reranking hook |
| `kode-memory` | Ingat adapter (linked `ingat-core`) and memory policy |
| `kode-verify` | Verification pipeline |
| `kode-mcp` | External MCP client |
| `kode-local` | Local router: Laya and Qwen3-Reranker on ONNX Runtime, router training data and calibration |
| `kode` (bin) | CLI, TUI, task pipeline |

## Conventions

- The task pipeline (`crates/kode/src/pipeline.rs`) communicates only via `KodeEvent`. No prints inside the pipeline.
- Core crates never depend on ratatui. TUI code lives only in the `kode` bin.
- Providers read credentials only from `~/.kode/auth/`, never from other tools' auth files.
- Verification honesty: a skipped check is reported as Skipped, never as Passed.
- Read [`DESIGN.md`](DESIGN.md) before making any TUI change. It defines the layout, colors, glyphs, motion, and reduced-motion rules.
- User-facing change? Update the matching page in `docs/` and add a line to the `CHANGELOG.md` in the same PR.

## PR process

- Branch from `main`.
- Keep PRs small and focused on one change. Use [Conventional Commits](https://www.conventionalcommits.org/) for commit messages (`feat(tui): ...`, `fix(router): ...`, `docs: ...`).
- Fill in the PR template, including how you tested the change.
- CI must be green before merge.
- No DCO-style sign-off is required.

## Licensing note

Contributions are accepted under AGPL-3.0. By submitting a contribution, you agree the maintainer may also license your contribution commercially, as part of Kode's dual-licensing model. This lets Kode stay free under AGPL while also being available under a commercial license for organizations that need one, without requiring a separate signed CLA for every contributor.

## Code of Conduct

Everyone participating in this project is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
