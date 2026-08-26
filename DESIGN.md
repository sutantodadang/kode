# Design System: Kode Contextual Workbench

Approved preview: `C:\Users\sutan\.gstack\projects\Kode\designs\design-system-20260826\contextual-workbench-preview.html`.

## Product Context

- **What this is:** Kode is a Rust/ratatui coding agent that compiles repository context from a code graph, engineering memory, and current git state before acting.
- **Who it is for:** Engineers who work in terminals and want to understand what an agent is doing, why it is doing it, and whether the result was actually verified.
- **Category:** Terminal developer tool and agentic coding interface.
- **Peers:** Claude Code, Codex CLI, OpenCode, Crush, Gemini CLI, and Aider.
- **Memorable behavior:** Intelligence becomes visible as evidence attached to decisions, not as a permanent telemetry dashboard.

## Visual Thesis

**Contextual Workbench:** a quiet engineering workbench where one active decision occupies the light, history recedes, and every consequential action leaves an inspectable receipt.

The first three seconds should communicate: "I know where I am, what Kode is doing, and what I can do next."

## Core Rule

**One state, one focus, one next action.**

Every persistent row must earn its screen cost. Context, tools, permissions, failures, and completion never compete as simultaneous panels.

## Stable Frame

Only three regions persist:

1. **Scope rail:** repository, branch/dirty state, working mode, and model.
2. **Contextual work surface:** changes shape around idle, understanding, tool execution, permission, failure, or completion.
3. **State-aware composer:** explicitly says whether Enter will send, steer, retry, review, or queue.

```text
 KODE / rust/Kode  main*                 BUILD · gpt-5.6-sol
 ───────────────────────────────────────────────────────────

 WORK SURFACE
 one active state, with relevant evidence and actions

 ───────────────────────────────────────────────────────────
 ASK KODE
 › _
 Enter send · Shift+Enter newline · ? shortcuts
```

The transcript remains the durable work log, but it is not the live focal surface while a run is active.

## State Compositions

### Idle

- Show repository orientation and only truthful engine/configuration signals.
- Offer two or three useful starting actions derived from real repository state.
- No centered logo art, tagline, fake readiness, or settings manual.
- Composer label: `ASK KODE`.

### Understanding

- Expand relevant code graph, memory, and git evidence in the work surface.
- Show compact real run progression: Understand, Decide, Change, Verify.
- Contract evidence to one seam after the active decision no longer needs it.
- Composer label: `STEER ACTIVE RUN`.

### Tool Execution

- One active tool receives the work surface.
- Show exact command/tool label, arguments, cwd, timeout when known, and elapsed time.
- Never fake a percentage. Unknown duration uses elapsed time.
- Completed tool bursts collapse into one-line receipts; failures expand automatically.
- Composer label: `STEER ACTIVE RUN`.

### Permission

- Permission becomes the primary decision surface, not a narrow bar between transcript and composer.
- Explain exact action, target/cwd, consequence when known, and decision scope.
- Only display choices the implementation truly supports.
- Conversation input is paused until the decision resolves.
- Composer label: `DECISION REQUIRED`.

### Failure

- Lead with the failed phase and first actionable error.
- State what changed so far and whether verification ran.
- Offer only real recovery actions; raw logs remain progressively disclosed.
- Composer label: `TELL KODE WHAT TO CHANGE` when retry through steering is possible, otherwise `ASK KODE`.

### Completion

- Completion is an auditable receipt, not a celebration.
- Show files changed, verification results, skipped checks, iterations, tool calls, and token usage before secondary prose.
- Use `DONE · VERIFIED`, `DONE · UNVERIFIED`, or `DONE · FAILED VERIFICATION` truthfully.
- Keep the receipt visible until the user begins the next task.
- Composer label: `REVIEW OR ASK A FOLLOW-UP`.

## Surface Contracts

### Scope Rail

Default form:

```text
 KODE / rust/Kode  main*                 BUILD · gpt-5.6-sol
```

