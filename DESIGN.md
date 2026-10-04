---
# gstack: design-md-format=spec
name: "Kode Benang"
description: "A thread ledger: every fact the agent uses is visibly tied to the source it came from."
colors:
  background: "terminal default"
  primary-text: "#D8DEE5"
  muted-text: "#87919C"
  structure-rules: "#414953"
  code-graph: "#4FD1C5"
  engineering-memory: "#F2B84B"
  git: "#E0709B"
  tools: "#8C9BAB"
  verified-success: "#8BD450"
  warning-skipped: "#F2C14E"
  failure: "#FF5F56"
typography:
  terminal-font: "user-controlled monospace"
  preview-reference: "Martian Mono"
  preview-alternatives: ["Cascadia Mono", "JetBrains Mono"]
  scale: "one terminal cell"
  emphasis: "bold for YOU, the active step, and knots; uppercase for step names and authority labels only"
rounded:
  terminal: "0"
  preview-control: "5px"
spacing:
  narrow-horizontal-inset: "1 cell"
  wide-horizontal-inset: "1 cell"
  thread-gutter-wide: "7 cells"
  thread-gutter-narrow: "3 cells"
  minimum-transcript-height: "3 rows"
  maximum-composer-text-height: "6 rows"
  narrative-turn-gap: "1 row"
components:
  scope-row:
    content: "repo/branch and dirty marker, PLAN and AUTO authority, model when space permits, context meter"
    height: "1 row"
  step-rail:
    content: "UNDERSTAND, DECIDE, CHANGE, VERIFY (PLAN prepended in plan mode), token pulse, elapsed clock"
    height: "1 row plus 1 separator row"
  thread-ledger:
    content: "chronological turns; sourced facts knotted to graph, memory, or git threads; route decision; edits; verification"
    minimum-height: "3 rows"
  now-line:
    content: "what is happening, for how long, and what Enter and Esc will do"
    height: "1 separator row plus 1 row"
  composer:
    content: "editable wrapped prompt, attachments"
    maximum-text-height: "6 rows"
  picker:
    content: "filtered, viewport-scrolling list with visible selection and Esc cancel"
  permission:
    content: "exact action and scope; only supported allow/deny choices; composer paused"
  receipt:
    content: "changed files and verified, failed, skipped, or unreported checks; token counts"
---

# Kode Benang

Approved 30 September 2026. Interactive preview: `C:/Users/sutan/.gstack/projects/Kode/designs/benang-20260930/preview.html` (direction A plus the pulse row from direction B). This replaces Guided Conversation (28 September), preserved in `DESIGN.md.guided-20260928.bak`; the older Contextual Workbench is in `DESIGN.md.legacy.bak`.

## Product context

- **What this is:** Kode, a local-first coding agent CLI built on a code graph (zindeks) and engineering memory (ingat).
- **Who it is for:** engineers in small teams who need to trust what an agent did.
- **Peers:** Claude Code, Codex CLI, Pi, OpenCode, Crush, Gemini CLI.
- **Surface:** a ratatui terminal UI. The user owns the font and the background.

**The one thing to remember:** you can see how Kode thinks. Every other decision serves that. Clarity, a terminal that feels alive, and honest receipts follow from it; they are not separate goals.

**Why this differs from peers:** they render a linear transcript, a spinner with a verb, and a one-line footer. Their motion decorates waiting. Kode already emits facts the others do not have (`Knowledge`, `SourcedNote`, `RouterDecision`, `TaskProgress`, `VerifyStep`), so its motion carries those facts instead.

## Aesthetic direction

- **Direction:** industrial/utilitarian, a measuring instrument.
- **Decoration:** minimal. The only ornament is the thread gutter, and the threads are data.
- **Mood:** calm and auditable. The first reaction should be "it is showing its work", then "I can check that".

## Layout

80×24 while working:

