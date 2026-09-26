# Improve the router with your team's tasks

Kode's local router (Laya) decides each task's model tier, reasoning
effort, and whether to plan first. Out of the box it is a general model;
this guide makes it learn from your team.

## 1. Turn on collection

In `.kode/config.toml` (commit it):

```toml
[router.training]
enabled = true
```

After each routed task that completes, Kode asks the task's own model what
the router should have decided, knowing what happened, and appends a record
to `.kode/router/dataset.jsonl`. Commit that file like any other; Kode adds
a `.gitattributes` rule so branches merge it without conflicts.

## 2. Correct wrong labels

```bash
kode router status                 # counts and thresholds
kode router correct last tier=heavy plan=plan
```

In the TUI: `/router` shows the last record; `/router effort=low` fixes it.

## 3. Calibrate (from 50 labeled records)

```bash
kode router calibrate              # before/after accuracy and ECE
kode router calibrate --write      # saves .kode/router/team-model.json
```

Commit `team-model.json`; teammates' confidence numbers become honest right away.

## 4. Fine-tune (from 300 labeled records)

Local (needs `uv` and an NVIDIA GPU):

```bash
kode router train
```

Or on HF Jobs (needs the `hf` CLI, logged in):

```toml
[router.training]
hf_dataset = "my-team/kode-router-data"   # private dataset repo
```

```bash
kode router train --remote
```

The candidate is accepted only if it matches or beats the current model's
accuracy on a held-out split without worse calibration.

## 5. Share it

```bash
kode router publish <candidate> --to hf:my-team/kode-router      # private HF repo
kode router publish <candidate> --to path:D:/shared/kode-router   # shared drive
kode router publish <candidate> --to lfs:models/router            # Git LFS in this repo
```

Commit `.kode/router/team-model.json`. Teammates run `kode setup` (HF) or
`git lfs pull` (LFS); `kode doctor` shows which model is active.
