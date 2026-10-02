# Kode documentation

Docs are organized by what you are trying to do, following the [Diátaxis](https://diataxis.fr/) model.

## Tutorial: learn by doing

| Doc | You will |
|---|---|
| [Getting started](tutorial-getting-started.md) | Install Kode, log in, set up the engines, and run your first task in the TUI and headless |

## How-to guides: solve a specific problem

| Doc | Covers |
|---|---|
| [Install](howto-install.md) | One-line installers, manual binaries, build from source, PATH and platform troubleshooting |
| [Auth providers](howto-auth-providers.md) | Logging in to codex, anthropic, antigravity, and opencode-family providers; switching provider and model |
| [Resume sessions](howto-resume-sessions.md) | Continuing work with `--continue` and `/resume`; history budget; deleting sessions |
| [Custom commands](howto-custom-commands.md) | Writing your own `/name` slash commands with `$ARGUMENTS` |
| [Skills](howto-skills.md) | Creating progressively loaded `SKILL.md` packages |
| [Team memory](howto-team-memory.md) | Sharing engineering memory with your team through git |
| [Router training](howto-router-training.md) | Calibrating and fine-tuning the local router on your team's tasks |

## Reference: look something up

| Doc | Covers |
|---|---|
| [CLI](reference-cli.md) | Every command, flag, TUI slash command, and keybinding |
| [Config](reference-config.md) | Every `.kode/config.toml` section and key, with defaults |

## Explanation: understand the design

| Doc | Covers |
|---|---|
| [Architecture](explanation-architecture.md) | How the pipeline, engines, router, agent loop, and verification fit together, and the trade-offs behind them |
| [Kode for organizations](enterprise.md) | AGPL obligations, the commercial license, and Kode's security posture |

## Project

- [Changelog](../CHANGELOG.md)
- [Contributing](../CONTRIBUTING.md)
- [Security policy](../SECURITY.md)
- [Support](../SUPPORT.md)
- [Code of Conduct](../CODE_OF_CONDUCT.md)
- [TUI design system](../DESIGN.md) (for contributors changing the interface)