```
 kode ⎇ feat/native-engines*  AUTO  sonnet-5.5           ctx ■■■□□□□□ 41k/96k
 UNDERSTAND ✓ ━━ DECIDE ✓ ━━ CHANGE ◐ ── VERIFY ·   ▁▁▁▃▅▇▆▅───       1:12
 ────────────────────────────────────────────────────────────────────────────
 YOU  fix the flaky retry in fetch_catalog

 g m t
 ●─┼─┼─ fetch_catalog ← 4 callers                                     graph
 │ ●─┼─ "30s timeout was tried, then reverted" · 12 Aug              memory
 │ │ ●─ 3 files co-change with catalog.rs                               git
 ╰─┴─┴▶ route sonnet · effort high   ■■■■■■■□□□ 0.71
 KODE
 │ │ │  fetch_catalog has 4 callers and fails when the catalog is slow...
 ●═╪═╪═ edit catalog.rs  +14 −3
 ┆ ┆ ┆  VERIFY  fmt ✓ 0.4s   clippy ✓ 1.6s   test ◐ 2.1s   lint ·
 ────────────────────────────────────────────────────────────────────────────
 ◐ tool: cargo test · 2.1s           Enter queue next · Esc cancel
 › _
```

Five regions persist, top to bottom: scope row, step rail, thread ledger, now-line, composer. No sidebar, no nested boxes, no cards, no logo art, no gradients.

- **Scope row.** Repo and branch with dirty marker, then `PLAN` and `AUTO` as independent authority labels (both stay visible when both are active), then the model, then a context meter on the right. The model is dropped before either authority label. The meter is 8 cells plus `used/budget`; below 80 columns it is numbers only. Before the first context compilation it reads `ctx —`, never `0`.
- **Step rail.** The steps are exactly the `TaskStep` values: `UNDERSTAND`, `DECIDE`, `CHANGE`, `VERIFY`, with `PLAN` prepended only in plan mode. A finished step shows `✓`, the active step is bold with the running glyph, a future step shows `·`. The connector between two steps turns from `──` to `━━` only when the earlier step is actually done. The token pulse sits between the rail and the elapsed clock. Below 80 columns the rail collapses to `3/4 CHANGE ◐` and the pulse takes the remaining width.
- **Thread ledger.** A chronological transcript with a permanent left gutter. At 80 columns or more the gutter is three threads, `g` (graph), `m` (memory), `t` (git), in seven cells. A sourced fact is a knot `●` on its own thread with a run `─` drawn to the text, and the source name right-aligned. A routing decision gathers the threads into `╰─┴─┴▶`. An edit is `●═╪═╪═`. Rows without a single source continue the threads as `│ │ │`; verification rows use `┆ ┆ ┆` because no source is being consulted. Below 80 columns the gutter is three cells: the source letter plus `●` for facts, `▶` for the route, `●═` for edits. The letter carries the meaning when color is unavailable. A line wider than the ledger wraps word by word and every wrapped row continues the threads (`│ │ │`, or `┆ ┆ ┆` for verification); bullets and the `YOU` row get a hanging indent. Model prose is capped at a 100-column reading measure.
- **Model prose stays at full brightness.** Only facts with exactly one source get a knot. Prose is never dimmed for being unsourced, and nothing is given a knot unless the pipeline attributed it.
- **`KODE` opens the reply.** Once per task, a bold `KODE` row precedes the model's first prose line, mirroring `YOU`. Later prose in the same task (after tools) has no second label.
- **Now-line.** One row that always answers three questions: what is happening, for how long, and what the next key does. The left side names one of five states: `model thinking`, `model writing`, `tool: <name>`, `waiting for you`, or the terminal state (`done · verified`, `stopped · verification failed`, `ready`). The right side states what Enter and Esc do in this exact state.
- **Composer.** One to six wrapped rows, then it scrolls around the cursor. Attachments show the newest two and an overflow count.
- **Small terminals.** At 60×20 the scope row, step rail, at least three ledger rows, the now-line, and the composer remain visible. Overlays scroll rather than hide their selected action.

Completed tools collapse to one-line receipts; the active or failed tool may expand. One blank row separates narrative turns; none inside a fact cluster. Permission, failure, and completion may take more rows because they need a decision or explain an outcome.

