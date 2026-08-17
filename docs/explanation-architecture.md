# Architecture

## The problem

Most coding agents work by grepping around a repo, dumping whatever files look relevant into the prompt, and hoping the model figures out the structure from raw text. This wastes context on file contents the model doesn't need, and it misses structure entirely: call graphs, symbol relationships, what changed recently, what the team already decided and wrote down. The agent re-derives things every session that a proper index already knows.

## Kode's approach

Kode treats code intelligence as infrastructure, not something the agent has to reconstruct by reading files. Two external engines carry that weight:

- **zindeks**: a local code knowledge graph. Symbols, call graphs, imports, BM25 and semantic search, kept current by a background file watcher.
- **Ingat**: an engineering memory service. Project rules, architecture decisions, conventions, known issues, and other durable knowledge, recalled by relevance rather than re-explained every time.

Kode never reimplements either. It spawns and talks to them as first-class adapters. A context compiler pulls from both, plus current git state, and assembles a token-budgeted context for the model: bounded by `[agent] context_budget_tokens`, distinct from the model's raw context window (`[agent] max_context_tokens`). See [reference-config.md](./reference-config.md) for the exact keys.

The agent then runs inside a tool sandbox with its own guardrails (`max_iterations`, `max_tool_calls`), and after edits land, a verification pipeline runs the project's real checks: tests, lint, build: and reports each one honestly: passed, failed, or skipped. A skipped check is never reported as passed. This same honesty rule applies to session replay: when you resume a session, truncated history shows a truncation marker rather than silently pretending the model saw turns it didn't.

## Event-driven pipeline

The task pipeline (`crates/kode/src/pipeline.rs`) communicates with the rest of the system only through `KodeEvent` values: no `print!`/`println!` calls inside the pipeline itself. This is what makes the TUI a pure renderer: it subscribes to the event stream and draws it, but the pipeline has no idea whether a human is watching in a terminal or a script is consuming `kode exec` output. Core crates never depend on `ratatui`: the rendering layer lives entirely in the `kode` binary crate.

## Data flow

```
                         ┌────────────────────┐
  user ──task──▶ TUI or  │   task pipeline     │
                 exec ──▶│  (KodeEvent only)   │
                         └─────────┬───────────┘
                                   │
                    ┌──────────────┼──────────────┐
                    ▼              ▼               ▼
             ┌───────────┐  ┌────────────┐  ┌────────────┐
             │  context  │  │   agent    │  │  git state │
             │ compiler  │◀▶│    loop    │  │  (local)   │
             └─────┬─────┘  └─────┬──────┘  └────────────┘
                    │              │
        ┌───────────┼───────┐     ▼
        ▼           ▼       │  model provider
   ┌─────────┐ ┌──────────┐ │  (codex / opencode-*)
   │ zindeks │ │  Ingat   │ │
   │ (graph) │ │ (memory) │ │
   └─────────┘ └──────────┘ │
                              ▼
                        tool sandbox
                       (reads/edits/shell)
                              │
                              ▼
                     verification pipeline
                    (tests/lint/build, honest
                     pass/fail/skipped)
```

zindeks runs as a spawned child process (stdio transport by default, `ZINDEKS_WATCH=1` so the index refreshes itself on a 2-second poll instead of Kode issuing an explicit post-task refresh). Ingat is reached over its REST API. Both are optional per `[zindeks].enabled` / `[ingat].enabled`: the context compiler simply omits a source it can't reach, and the TUI's knowledge band hides that source rather than showing empty or fake data.

## Trade-offs

**External engines instead of reimplementing them.** Kode could have built its own indexer and memory store. Depending on zindeks and Ingat as first-class adapters means Kode inherits their correctness and their pace of improvement, and stays out of the business of maintaining a second code-graph engine. The cost is an external dependency: if zindeks or Ingat aren't installed, those context sources go dark. `kode setup` and `kode doctor` exist specifically to make that dependency visible and easy to fix, not silently degraded.

**Turn-level session replay instead of full replay.** Sessions store the task and final answer per turn, not the tool calls in between. Full replay would let you inspect exactly what a past session did tool-by-tool, but it means resuming has to re-attach old tool-call ids and re-inject old file contents that may no longer match the repo. Turn-level replay avoids both: no dangling tool-call ids, no stale file contents leaking into a new turn. The cost is that you lose the tool-by-tool trace once a session ends: the transcript in the TUI while it's live is where that detail exists.

**AGPL-3.0 licensing.** Choosing AGPL over a permissive license means anyone offering Kode as a network service has to share their modifications back, which protects the project from being silently forked into a closed competing service. The cost is friction for companies that want to embed Kode in a closed product: which is what the commercial license in [enterprise.md](./enterprise.md) exists to resolve.

## Related

- [reference-config.md](./reference-config.md): the exact budget and engine keys referenced above
- [reference-cli.md](./reference-cli.md): commands that drive this pipeline (`exec`, `verify`, `doctor`)
- [enterprise.md](./enterprise.md): licensing trade-off in more depth
- [../README.md](../README.md)
