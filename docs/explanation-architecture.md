# Architecture

## The problem

Most coding agents work by grepping around a repo, dumping whatever files look relevant into the prompt, and hoping the model figures out the structure from raw text. This wastes context on file contents the model doesn't need, and it misses structure entirely: call graphs, symbol relationships, what changed recently, what the team already decided and wrote down. The agent re-derives things every session that a proper index already knows.

## Kode's approach

Kode treats code intelligence as infrastructure, not something the agent has to reconstruct by reading files. Two separately developed engines carry that weight:

- **[zindeks](https://github.com/sutantodadang/zindeks)**: a local code knowledge graph. Symbols, call graphs, imports, BM25 and semantic search, kept current by a background file watcher.
- **[Ingat](https://github.com/sutantodadang/Ingat)**: engineering memory. Project rules, architecture decisions, conventions, known issues, and other durable knowledge, recalled by relevance rather than re-explained every time.

Kode never reimplements either. Both run inside the Kode process behind first-class adapters (see [Data flow](#data-flow)). A context compiler pulls from both, plus current git state, and assembles a token-budgeted context for the model. When zindeks is healthy, the same adapter also registers `code_search` and `file_outline` as read-only agent tools; indexed symbol/concept discovery comes first, while `git grep` remains the exact-literal or unavailable-engine fallback. When Ingat is healthy, Kode registers read-only `memory_search` for targeted recall during a run and mutating `remember` for durable verified knowledge.

By default Kode resolves the selected model's live context-window metadata, allocates proportional budgets for repository evidence and recent raw history, and auto-compacts an active run at 80% into a structured continuation summary. System rules, repository context, and the newest task remain verbatim; the latest tool protocol stays attached with oversized output bounded. Raw session turns remain on disk. Explicit non-zero `[agent]` budgets override the automatic values. See [reference-config.md](./reference-config.md) for the exact keys.

The agent then runs inside a tool sandbox with command timeouts and user cancellation. Model iterations and tool calls are unbounded, so a valid long-running task is not stopped at an arbitrary count. The root can delegate an independent bounded task through the native `delegate_task` tool. The child shares the workspace, but starts with explicit leaf context rather than the root transcript, cannot delegate recursively, cannot execute arbitrary commands or external MCP tools, and reports only coarse lifecycle receipts to the root UI. Read-only children run with mutations denied; writable children must declare a narrow path ownership scope enforced around `write_file` and `apply_patch`. A single-child gate prevents overlapping workspace writes, and cancellation flows from root to child. The root still owns integration and verification.

Delegation supports tiered model routing: `[agent.subagents.models.<tier>]` maps a short name to any provider/model pair, and the root selects it per delegation via `delegate_task`'s `model` argument — mechanical child work runs on a cheap executor while judgment stays on the root session model. When a child dies mid-run (e.g. provider quota exhaustion), the failure result carries an `activity_tail` of the child's last tool activities so the root can re-delegate with that as context and resume instead of re-exploring from zero.

Before the model is built, a local router decides the task's tier, effort,
and whether to plan first. It runs Laya, a non-generative multilingual
decision model, in-process on ONNX Runtime; each answer carries a calibrated
confidence and falls back to the static configuration below
`router.min_confidence`. The same runtime hosts Qwen3-Reranker-0.6B, which
reorders Ingat memories and zindeks search hits against the task before the
context budget is applied. Both degrade honestly: a missing model, a failing
GPU, or a timeout shows up as `static: <reason>` or `rerank: skipped: <reason>`,
never as a model decision. Every routed task appends one line to
`.kode/router-log.jsonl`, the data for a future fine-tuned checkpoint.

Teams can improve the router from their own work. With
`[router.training] enabled`, each routed task that completes is labeled in
hindsight by the task's own model (what tier, effort, and plan would have
been right, knowing what happened) and appended to a committed dataset;
anyone can correct a label, and corrections win. Calibration and the
acceptance gate run natively in Rust; only fine-tuning runs in Python,
locally through `uv` or on HF Jobs. A fine-tuned model is used only if it
beats the current one on a held-out split, and a committed manifest tells
every teammate's Kode which model to load, falling back to the pinned model
with a note whenever the team model cannot be used.

After edits land, a verification pipeline runs the project's real checks: tests, lint, build: and reports each one honestly: passed, failed, or skipped. A skipped check is never reported as passed. This same honesty rule applies to session replay: when you resume a session, truncated history shows a truncation marker rather than silently pretending the model saw turns it didn't.

Edits are wrapped by an `ImpactAwareTool` decorator (in the `kode` bin). After a successful `apply_patch` or `write_file`, it intersects the edited line range with the file's outline, traces inbound callers and covering tests through the code graph, emits a `KodeEvent::Impact`, appends a bounded caller list to the tool result so the model knows what else may need updating, and records the covering tests for this task. In non-auto mode the same impact appears in the permission prompt before the write (when the target can be located without writing). The recorded tests feed targeted verification: with `[verify] targeted = "first"` (default) the graph-selected tests run first and fail fast, then the full suite runs; `"only"` replaces the full test step, which is reported Skipped; `"off"` disables it. A skipped full suite is never reported as passed.

## Event-driven pipeline

The task pipeline (`crates/kode/src/pipeline.rs`) communicates with the rest of the system only through `KodeEvent` values: no `print!`/`println!` calls inside the pipeline itself. This is what makes the TUI a pure renderer: it subscribes to the event stream and draws it, but the pipeline has no idea whether a human is watching in a terminal or a script is consuming `kode exec` output. Core crates never depend on `ratatui`: the rendering layer lives entirely in the `kode` binary crate.

## Data flow

```
                         ┌─────────────────────┐
  user ──task──▶ TUI or  │    task pipeline    │
                 exec ──▶│  (KodeEvent only)   │
                         └──────────┬──────────┘
                                    │
                                    ▼
                         ┌─────────────────────┐
                         │    local router     │  tier / effort / plan
                         │ (Laya + reranker,   │  static fallback when
                         │  ONNX, optional)    │  models are absent
                         └──────────┬──────────┘
                                    │
                         ┌──────────┴──────────┐
                         ▼                     ▼
                  ┌────────────┐        ┌────────────┐
                  │  context   │◀──────▶│   agent    │──────▶ model provider
                  │  compiler  │        │    loop    │        (codex / anthropic /
                  └─────┬──────┘        └─────┬──────┘         antigravity / openai /
                        │                     │                opencode-family)
         ┌──────────────┼──────────────┐      ▼
         ▼              ▼              ▼    tool sandbox
   ┌───────────┐  ┌───────────┐  ┌──────────┐ (reads/edits/shell,
   │  zindeks  │  │   Ingat   │  │ git state│  MCP, sub-agents)
   │  (graph)  │  │ (memory)  │  │ (local)  │ │
   └───────────┘  └───────────┘  └──────────┘ ▼
      in-process engines           verification pipeline
                                  (tests/lint/build, honest
                                   pass/fail/skipped)
```

Each box maps to a workspace crate; the crate table in [CONTRIBUTING.md](../CONTRIBUTING.md#workspace-layout) lists them.

Both engines run in-process: Kode loads a pinned zindeks shared library and links Ingat's headless core, so there is no spawned child, service, or port. With `watch = true` the code engine's own watcher refreshes the index on a background poll instead of Kode issuing an explicit post-task refresh. Both are optional per `[zindeks].enabled` / `[ingat].enabled`: the context compiler simply omits a source it can't reach, their native tools are not registered, and the TUI's knowledge band hides that source rather than showing empty or fake data.

## Trade-offs

**Embedded engines instead of reimplementing them.** Kode could have built its own indexer and memory store. Embedding zindeks (as a pinned shared library) and Ingat's headless core means Kode inherits their correctness and their pace of improvement, and stays out of the business of maintaining a second code-graph engine. The cost is shipping and verifying those engine artifacts: `kode setup` downloads the pinned zindeks library (checksum-verified) and Ingat's core is linked into the binary. `kode doctor` exists specifically to make a missing or mismatched engine visible, not silently degraded.

**Turn-level session replay instead of full replay.** Sessions store the task and final answer per turn, not the tool calls in between. Full replay would let you inspect exactly what a past session did tool-by-tool, but it means resuming has to re-attach old tool-call ids and re-inject old file contents that may no longer match the repo. Turn-level replay avoids both: no dangling tool-call ids, no stale file contents leaking into a new turn. The cost is that you lose the tool-by-tool trace once a session ends: the transcript in the TUI while it's live is where that detail exists.

**AGPL-3.0 licensing.** Choosing AGPL over a permissive license means anyone offering Kode as a network service has to share their modifications back, which protects the project from being silently forked into a closed competing service. The cost is friction for companies that want to embed Kode in a closed product: which is what the commercial license in [enterprise.md](./enterprise.md) exists to resolve.

## Related

- [reference-config.md](./reference-config.md): the exact budget and engine keys referenced above
- [reference-cli.md](./reference-cli.md): commands that drive this pipeline (`exec`, `verify`, `doctor`)
- [enterprise.md](./enterprise.md): licensing trade-off in more depth
- [../README.md](../README.md)
