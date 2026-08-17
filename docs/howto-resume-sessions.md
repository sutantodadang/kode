# How to resume sessions

## Where sessions live

Kode writes completed turns to `.kode/sessions/<id>.jsonl`, one file per session, inside the repo you're working in. Each line is one completed turn: the task you gave and the agent's final answer. Kode does not persist tool call traffic (no file reads, no shell output, no intermediate tool results) and never writes credentials into a session file. Sessions are plain JSONL, so they're readable with any text tool and safe to `.gitignore`.

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

When resuming, Kode replays prior turns to the model under a token budget: `[agent] history_budget_tokens` in `.kode/config.toml`, default `6000`. If the full history doesn't fit, Kode drops the oldest turns first and shows an honest truncation marker in the transcript: it never silently pretends the model saw turns it didn't. Raise this value in config if you need more history replayed; see [reference-config.md](./reference-config.md).

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
