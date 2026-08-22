# Design System — Kode TUI

Preview: `.kode-design-preview.html` (rendered mocks of every surface below).

## Product Context
- **What this is:** Kode — a code-intelligence-first coding agent TUI (Rust/ratatui). The agent knows the repository before acting: zindeks code graph + ingat engineering memory feed a provenance-tracked context compiler.
- **Who it's for:** engineers living in the terminal who distrust black-box agents.
- **Competitors:** Claude Code, Codex CLI, opencode, crush — all chat-transcript-first; none surface what the agent knows.
- **Memorable thing:** *intelligence made visible* — you SEE what the agent knows and where every claim came from.

## Aesthetic Direction
- **Direction:** calm instrument. "Senior engineer who read the system", not "AI typing excitedly."
- **Decoration level:** minimal. Evidence composed clearly beats subsystems shouting.
- **Feel target (first 3 seconds):** "this thing already read my codebase."

## Layout (ratatui panes)
1. **Breadcrumb** — 1 row, always: repo · branch+state · provider/model · effort · live context meter (`▓▓▓░░ 3.4k/16k`).
2. **Knowledge Band** — 3 rows under breadcrumb, collapsible (Ctrl+K): `Z` top graph facts, `I` top recalled memory (italic, quoted verbatim), `G` git impact. Real data from the last context compilation only; hidden entirely when a source is unavailable.
3. **Transcript** — home surface. Conversation and event lines carry a 2-col role/provenance gutter: bold `U` anchors user turns; a quiet `│` joins multi-line agent prose into one response block; `Z I G T V` identify knowledge/tool/verification lines. Never fake source provenance on agent prose.
4. **Input line** — bottom, `›` prompt, right edge shows per-source context counts (`Z:4 I:2 G:2`).
5. **Knowledge Aperture** — signature moment. On task submit the band expands ~6 rows: request tree (`─┬─`) with real graph trace, recalled memory + confidence, git impact; contracts on first tool call. Never decorative, never faked; absent when engines are absent.
6. **Ledger view** — Ctrl+L alternate screen: OBJECTIVE / numbered steps (`01 UNDERSTAND ✓`) / CURRENT CHANGE diff / WHY (provenance lines). Chat history demoted, not deleted.
- No permanent sidebar. No file tree. Overlays (model picker, ledger) are summoned and dismissed, zero standing footprint.

## Color
- **Approach:** restrained-semantic. Color = provenance, never decoration. ≤15% of visible cells colored.
- **Background:** terminal default (respect user themes). Never paint full-screen backgrounds.
- **Accents (only two brand colors):**
  - zindeks / structural knowledge: `#63C5DA` (ANSI-256: 80)
  - ingat / recalled memory: `#D7A85B` (179)
- **Semantic:**
  - git: `#8FAE8B` (108) · tools: `#8C9BAB` (103)
  - verified/pass: `#74B88A` (108) · failure: `#D16D72` (167)
  - primary text: terminal default / `#D8DEE5` (253) · muted: `#7C8793` (244) · dim structure: `#525C66` (240)
- Every color pairs with a fixed glyph — shape carries meaning without color (colorblind-safe).

## Glyphs (the whole vocabulary — nothing else)
- Roles: `U` user (bold, primary) · `│` agent prose (dim, continuous)
- Sources: `Z I G T V` (bold, colored)
- Progress: `●` active (`◉` alternate), `○` pending, `✓` done, `×` failed
- Trees: `├─ └─ │ ─┬─` · Relationships: `→` · Input: `›` · Tool: `▸` · Diff: `+ -`
- Markdown list bullet (transcript): `•` · Skipped step mark: `–` · Code-fence rule (transcript): `┄`
- Emphasis: **bold** = headings/active/agent conclusions · dim = history/meta/timestamps · *italic* = verbatim quoted ingat memory (only)
- No emoji in core UI. No box-drawing borders around every section.

## Motion
- Word/sentence-chunk streaming. No character typewriter effect.
- ONE moving region max. Single spinner instance: `· • ● •`. Active-step marker `●/◉` at 4 Hz.
- Unknown duration → elapsed time (`● cargo test 08.4s`). Determinate → `▓▓▓░░ 60%`. Never fake progress.
- New Z/I evidence steps dim→normal over 3 frames. Settled rows never move again.
- Diffs, provenance, verification results, user input: never animate.
- `[ui] reduced_motion = true` kills spinner + pulse.

