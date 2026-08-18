# How to write custom slash commands

## What they are

A custom command is a markdown file whose body is a prompt template. Drop `review.md` into a commands directory and `/review` becomes available in the TUI's `/` hint menu and as a `kode exec "/review ..."` task, expanding to the file's contents before the agent runs it.

## Where they live

| Location | Scope |
|---|---|
| `.kode/commands/*.md` | Repo-local — only available in this repo. |
| `~/.kode/commands/*.md` | User-global — available in every repo. |

Both directories are scanned fresh each time (on every TUI hint render and command invocation), so there's no reload step — add or edit a file and it's picked up immediately.

The command name is the file's stem, matched case-insensitively against `[a-z0-9-_]+`; files with other characters in the stem are skipped. `review.md` → `/review`, `fix-bug.md` → `/fix-bug`.

## Precedence

Builtin commands (`/model`, `/effort`, `/provider`, `/copy`, `/resume`, `/help`) are never shadowed — a custom command with one of those names is simply ignored. Otherwise, a repo-local command wins over a user-global one with the same name.

## Frontmatter

An optional YAML-ish frontmatter block at the top of the file sets the one-line description shown in the hint menu:

```markdown
---
description: Review a diff for correctness and style
---
Review the following as a senior engineer: $ARGUMENTS
```

Without frontmatter, the hint menu falls back to showing the command name itself. No other frontmatter keys are read.

## `$ARGUMENTS`

Everything typed after the command name is available as `$ARGUMENTS` in the template, substituted at every occurrence:

```markdown
Compare $ARGUMENTS against main and flag regressions.
```

`/diff-check the payments module` expands to:

```
Compare the payments module against main and flag regressions.
```

If the template has no `$ARGUMENTS` placeholder and you typed args anyway, they're appended to the end of the expanded prompt instead of being dropped.

## Example

`.kode/commands/review.md`:

```markdown
---
description: Review a diff carefully
---
Review the current diff for correctness bugs, then flag any
reuse/simplification opportunities. Focus area: $ARGUMENTS
```

```
/review the auth module
```

expands to:

```
Review the current diff for correctness bugs, then flag any
reuse/simplification opportunities. Focus area: the auth module
```

and runs as an ordinary task — same pipeline, same tool permissions, same transcript, as if you'd typed the expanded text directly.

## What's not supported

No positional arguments (`$1`, `$2`), no embedded bash execution, no nested command expansion, no YAML beyond the single `description:` key.

## Related

- [reference-cli.md](./reference-cli.md): the full slash-command table and `kode exec` usage
- [../README.md](../README.md)