- Always show repository, branch, and dirty state when available.
- Show `PLAN`, `BUILD`, or `AUTO` only when the mode changes agent authority.
- Provider, effort, and raw context counts belong in `/status` or relevant overlays, not permanent chrome.
- At narrow widths remove secondary model metadata before repository or mode.

### Evidence Seam

The permanent knowledge band is replaced by a compact receipt:

```text
 CONTEXT  7 code facts · 2 memories · 3 changed files   Ctrl+K
```

Expanded evidence uses words plus provenance glyphs:

```text
 CODE  auth::refresh_token ← 5 callers
 MEM   preserve the previous token on write failure · .86
 GIT   crates/kode/src/auth.rs modified
```

Evidence appears near the action or decision it explains. Never invent knowledge when an engine is disabled or a source is empty.

### Transcript Work Log

- One role label per block, not one label per wrapped line.
- User turns use bold `YOU`; agent prose uses a continuous dim rail or `KODE` anchor.
- Completed tools are quiet receipts. The active tool and failed tools expose details.
- Historical turns reduce intensity; current state remains primary.
- Preserve chronological ordering when steering interrupts streamed prose.

### Run Map

`Run Map` replaces the internal-facing name `Ledger` in user-visible copy.

- Compact progression appears in the work surface during a run.
- `Ctrl+L` opens objective, real progress, current changes, verification, and evidence.
- The scope rail remains visible while Run Map is open.
- `Esc` returns to the transcript.
- Never mark a decision complete based on a decorative proxy when no event proves it.

### Composer

- Labels are state-aware: `ASK KODE`, `STEER ACTIVE RUN`, `QUEUE NEXT TASK`, `DECISION REQUIRED`, or `REVIEW OR ASK A FOLLOW-UP`.
- `Shift+Enter` inserts a newline. `Enter` follows the visible label.
- During an active model/tool segment Enter steers the same run.
- During verification/finalization Enter queues the next turn.
- First Esc arms interruption; second Esc within two seconds interrupts.

### Attachments

- Images and long text are first-class rows with kind, filename, format, and size/count.
- Show the newest two rows, then `+N more`.
- Backspace with an empty composer removes the latest attachment.
- A discoverable attachment inspector shows the full stack.
- Binary/base64 data never renders in the composer or transcript.

### Shortcuts and Commands

- Show no more than three contextual actions beside the composer.
- Use action-first copy: `Enter send`, not `send Enter`.
- `?` opens the complete shortcut sheet.
- `/` opens/filter commands. Unknown commands never run the closest match silently.

## Typography

Kode cannot control the terminal font. The reference face is **JetBrains Mono**, with Berkeley Mono and Commit Mono as compatible alternatives.

- Active decisions, user input, and conclusions may be bold.
- Uppercase is reserved for short state labels.
- Metadata is muted and never bold.
- Do not depend on italics as the only carrier of meaning.
- Numeric metrics and elapsed times align consistently.
- Reference scale is one terminal cell; hierarchy comes from weight, case, spacing, and alignment rather than font size.

## Color

Background remains terminal-default. Color communicates provenance or consequence, never decoration.

- Primary text: `#D8DEE5`, ANSI 253
- Muted text: `#87919C`, ANSI 245
- Structure/rules: `#414953`, ANSI 238
- Code graph: `#63C5DA`, ANSI 80
- Engineering memory: `#D7A85B`, ANSI 179
- Git: `#8FAE8B`, ANSI 108
- Tools: `#8C9BAB`, ANSI 103
- Verified/success: `#74B88A`, ANSI 108
- Warning/skipped: `#F2C14E`, ANSI 221
- Failure: `#D16D72`, ANSI 167

Every colored meaning also has a glyph or word. The UI must remain understandable in monochrome. No more than 12–15% of visible cells should be colored.

## Glyph Vocabulary

