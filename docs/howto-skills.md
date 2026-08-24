# Use skills

Skills are reusable instruction packages built around a `SKILL.md` file. Kode discovers their metadata at task start and loads the full instructions only when the user names a skill or the task clearly matches its description.

## Create a project skill

Create `.kode/skills/review/SKILL.md`:

```markdown
---
name: review
description: Review a change for correctness, regressions, and missing tests.
---

# Review

Read the diff, report findings by severity, and cite file locations.
```

Then name it in a task:

```text
$review check my current changes
```

Kode calls the read-only `use_skill` tool before following the instructions. A skill can reference files such as `references/checklist.md`, `scripts/check.ps1`, or `templates/report.md`; Kode reads those resources through the same tool with paths restricted to the skill directory.

## Discovery order

Earlier locations override later skills with the same case-insensitive name:

1. `<project>/.kode/skills`
2. `<project>/.agents/skills`
3. `<project>/.codex/skills`
4. `<project>/.claude/skills`
5. The same four directories under the user home directory

Discovery searches nested skill directories for `SKILL.md`. The catalog injected into model context contains at most 100 names and descriptions, with descriptions clipped to 140 characters. Skill bodies remain out of context until selected.

## Safety

- Skill resource paths cannot be absolute or contain `..`.
- Canonical resource paths must remain inside the selected skill directory.
- Individual skill files are limited to 1 MiB.
- Reading a skill is read-only. Any later mutating tool call still follows the configured permission mode.
- User instructions take precedence over skill instructions.
