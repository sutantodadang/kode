# Kode

Local-first coding agent that thinks in your code graph, not just your files.

[![CI](https://github.com/sutantodadang/kode/actions/workflows/ci.yml/badge.svg)](https://github.com/sutantodadang/kode/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/sutantodadang/kode)](https://github.com/sutantodadang/kode/releases)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Sponsor](https://img.shields.io/badge/sponsor-%E2%9D%A4-ff69b4)](https://github.com/sponsors/sutantodadang)

## Why Kode

- **Code-graph-first context.** Kode queries zindeks for symbols, call graphs, and dependencies instead of grepping and dumping whole files into the model.
- **Persistent engineering memory.** Ingat remembers decisions and context across sessions, so you don't re-explain your codebase every time.
- **Local-first.** Your code and credentials never leave your machine, except for the model API call itself.
- **Honest verification.** A skipped check is reported as Skipped, never as Passed.
- **Resumable sessions.** Every turn persists to disk. Pick up where you left off with `--continue` or `/resume`.
- **Provider choice.** Log in with codex via OAuth, or paste an API key for an opencode-family provider.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/main/install.sh | sh
```

```powershell
powershell -c "irm https://raw.githubusercontent.com/sutantodadang/kode/main/install.ps1 | iex"
```

Or grab a binary from [Releases](https://github.com/sutantodadang/kode/releases).

For manual install and build-from-source steps, see [docs/howto-install.md](docs/howto-install.md).

## Quick start

```bash
kode auth login codex        # authenticate with a provider
kode setup                   # install zindeks + Ingat, consent-gated
kode                         # launch the TUI
kode exec "explain the auth flow in this repo"   # one-shot agentic task
kode --continue               # resume your last session
```

## Commands

| Command | Description |
|---|---|
| `auth` | Login, check status, or logout of a provider |
| `status` | Show current provider, model, and session state |
| `exec <TASK>` | Run an agentic task once, with retry and verification |
| `models` | List available models for the current provider |
| `verify` | Run the verification pipeline against the workspace |
| `doctor` | Diagnose the local Kode install and engine health |
| `setup` | Consent-gated install of zindeks and Ingat |
| `update` | Consent-gated self-update from the latest GitHub release |
| `remember` | Write to engineering memory (Ingat) directly |

## Documentation

| Doc | Description |
|---|---|
| [Getting started](docs/tutorial-getting-started.md) | First run, first task, walkthrough |
| [Install](docs/howto-install.md) | Manual install and build-from-source |
| [Auth providers](docs/howto-auth-providers.md) | Setting up codex and opencode-family auth |
| [Resume sessions](docs/howto-resume-sessions.md) | How session persistence and resume work |
| [CLI reference](docs/reference-cli.md) | Full command and flag reference |
| [Config reference](docs/reference-config.md) | Configuration file and options |
| [Architecture](docs/explanation-architecture.md) | How the crates and engines fit together |
| [Enterprise](docs/enterprise.md) | AGPL obligations and commercial licensing |

## Sponsorship

If Kode saves you time, consider sponsoring via [GitHub Sponsors](https://github.com/sponsors/sutantodadang) or [Ko-fi](https://ko-fi.com/sutantodadang). Sponsorships fund engine integration work (zindeks, Ingat) and release maintenance.

## Enterprise

Kode is licensed AGPL-3.0. Under AGPL, running a modified version as a network service counts as distribution and requires source disclosure. If your company cannot comply with that, a commercial license is available. See [docs/enterprise.md](docs/enterprise.md) and [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md), or contact sutantodadang@gmail.com.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Before opening a PR, make sure `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` are clean.

## License

Kode is licensed [AGPL-3.0-only](LICENSE). A commercial license is also available, see [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md).