- Roles: `YOU`, `KODE`, `│`
- Provenance: `CODE`, `MEM`, `GIT`, `TOOL`, `VERIFY`
- Compact source glyphs where width is constrained: `Z I G T V`
- State: `● ○ ✓ × –`
- Disclosure: `▸ ▾`
- Relationships: `→ ├─ └─ │`
- Input: `›`
- Diff: `+ -`

ASCII fallbacks: `| > v * o x - +`. No emoji in core UI.

## Spacing and Density

- Density: compact-comfortable.
- Horizontal inset: 2 cells at 80+ columns, 1 cell below 80.
- Transcript gutter: 4 cells.
- One blank row between narrative turns.
- No blank row inside one tool, evidence, permission, or receipt cluster.
- Composer grows to six text rows; attachments consume at most three additional rows.
- Show newest two attachment rows plus an overflow count.
- Essential behavior must remain usable at `60×20`; enhanced composition begins at `100×30`.

## Motion

- Minimal-functional only.
- Stream word/sentence chunks with a 120 ms maximum buffering window.
- Only one region moves at a time.
- Active heartbeat is 1 Hz, not 4 Hz.
- Tool output updates at most four times per second.
- New evidence may settle from dim to normal once over 200–300 ms.
- Permission, failure, diff, attachment, and completion never animate.
- `[ui] reduced_motion = true` freezes all spinner and transition states.

## Anti-Slop

No gradients, chat bubbles, card grids, permanent sidebar, decorative full-screen backgrounds, fake thinking, fake progress, provenance without real data, success color for skipped verification, or borders around every section.

## Acceptance Tests

- **Squint:** only one region is visually dominant.
- **Five-second:** the user can state what Kode is doing and what they can do next.
- **Monochrome:** provenance and outcomes remain distinguishable.
- **60-column:** no essential action or consequence disappears.
- **Idle:** an empty session looks intentional and useful.
- **Permission:** exact action and decision scope are clear before approval.
- **Failure:** the first actionable failure is visible without scrolling.
- **Receipt:** verified, skipped, failed, and unverified are distinguishable without reading prose.
- **Truth:** removing decorative animation loses no information.
- **Identity:** hiding the logo still leaves a recognizable evidence-and-receipt interaction model.

## Safe Choices

- Transcript remains durable history.
- Same-run steering, double-Esc interruption, session resume, progressive skills, and reduced motion remain intact.
- Terminal themes remain respected.
- Pickers and commands remain keyboard-first.
- No permanent sidebar or file tree.

## Deliberate Risks

1. During execution the transcript stops being the focal surface; current evidence or action takes priority.
2. Knowledge moves from a permanent dashboard into contextual receipts.
3. Completion foregrounds verification and changed files before polished prose.

These departures cost some chat-like familiarity, but make Kode's transparency and honest verification tangible.

## Decisions Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-08-15 | Calm instrument and intelligence made visible | Evidence and provenance are Kode's product identity. |
| 2026-08-17 | Semantic palette, provenance gutter, reduced motion, real git numstat, scroll and mouse support | Make information truthful, accessible, and inspectable. |
| 2026-08-22 | One user anchor and continuous agent rail | Repeated agent role markers made prose look like a noisy log table. |
| 2026-08-24 | Multiline composer, same-run steering, safe double-Esc interruption, progressive skills, compact long-paste attachments | Match terminal-native Claude/Codex interaction expectations without concurrent mutating runs. |
| 2026-08-26 | Structured image attachments | Images remain compact in the UI and travel as provider-native multimodal input. |
| 2026-08-26 | Contextual Workbench approved | Replace six competing surfaces with a stable scope rail, one contextual work surface, and state-aware composer. |
| 2026-08-26 | Preserve existing provenance palette and reject a third coral accent | Bold and whitespace establish focus without weakening Kode's cyan/amber identity. |
| 2026-08-26 | Evidence seam, Run Map, permission/failure/completion receipts | Information appears where it supports the user's next decision. |