### Thread ledger additions (2026-10-04)

All reuse existing tokens and glyphs; no new colors.

| Element | Where | Rendering |
|---|---|---|
| Setup card | composer region, first run | picker component, `setup N/M · Enter to do it · Esc to skip` |
| Indexing row | ledger, `g` thread | `g ◐ indexing…` → `g ✓ indexed N files · Ns` / `g ✗ index failed: <reason>` |
| Repo map | ledger, `g` thread | `repo  N files · N symbols · N edges`, `core  <fan-out leaders>`, `hot   <fan-in leaders>`, closing `map · 0 tokens · Nms` |
| Graph answer receipt | ledger | `╰▶ graph · <kind> · 0 tokens · Nms` |
| Impact row | ledger, `g` thread, after the tool receipt | `<symbol> ← N callers, M crates, K tests` |
| Targeted test step | verify row | `test·targeted` label |
| Memory proposal | ledger, `m` thread, after the receipt | `◇` (proposed, unsaved), memory color; becomes `●` when saved |
| `/why` overlay | overlay | picker scrolling, ledger glyphs |

`◇` is the only new glyph: proposed, not yet saved. It becomes `●` when saved. Monochrome terminals keep meaning through the `m` letter and the `remember?` label.

## Color

Restrained. Color means provenance or outcome, never decoration. Less than 15% of visible cells carry semantic color. Every colored element also has a glyph or a text label, so monochrome terminals lose nothing.

| Token | Dark bg | ANSI 256 | Light bg | Meaning |
|---|---|---|---|---|
| primary-text | `#D8DEE5` | 253 | `#1F2328` | user and model text |
| muted-text | `#87919C` | 245 | `#6B7280` | metadata, hints |
| structure-rules | `#414953` | 238 | `#C9CED6` | rules, idle threads |
| code-graph | `#4FD1C5` | 80 | `#0E7C86` | zindeks facts |
| engineering-memory | `#F2B84B` | 214 | `#9A6700` | ingat facts |
| git | `#E0709B` | 168 | `#B4236A` | git facts |
| tools | `#8C9BAB` | 103 | `#57606A` | running glyph, tool pulse |
| verified-success | `#8BD450` | 113 | `#2F7D32` | `✓`, additions |
| warning-skipped | `#F2C14E` | 221 | `#8A6D00` | `⊘` skipped, `?` not reported, authority labels |
| failure | `#FF5F56` | 203 | `#C62828` | `✗`, deletions |

Git moved from green to rose. In the previous palette git and verified-success both fell on ANSI 108, so 256-color terminals could not tell "this came from git" from "this passed". Green now means verified and nothing else. The background is never set.

## Type and glyphs

Font and size belong to the terminal. Hierarchy uses cell position, weight, and case.

- **Bold:** `YOU`, `KODE`, the active step name, knots, the item being decided, check result glyphs.
- **Uppercase:** step names, `PLAN`, `AUTO`, `KODE`, and the block labels `VERIFY`, `RECEIPT`, `PERMISSION`, `FAILED`. Nothing else.
- **Never:** italics, or color as the only signal.

| Glyph | Meaning |
|---|---|
| `●` | fact with one known source (a knot) |
| `◇` | proposed, not yet saved (memory proposal; becomes `●` when saved) |
| `─ ┼ │` | thread run and idle threads |
| `╰ ┴ ▶` | threads converging into a decision |
| `═ ╪` | a change to the working tree |
| `┆` | thread not consulted (verification) |
| `■ □` | measured quantity: context used, router confidence |
| `◐ ◓ ◑ ◒` | something is running right now |
| `✓ ✗ ⊘ ?` | passed, failed, skipped, not reported |
| `▁▂▃▄▅▆▇` | tokens received in one second |
| `▸` | model text is streaming |
| `⏸` | waiting for the user |

Real-terminal QA must confirm these render at one cell in Windows Terminal with Cascadia Mono and in one Unix terminal. If `◐◓◑◒` does not, use `|/-\`; if `■□` does not, use `#.`.

