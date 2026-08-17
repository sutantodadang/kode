# Getting started with Kode

This tutorial takes you from a clean machine to your first agentic task in Kode. You will install the CLI, log in to a model provider, install Kode's engines, and run a real task in a repo. By step 2 you will see a live model list, and by step 4 you will watch Kode work.

## What you need

A terminal, a git repo you can experiment in, and network access for the install and OAuth login.

## Step 1: Install Kode

On Linux or macOS:

```
curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/master/install.sh | sh
```

On Windows (PowerShell):

```
powershell -c "irm https://raw.githubusercontent.com/sutantodadang/kode/master/install.ps1 | iex"
```

Verify the install:

```
kode --version
```

You should see a version string like `kode 0.1.0`. If the command is not found, see [howto-install.md](./howto-install.md) for PATH troubleshooting.

## Step 2: Log in to a provider

Kode talks to model providers through its own credential store, never through another tool's auth files. Log in to Codex (OAuth+PKCE, browser-based):

```
kode auth login codex
```

A browser window opens for the OAuth flow. Once you approve it, Kode writes a token to `~/.kode/auth/codex.json` and prints the live model list fetched from the backend, for example:

```
Logged in to codex.
Available models:
  gpt-5.6-sol
  gpt-5.6-sol-mini
```

This is the first working thing you see: real credentials, real models, no guesswork.

If you use an opencode-family provider instead (paste an API key), see [howto-auth-providers.md](./howto-auth-providers.md).

## Step 3: Install the engines and confirm health

Kode's intelligence comes from two engines: zindeks (code graph) and Ingat (engineering memory). Install them with consent:

```
kode setup
```

Kode prompts before downloading anything. Confirm, and it installs both engines to Kode's managed bin directory. Skip the prompts with `kode setup --yes` if you already trust the source.

Confirm everything is wired up:

```
kode doctor
```

`doctor` checks config, LLM auth, zindeks, Ingat, git, and environment, and reports each check as pass, fail, or skipped. A skipped check is never reported as passed: if zindeks isn't installed yet, you will see it called out honestly, not silently green.

## Step 4: Run the TUI on a real task

From inside a git repo, launch the interactive TUI:

```
kode
```

Type a task at the `›` prompt, for example "explain how the config loader works." Watch the knowledge band above the transcript: `Z` shows top graph facts pulled from zindeks, `I` shows recalled memory from Ingat, `G` shows git impact. These are only shown when Kode actually has real data behind them: never faked.

After the agent finishes, a verification stage runs (tests, lint, whatever the project defines) and reports results honestly: passed, failed, or skipped.

## Step 5: Run a task without the TUI

For scripted or CI use, run a task directly from the shell:

```
kode exec "add a doc comment to the config loader"
```

Add `--model` or `--effort minimal|low|medium|high|xhigh|max|ultra` to override the configured model or reasoning depth for that one run. Add `-c`/`--continue` to append the task to your latest session instead of starting fresh.

## What you built

You installed Kode, authenticated against a real provider, installed its engines, confirmed health with `doctor`, and ran a task both interactively and headlessly. Kode now has a working credential store, a working code graph, and a working memory service behind it.

## Related

- [reference-cli.md](./reference-cli.md): every command and flag
- [howto-resume-sessions.md](./howto-resume-sessions.md): continuing work across sessions
- [howto-auth-providers.md](./howto-auth-providers.md): provider login details
- [../README.md](../README.md)