## Anti-slop (non-negotiable)
No gradients · no emoji spam · no chat bubbles · no card grids · no permanent sidebar · no spinner beside streaming prose · no color without semantics · no fake thinking animation · no knowledge display without real zindeks/ingat data behind it.

## Decisions Log
| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-08-15 | Initial TUI design system | /design-consultation: Codex voice (workbench/aperture) + Claude voice (marginalia) + research: all 4 competitors are transcript+spinner; provenance lane empty |
| 2026-08-15 | Transcript stays home; ledger = alternate view | Stage Codex's boldest idea without betting the home screen |
| 2026-08-15 | Cyan for zindeks, not violet | crush owns purple; cyan+amber unclaimed in the category |
| 2026-08-15 | Gutter shows real event provenance only | No fake footnotes on LLM prose — honesty > theater |
| 2026-08-17 | /design-review pass 1: picker de-boxed to match hint-menu vocabulary; italic restricted to ingat; provenance colors de-decorated; band capped at 3 rows; • – ┄ added to glyph vocabulary | cross-model audit (Codex + Claude) against this file |
| 2026-08-17 | Transcript gutter gains `Z`/`I`/`G` glyphs (cyan bold / amber bold / dim bold) for single-source knowledge notes (`KodeEvent::SourcedNote`) | Design audit item 1 — knowledge-derived facts had no provenance lane in the transcript, only Z/I/G lines |
| 2026-08-17 | Breadcrumb branch segment gets a dim `*` suffix when the worktree is dirty, refreshed by a lazy (non-interval) git poll at TUI start + after each task | Design audit item 3 — breadcrumb never showed clean/dirty state |
| 2026-08-17 | Ledger CURRENT CHANGE renders up to 3 real `git diff --numstat` rows (`<file>  +A -D`, dim numbers) + `+N more`, replacing the apply_patch/write_file call counter; shares the item-3 git poll | Design audit item 4 — counter was a proxy, not the actual diff |
| 2026-08-17 | Ingat knowledge-band/aperture rows get a dim `┄ 0.NN` confidence suffix, split off the ingat text (never baked into the amber/italic color) | Design audit item 2 — Ingat's search `score` field existed but wasn't surfaced |
| 2026-08-17 | New `[ui] reduced_motion` config key (default `false`), threaded into the TUI: kills the spinner glyph animation, the knowledge-band fade, and the Ledger pulse when set | Motion-set audit item 1 — no user-facing off switch existed for the motion vocabulary this file mandates |
| 2026-08-17 | Streamed model tokens buffer and flush to the visible transcript only on a word/whitespace boundary or a 120ms timer, whichever first; the spinner glyph holds a static frame for the whole streaming phase (first delta to stream end) | Motion-set audit item 2 — token-by-token append plus an animating spinner were two moving regions at once, violating the one-moving-region rule |
| 2026-08-17 | New Z/I knowledge-band evidence rows render dim for their first 2 render ticks, normal from the 3rd; gated off (rows render normal immediately) by `reduced_motion` | Motion-set audit item 3 — this file already specified the fade but no implementation tracked row age |
| 2026-08-17 | Ledger's `●` active-step marker alternates `●`/`◉` at 4 Hz while a task is running; static `●` when idle or `reduced_motion` is on | Motion-set audit item 4 — the 4 Hz pulse was specified in this file's Motion section but the Ledger glyph was static |
| 2026-08-17 | Transcript scrollbar: 1-col strip on the right edge, dim `│` track + normal-intensity `│` thumb (no `┃` — outside the glyph vocabulary), no end arrows; auto-hides when content fits the viewport; scroll offset now clamped to content length each render | User-approved addition — transcript had no scrollbar and no clamp, so scroll could run past the end of content |
| 2026-08-17 | Mouse capture enabled for the transcript: wheel scrolls 3 lines per notch, same clamp + follow semantics as the arrow keys; no click/drag handling. Terminal-native text selection is superseded by the `/copy` command | Completes the scrollbar work — a scrollbar with no wheel support reads as decoration, not a real scroll surface |
| 2026-08-22 | Transcript role gutter uses `U` for bold user turns and a continuous dim `│` for agent prose | Repeating `A` on every response line made prose look like a noisy log table; one strong user anchor plus one continuous response rail scans better |
| 2026-08-22 | Running-task interruption requires two Esc presses within 2 seconds; the first replaces the live status with `× press Esc again to interrupt` | A single accidental Esc should not discard in-flight agent work |
