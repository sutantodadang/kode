# How to share engineering memory with your team

## What it is

Ingat's engineering memory (project rules, conventions, decisions, known issues) is normally per-machine — only you benefit from what your agent has learned. Team memory shares specific memories with everyone who has access to the repo, using the repo itself as the transport: a git-tracked JSONL file, `.kode/memory/team.jsonl`. No server, no account, no sync step beyond `git add`/`commit`/`push`.

Sharing is always explicit. Nothing is shared automatically based on memory kind — you decide, per memory, with `--team` or `team: true`.

## Enable

There's nothing to turn on. If Ingat is enabled (`[ingat] enabled = true`, the default), team memory works as soon as you use `--team`. Ingat's core is built into Kode, so there is nothing extra to install.

## Share a memory

```
kode remember "staging deploys go through the release branch, not main" --kind convention --team
```

This does two things:

1. Writes the memory to your local Ingat, same as a normal `kode remember`.
2. Appends it as one JSON line to `.kode/memory/team.jsonl` in the repo root.

The agent's `remember` tool has the same option — pass `team: true` in the tool call. It's model-initiated, so the same [policy gate](../crates/kode-memory/src/policy.rs) that screens agent-inferred memories for secrets and hedged language runs first.

Commit the file like any other change:

```
git add .kode/memory/team.jsonl
git commit -m "share: staging release-branch convention"
git push
```

There's no auto-commit — Kode never runs `git commit` on your behalf.

## Import

Every session start (TUI launch, or `kode exec`), after Ingat's health check passes, Kode reads `.kode/memory/team.jsonl` and imports every entry into your local Ingat. Import is idempotent (upsert by id), so pulling a teammate's changes and starting a new session is enough — you'll see a note like:

```
◆ 3 new team memories
```

only when there's something new. Nothing to run by hand.

## `.gitattributes`

Because the file is append-only, concurrent edits from different branches almost never conflict — but when they do, a line-based union merge resolves it without manual intervention. Add this to `.gitattributes`:

```
.kode/memory/*.jsonl merge=union
```

## Check the file directly

```
kode memory status
```

prints the entry count and how many lines failed to parse (corrupt lines are skipped, never fatal — a corrupt entry from a bad merge doesn't block the rest of the import).

## Tour a repo with team memory

`kode onboard` (or `/onboard` in the TUI) gives a newcomer a zero-token tour: the code map, `start here` entry points, and team memories grouped as decisions, conventions, known issues, and rejected approaches. Corrupt lines are reported as a count, never hidden. `--explain` opts into one model-written narration.

## Privacy warning

`.kode/memory/team.jsonl` is a normal file in your repo — everyone with read access to the repository can see everything in it, including anyone browsing it on GitHub/GitLab/etc. Only use `--team` for things you're fine with the whole team (and anyone else with repo access) reading. The policy gate blocks obvious secrets (API keys, passwords, tokens) on agent-initiated writes, but it is not a substitute for judgment — nothing stops a human from typing a secret into `kode remember --team` directly.

Personal memories (the default, no `--team`) never touch this file and stay local to your machine.

## Proposed memories

Kode can notice a memorable moment and offer to save it, so you don't have to remember to run `kode remember`. Deterministic triggers decide when: a check failed then passed after a fix, an earlier change was reverted, the user steered the agent, or the same tool failed twice with the same error. Only then does Kode make one small model request to draft a sentence; the draft must pass the same policy gate and a near-duplicate check against Ingat.

In the TUI, after the turn's receipt, a `◇ remember? "…"` row appears:

- `Enter` saves a personal memory; `Tab` saves it as team memory (same `.kode/memory/team.jsonl` path as `--team`).
- `Ctrl+E` puts `/remember <draft>` in the composer for editing.
- `Esc` skips; typing a new prompt also skips.

Saved proposals are stored as user-approved (`explicit-user`) with the `kode:proposed` tag, and shown under `MEMORY` in `/why`. Configure with `[memory] propose` (default `true`); `kode exec` only proposes with `--propose-memory` (and saves with `--save-memory`).

## What's not built yet

Team memory currently only flows one direction into your local Ingat (import). There's no `kode export`, no hosted server, and no way to un-share a memory once it's committed — treat the file the way you'd treat any other committed text: removing a line from a future commit doesn't erase it from git history.

## Related

- [reference-cli.md](./reference-cli.md): `kode remember --team` and `kode memory status` flag reference
- [../README.md](../README.md)
