# How to resume sessions

## Where sessions live

Kode writes completed turns to `.kode/sessions/<id>.jsonl`, one file per session, inside the repo you're working in. Each line is one completed turn: the task you gave and the agent's final answer. Kode does not persist tool call traffic (no file reads, no shell output, no intermediate tool results) and never writes credentials into a session file. Sessions are plain JSONL, so they're readable with any text tool and safe to `.gitignore`.

Each turn also stores a per-turn **ledger**: the facts the agent knew (zindeks/Ingat/git), the routing decision, changed files, verification results, and token usage. This is what `/why`, memory proposals, and `kode receipt` read. Sessions written before ledgers existed load unchanged, with an empty ledger for those turns.

## Resume the latest session

From the shell, before launching the TUI or `exec`:

```
kode --continue
```

or the short form:

```
kode -c
```

This restores the transcript and history from your most recent session in the current repo and launches the TUI with that context already loaded.

## Pick a specific session in the TUI

Inside the TUI, run:

```
/resume
```

This opens a picker over sessions found in `.kode/sessions/`, letting you resume any of them, not just the latest.

## Continue a session from `exec`

For scripted use, append a task to your latest session instead of starting fresh:

```
kode exec --continue "next task"
```

or:

```
kode exec -c "next task"
```

The prior turns are sent to the model as history, and the new task is appended once it completes.

## History budget

When resuming, Kode replays prior turns under `[agent] history_budget_tokens`. The default `0` is automatic: roughly 30% of the selected model's resolved context window, capped at 256k tokens. If the full history doesn't fit verbatim, Kode drops the oldest turns first and shows an honest truncation marker; the complete turns remain stored in the session file.

During a long active run, `auto_compact = true` triggers at 80% of the usable model window. Kode asks the selected model for a dense continuation summary, retains system/repository context and the newest task/tool round verbatim, and reports the estimated before/after size in the transcript.

This is a separate budget from `[agent] context_budget_tokens`, which governs how much knowledge-graph and memory context is compiled per turn, not session history.

## Deleting sessions

Sessions are plain files. To remove one, delete it:

```
rm .kode/sessions/<id>.jsonl
```

To clear all session history for a repo, delete the whole directory:

```
rm -rf .kode/sessions/
```

There is no separate "forget" command: the JSONL files are the entire state.

## Related

- [reference-cli.md](./reference-cli.md): `--continue`/`-c` flag reference
- [reference-config.md](./reference-config.md): `[agent] history_budget_tokens` and `context_budget_tokens`
- [tutorial-getting-started.md](./tutorial-getting-started.md)
- [../README.md](../README.md)