## Motion

Intentional. Every moving cell is caused by a real event, and the event is named here. There is no decorative spinner, no verb carousel, no fabricated typing, no percentage.

| Motion | Trigger | Behavior |
|---|---|---|
| Thread pull | `SourcedNote`, `Knowledge` | The knot appears, the run draws toward the text in 3 frames of 40 ms, then the text lands whole. |
| Converge | `RouterDecision` | `╰─┴─┴▶` draws in 4 frames of 50 ms, then the confidence bar fills to the real value in steps of 30 ms. Below 0.5 the number is followed by `low` in warning color. With a static route the bar is omitted and the reason is shown as text. |
| Rail fill | `TaskProgress { done: true }` | The connector after the step turns `━━` in 2 frames of 90 ms. `DECIDE` is derived from the first `ToolStarted`, as today. |
| Running glyph | `ToolStarted`, `VerificationStarted`, model request with no tokens yet | `◐◓◑◒` at 250 ms, always beside a real elapsed counter. All running glyphs share one phase. |
| Trace back | `VerifyStep` | The glyph becomes `✓`, `✗`, or `⊘`. When the last check passes, the knots of this turn go bold for 300 ms. On failure they stay bold until the next turn is sent. |
| Pulse | `ModelToken`, `ToolStarted`, permission pending | One column per second. Height is tokens received in that second scaled to the run's peak; `─` in tool color when a tool ran; `·` in warning color while waiting for the user. |
| Stream | `ModelToken` | Text appears at word boundaries or within 120 ms. While text streams the now-line shows `▸` and no running glyph. |

Rules:

- Permission, failure, diff, attachment, and receipt are static.
- Nothing renders on a timer while idle. During a run, redraw on an event or on the shared 250 ms phase, not in a free loop, and keep the cached transcript rows.
- The UI ticks every 50 ms only while a run or an animation is in flight, so the frame timings above are rounded to that tick. Idle keeps the 100 ms tick.
- Tool output updates at most four times a second.
- `[ui].reduced_motion=true` shows every row in its final state at once, replaces the running glyph with a static `◐`, updates counters once a second, and keeps the pulse at one column per second. No meaning is lost.

## Input and command contract

Carried over unchanged from Guided Conversation; it is implemented and tested.

- Ordinary printable characters always enter the prompt; `q` is text. Exit uses an explicit command or a confirmed control.
- Left/Right, Home/End, Backspace/Delete, and Up/Down edit Unicode graphemes and move within wrapped or multiline input. At the top or bottom edge, Up/Down recalls history; PageUp/PageDown scroll the ledger.
- Enter sends while idle, steers the same run while steering is available, and queues the next turn during verification. Shift+Enter inserts a newline. Alt+Enter queues while a run is active. The now-line states which of these Enter will do.
- Esc cancels an active run immediately and keeps the draft. With an overlay open, Esc closes it; Ctrl+C cancels the run from any overlay. With an approval pending, Esc denies.
- `/` and Ctrl+P open the same filtered command discovery. `?` opens help. Commands are listed only when implemented.
- `@` discovers repository files when implemented. Images and long pastes remain compact attachments.

## Pickers, permission, and recovery

- **Provider and model** are one transaction: opening, filtering, and changing provider do not alter the active config; choosing a valid model saves both. Esc or a fetch failure keeps the active pair and the draft. No-match text is never accepted silently; `/model <name>` is the explicit custom path and says it is not catalog-validated. The selected row stays visible at any list position.
- **Permission** pauses the composer and shows `PERMISSION`, the exact action, the target or cwd, the consequence when known, and only the choices the backend supports, each with its key (`[A] Allow once`, `[D] Deny`). The now-line reads `⏸ waiting for you` with a counter and the pulse shows `·`.
- **Failure** stays visible while the user writes a follow-up. It names the first failing check, what was expected and found, and which changes are still in the working tree and unverified.
- **Completion** stays until the next task is sent. `RECEIPT` lists changed files with additions and deletions, each check as passed, failed, skipped, or not reported, and token counts. An unknown value reads `? not reported`, never `0` and never success.
- **Resume** replaces session context as a unit: ledger, model and scope, receipt or error, history, and draft never mix between sessions. An unsent draft is preserved or offered for recovery.

