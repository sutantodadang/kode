# Kode

Local-first coding agent that thinks in your code graph, not just your files.

[![CI](https://github.com/sutantodadang/kode/actions/workflows/ci.yml/badge.svg)](https://github.com/sutantodadang/kode/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/sutantodadang/kode)](https://github.com/sutantodadang/kode/releases)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Sponsor](https://img.shields.io/badge/sponsor-%E2%9D%A4-ff69b4)](https://github.com/sponsors/sutantodadang)

Kode is a terminal coding agent written in Rust. Instead of grepping a repo and pasting whole files into the prompt, it asks a code knowledge graph ([zindeks](https://github.com/sutantodadang/zindeks)) for symbols, call graphs, and dependencies, recalls your team's engineering decisions from [Ingat](https://github.com/sutantodadang/Ingat), and verifies its own work with your project's real checks. Both engines run inside the Kode process: no daemon, no port, no account.

## Why Kode

- **Code-graph-first context.** Symbols, callers, and imports come from an index, not from dumping files into the model.
- **Persistent engineering memory.** Ingat remembers decisions, conventions, and known issues across sessions. Share selected memories with your team through a git-tracked file.
- **Local-first.** Your code, index, memory, and credentials stay on your machine. The only network call during a task is the model API you chose.
- **Honest verification.** Kode runs your tests, lint, and build after edits. A skipped check is reported as Skipped, never as Passed.
- **Local router.** An optional on-device model picks the model tier, reasoning effort, and whether to plan first, and reranks context before it reaches the model. Teams can fine-tune it on their own work.
- **Cheaper long sessions.** Prompt caching on Anthropic, OpenAI, Codex, and Antigravity, automatic compaction at 80% of the window, and bounded tool output.
- **Resumable sessions.** Every turn persists to `.kode/sessions/`. Pick up with `kode --continue` or `/resume`.
- **Shareable receipts.** Every task leaves a ledger of what Kode consulted, changed and verified — paste it into a PR or commit trailers.
- **Extensible.** Project or user `SKILL.md` skills, custom `/name` slash commands, external MCP servers, and bounded sub-agent delegation with per-tier models.

## Supported providers

| Provider | Auth | Notes |
|---|---|---|
| `codex` | OAuth (PKCE, browser) | ChatGPT account |
| `anthropic` | API key, or OAuth via Claude Pro/Max | OAuth is EXPERIMENTAL |
| `antigravity` | Google OAuth (PKCE) | EXPERIMENTAL, Gemini models via Cloud Code Assist |
| `opencode-go`, `opencode`, `kilo`, `lmstudio` | API key | OpenAI-compatible gateways |
| `openai` | API key from `OPENAI_API_KEY` or `KODE_API_KEY` | Default `[model].provider` |

Details: [docs/howto-auth-providers.md](docs/howto-auth-providers.md).

## Install

Linux / macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.ps1 | iex
```

The installers check the archive against the release's published sha256 checksum and need no `sudo`/admin. Prebuilt binaries cover Linux x86_64/arm64 (glibc), macOS Apple Silicon, and Windows x86_64. Manual install and build from source: [docs/howto-install.md](docs/howto-install.md).

## Quick start

```bash
kode auth login codex        # authenticate with a provider
kode setup                   # download the pinned engine library (asks first)
kode doctor                  # confirm config, auth, engines, and git are healthy
kode index                   # index this repo once; the watcher keeps it fresh
kode                         # launch the TUI in the current repo
kode exec "explain the auth flow in this repo"     # one-shot task
kode exec --image screenshot.png "fix this UI bug" # attach an image
kode --continue              # resume your last session
```

New to Kode? Follow the [getting started tutorial](docs/tutorial-getting-started.md).

## Commands

| Command | Description |
|---|---|
| `kode` | Launch the interactive TUI (`-c` resumes the latest session) |
| `kode exec <TASK>` | Run one agentic task, with retry and verification |
| `kode auth login\|status\|logout` | Manage Kode's own credential store |
| `kode status` | Show provider, model, engines, and session state |
| `kode models` | List models for the current provider |
| `kode setup` | Download the pinned zindeks library and optional router models (consent-gated) |
| `kode doctor` | Diagnose config, auth, engines, router, git, and environment |
| `kode index` | Build or refresh the code index for this repo |
| `kode verify` | Run the project's verification pipeline |
| `kode remember <TEXT>` | Save an engineering memory (`--team` to share it via git) |
| `kode memory status\|import` | Inspect team memory, import a legacy Ingat export |
| `kode receipt` | Print a shareable receipt for a session (`--pr` comments it on the PR, `--trailer` prints git trailers) |
| `kode onboard` | Zero-token tour of the repo: code map, team decisions, where to start (`--explain` narrates with the model) |
| `kode router ...` | Inspect, correct, calibrate, train, and publish the local router |
| `kode update` | Self-update from the latest GitHub release (consent-gated) |

Every flag and TUI slash command: [docs/reference-cli.md](docs/reference-cli.md).

## Documentation

Full index: [docs/README.md](docs/README.md).

- **Tutorial:** [Getting started](docs/tutorial-getting-started.md)
- **How-to:** [Install](docs/howto-install.md) · [Auth providers](docs/howto-auth-providers.md) · [Resume sessions](docs/howto-resume-sessions.md) · [Custom commands](docs/howto-custom-commands.md) · [Skills](docs/howto-skills.md) · [Team memory](docs/howto-team-memory.md) · [Router training](docs/howto-router-training.md)
- **Reference:** [CLI](docs/reference-cli.md) · [Config](docs/reference-config.md)
- **Explanation:** [Architecture](docs/explanation-architecture.md) · [Licensing for organizations](docs/enterprise.md)

## Community

- Bugs, feature requests, and questions: [GitHub Issues](https://github.com/sutantodadang/kode/issues/new/choose)
- Security reports: privately, see [SECURITY.md](SECURITY.md)
- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md) and our [Code of Conduct](CODE_OF_CONDUCT.md)

## Sponsorship

If Kode saves you time, consider sponsoring via [GitHub Sponsors](https://github.com/sponsors/sutantodadang) or [Ko-fi](https://ko-fi.com/sutantodadang). Sponsorships fund engine integration work (zindeks, Ingat) and release maintenance.

## License

Kode is licensed [AGPL-3.0-only](LICENSE). Running unmodified Kode, including inside a company, carries no disclosure obligation. If you modify Kode and distribute it or offer it as a network service, AGPL requires you to share the source. A commercial license is available for organizations that cannot meet those terms: see [docs/enterprise.md](docs/enterprise.md) and [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md).
