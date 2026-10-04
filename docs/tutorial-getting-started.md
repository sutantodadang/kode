# Getting started with Kode

This tutorial takes you from a clean machine to your first agentic task in Kode. You will install the CLI, log in to a model provider, install Kode's engines, and run a real task in a repo. By step 2 you will see a live model list, and by step 5 you will watch Kode work.

The fast path is to skip steps 2–4: run `kode` in your repo and let the setup cards walk you through provider, login, engine download, and indexing. Press `Esc` to skip any card. The CLI commands below remain the scripted alternative.

## What you need

A terminal, a git repo you can experiment in, and network access for the install and OAuth login.

## Step 1: Install Kode

On Linux or macOS:

```
curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.sh | sh
```

On Windows (PowerShell):

```
powershell -c "irm https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.ps1 | iex"
```

Verify the install:

```
kode --version
```

You should see a version string like `kode 0.5.6`. If the command is not found, see [howto-install.md](./howto-install.md) for PATH troubleshooting.

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

Kode's intelligence runs in-process: a pinned zindeks shared library (code graph) and Ingat's headless core (engineering memory). `kode setup` downloads the pinned zindeks library (checksum-verified) and initializes the native memory store:

```
kode setup
```

Kode prompts before downloading anything. Skip the prompt with `kode setup --yes` if you already trust the source.

Confirm everything is wired up:

```
kode doctor
```

`doctor` checks config, LLM auth, zindeks, Ingat, git, and environment, and reports each check as pass, fail, or skipped. A skipped check is never reported as passed: if the zindeks library isn't installed yet, you will see it called out honestly, not silently green.

`kode setup` also offers the optional local router models (about 4 GB). You can decline: Kode then routes statically and says so.

## Step 4: Index your repo

From inside the git repo you want to work in, build the code graph once:

```
kode index
```

Kode prints the number of files, symbols, and edges it indexed. Starting a task never creates the first index for you, so run this once per repository. After that, the engine's background watcher keeps the index current as files change.

## Step 5: Run the TUI on a real task

From inside the same repo, launch the interactive TUI:

```
kode
```

Type a task in the composer, for example "explain how the config loader works." Kode keeps three stable regions on screen: the scope rail tells you which repo, branch, and authority mode you are using; the transcript records what happened; and the contextual work surface shows the one thing that needs attention now.

When Kode gathers context, the work surface shows a compact `CONTEXT` receipt. Press `Ctrl+K` to inspect its evidence: `Z` is code-graph context from zindeks, `I` is recalled memory from Ingat, and `G` is git context. The receipt and evidence rows only appear when real data exists. During a tool call the same surface becomes a live tool receipt; after the run it becomes a completion or recovery receipt.

After the agent finishes, a verification stage runs (tests, lint, whatever the project defines) and reports results honestly: passed, failed, or skipped.

## Step 6: Run a task without the TUI

For scripted or CI use, run a task directly from the shell:

```
kode exec "add a doc comment to the config loader"
```

Add `--model` or `--effort minimal|low|medium|high|xhigh|max|ultra` to override the configured model or reasoning depth for that one run. Add `-c`/`--continue` to append the task to your latest session instead of starting fresh.

To include a screenshot, use `kode exec --image screenshot.png "explain this error"`. In the TUI, run `/image <path>` or paste/drag the image path into the composer; the two newest attachments stay visible and `Ctrl+A` opens the complete attachment inspector.

## What you built

You installed Kode, authenticated against a real provider, installed its engines, confirmed health with `doctor`, indexed a repo, and ran a task both interactively and headlessly. Kode now has a working credential store, a working code graph, and a working memory store behind it, all running in-process on your machine.

## Related

- [reference-cli.md](./reference-cli.md): every command and flag
- [howto-resume-sessions.md](./howto-resume-sessions.md): continuing work across sessions
- [howto-auth-providers.md](./howto-auth-providers.md): provider login details
- [../README.md](../README.md)