## Idle screen

The three threads show engine state instead of a logo: graph (symbols indexed, age of index), memory (note count), git (dirty file count). Below them: one line inviting a task, the most recent session for `/resume`, and the `/`, `@`, `?` entry points. An engine that is missing or degraded says so on its own thread in warning color with the command that fixes it.

## Acceptance

- At 60×20 the scope row, step position, at least three ledger rows, the now-line, and the composer are visible; the gutter is three cells.
- A knot appears only for events the pipeline attributed to exactly one source. Unsourced prose has no knot and is not dimmed.
- The rail connector fills only after `TaskProgress { done: true }`. The confidence bar equals the router's reported value; a static route shows its reason and no bar.
- Every running glyph has an elapsed counter beside it. The now-line distinguishes model thinking, model writing, tool, and waiting for the user.
- In ANSI 256 and in monochrome, graph, memory, git, passed, failed, skipped, and not reported are each distinguishable.
- With reduced motion, every state reads the same and nothing animates except counters at 1 Hz.
- CPU stays idle when no run is active, and redraw cost during a run does not grow with transcript length.
- All input, picker, permission, and resume tests from Guided Conversation still pass.
- Snapshot tests cover idle, working, permission, failure, and completion at 80×24 and 60×20, plus a real-terminal pass on Windows and Unix.

## Not in this design

**Symbol map pane** (direction B in the preview). It needs the pipeline to report which symbols or files each tool touched; today it reports only the tool name. Revisit when that event exists.

## Decisions log

| Date | Decision | Rationale |
|---|---|---|
| 2026-08-26 | Contextual Workbench | Source-specific evidence and honest receipts became Kode's identity. |
| 2026-09-28 | Guided Conversation | Clear task flow and a stable prompt; input, picker, permission, and resume contracts. |
| 2026-09-30 | Immediate run interruption | Esc cancels the active run; Ctrl+C remains available in overlays. |
| 2026-09-30 | Benang replaces Guided Conversation | Owner asked for clearer information, helpful animation, and a look unlike other agent CLIs. |
| 2026-09-30 | Permanent thread gutter | One-glance audit of where an answer came from; costs 7 cells and a short learning curve. |
| 2026-09-30 | Step rail always visible | Reverses the 28 September choice to hide it; the owner wants position always known. Costs one row. |
| 2026-09-30 | No decorative spinner; event-driven motion only | Motion becomes trustworthy. A quiet model call is covered by the elapsed counter and the pulse. |
| 2026-09-30 | Token pulse in the rail row | Shows real throughput and tells thinking, tool, and waiting apart; needs only existing events. |
| 2026-09-30 | Git color moved to rose; green reserved for verified | Git and success collided on ANSI 108. |
| 2026-09-30 | Model prose not dimmed | The outside design voice proposed dimming unsourced prose; rejected because readability was the first request. |
| 2026-09-30 | Symbol map deferred | Needs a pipeline event that does not exist yet. |
| 2026-09-30 | Input, picker, permission, resume contracts kept | Implemented and tested two days ago; the redesign is about information and identity. |
| 2026-10-01 | KODE reply label | Owner asked for the reply to stand out; a label mirrors YOU and keeps the no-box rule. |
| 2026-10-01 | Wrapped rows keep the gutter; prose measure 100 columns | Ratatui wrap dropped continuation rows to column 0, and 200-column prose lines were hard to read. |
| 2026-10-04 | First-run setup cards in the composer region | A fresh launch should reach an indexed repo without four CLI commands. |
| 2026-10-04 | Background indexing row + repo map on the `g` thread | Shows real progress and a zero-token repo map; no fabricated file counts. |
| 2026-10-04 | `◇` for proposed-but-unsaved memories | The one new glyph; monochrome keeps meaning via the `m` letter and `remember?` label. |
