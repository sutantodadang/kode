# Contributing to Kode

## Prerequisites

- Rust, stable channel
- git

## Build

```bash
cargo build
```

## Before you commit

These three checks must be clean before any commit:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

## Workspace layout

| Crate | Purpose |
|---|---|
| `kode-core` | Config, events, errors |
| `kode-model` | Provider clients |
| `kode-tools` | Sandboxed tool implementations |
| `kode-agent` | Agent loop |
| `kode-intel` | zindeks adapter |
| `kode-context` | Context compiler |
| `kode-memory` | Ingat adapter |
| `kode-verify` | Verification pipeline |
| `kode-mcp` | MCP client |
| `kode` (bin) | CLI and TUI |

## Conventions

- The task pipeline (`crates/kode/src/pipeline.rs`) communicates only via `KodeEvent`. No prints inside the pipeline.
- Core crates never depend on ratatui. TUI code lives only in the `kode` bin.
- Providers read credentials only from `~/.kode/auth/`, never from other tools' auth files.
- Read `DESIGN.md` before making any TUI change.

## PR process

- Branch from `main`.
- Keep PRs small and focused on one change.
- CI must be green before merge.
- No DCO-style sign-off is required.

## Licensing note

Contributions are accepted under AGPL-3.0. By submitting a contribution, you agree the maintainer may also license your contribution commercially, as part of Kode's dual-licensing model. This lets Kode stay free under AGPL while also being available under a commercial license for organizations that need one, without requiring a separate signed CLA for every contributor.
