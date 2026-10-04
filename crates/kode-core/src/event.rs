use tokio::sync::broadcast;

use crate::UserInput;

/// One step of the Ledger view's task lifecycle. `Plan` only appears when
/// plan mode is on — it's prepended ahead of the fixed Understand/Decide/
/// Change/Verify steps and marked done once the user approves the plan (see
/// `kode::pipeline::run_plan_phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStep {
    Plan,
    Understand,
    Decide,
    Change,
    Verify,
}

/// Which engine a [`KodeEvent::SourcedNote`] traces back to — drives the
/// TUI transcript gutter's `Z`/`I`/`G` provenance glyph. Kept separate from
/// the plain `Note` variant (used for status/error text with no single
/// engine behind it) so existing `Note` call sites are untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSource {
    Zindeks,
    Ingat,
    Git,
}

/// Where one routing answer came from. `Static` carries the reason the
/// model's answer was not used (disabled, not installed, low confidence…).
#[derive(Debug, Clone, PartialEq)]
pub enum RouteSource {
    Laya,
    Static(String),
}

/// One routed decision (`tier`, `effort`, or `plan`).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteAnswer {
    pub key: String,
    pub value: String,
    /// Laya confidence (1 - normalized entropy) when the model ran.
    pub confidence: Option<f32>,
    pub source: RouteSource,
}

impl RouteAnswer {
    /// `tier=heavy (laya 0.82)` / `plan=direct (static: low confidence 0.41)`.
    pub fn describe(&self) -> String {
        let origin = match &self.source {
            RouteSource::Laya => format!("laya {:.2}", self.confidence.unwrap_or(0.0)),
            RouteSource::Static(reason) => format!("static: {reason}"),
        };
        format!("{}={} ({origin})", self.key, self.value)
    }
}

/// One-line, frontend-agnostic rendering of a routing decision.
pub fn router_summary(answers: &[RouteAnswer]) -> String {
    let parts: Vec<String> = answers.iter().map(RouteAnswer::describe).collect();
    format!("router: {}", parts.join(" · "))
}

/// One file's line delta for a task, relative to `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

/// Events emitted during an agent run.
#[derive(Debug, Clone)]
pub enum KodeEvent {
    AgentStarted,
    ContextCompilationStarted,
    ContextCompiled {
        token_estimate: usize,
        sections: usize,
    },
    ModelStarted,
    ModelToken {
        text: String,
    },
    ToolRequested {
        name: String,
    },
    ToolStarted {
        name: String,
    },
    ToolFinished {
        name: String,
        ok: bool,
        /// Short, single-line failure reason when `ok == false` (None on
        /// success). Frontends render it next to the tool name.
        error: Option<String>,
    },
    /// A bounded leaf agent started. Child model/tool traffic stays private;
    /// frontends receive only these lifecycle receipts.
    SubagentStarted {
        id: String,
        ownership: Vec<String>,
    },
    /// One child activity receipt, grouped under the active subagent by UIs.
    SubagentActivity {
        id: String,
        text: String,
    },
    /// A bounded leaf agent completed or failed without changing the parent
    /// agent's top-level run state.
    SubagentFinished {
        id: String,
        ok: bool,
        summary: String,
    },
    /// A user steering message was accepted into the active agent's message
    /// history. The TUI uses this acknowledgment when persisting the turn.
    SteeringAccepted {
        message: UserInput,
    },
    /// Messages reached the pipeline after its last agent segment. They are
    /// returned to the TUI to run as the next turn instead of being dropped.
    SteeringDeferred {
        messages: Vec<UserInput>,
    },
    VerificationStarted,
    VerificationFinished {
        ok: bool,
    },
    AgentFinished,
    AgentError {
        message: String,
    },
    /// A progress or degradation note (UI-agnostic; frontends render it as
    /// they see fit, e.g. `◆ {text}`).
    Note {
        text: String,
    },
    /// The local router's per-task decision, emitted once before the model
    /// is built. Frontends render [`router_summary`] of it.
    RouterDecision {
        answers: Vec<RouteAnswer>,
    },
    /// A `Note` with known single-engine provenance (zindeks/ingat/git),
    /// emitted where the pipeline can attribute the fact to exactly one
    /// source. Frontends render this distinctly (TUI: `Z`/`I`/`G` gutter;
    /// headless: same as `Note`). Never emitted for multi-source or
    /// no-source text — those stay plain `Note`.
    SourcedNote {
        text: String,
        source: NoteSource,
    },
    /// Emitted once a task's agent loop (including any verification retry)
    /// has fully completed, carrying the final summary counters.
    TaskFinished {
        iterations: u32,
        tool_calls: u32,
        /// Total input tokens, cached share included.
        input_tokens: u64,
        output_tokens: u64,
        /// Input tokens served from the provider cache. `None` means the
        /// provider did not report it; never render that as zero.
        cached_tokens: Option<u64>,
    },
    /// Emitted once per context compilation, alongside `ContextCompiled`.
    /// Carries a UI-ready digest of what the agent knows for this task:
    /// up to 3 zindeks fact lines, up to 2 ingat memory summaries, up to 1
    /// git impact line, and the compiled/budget token counts. Frontends
    /// render this as they see fit (TUI: Knowledge Band; headless: a
    /// compact summary line).
    Knowledge {
        zindeks: Vec<String>,
        ingat: Vec<String>,
        git: Vec<String>,
        context_tokens: usize,
        budget_tokens: usize,
    },
    /// One verification step's result, emitted per `StepResult` right
    /// before the summary `Note`. Frontends render this distinctly (TUI:
    /// `V` gutter; headless: `◆ {name}: {PASS|FAIL|SKIP}`).
    VerifyStep {
        name: String,
        passed: bool,
        skipped: bool,
        duration_ms: u64,
    },
    /// Progress on one of the Ledger view's 4 fixed task steps
    /// (Understand/Decide/Change/Verify). `Decide` is never emitted by the
    /// pipeline — frontends derive it locally from the first `ToolStarted`
    /// event of a run, which is itself observable fact.
    TaskProgress {
        step: TaskStep,
        done: bool,
    },
    /// Files whose diff against `HEAD` moved during the task, from git.
    /// Emitted once per task after the agent loop (and any repair), only
    /// when the task mutated files and both git snapshots succeeded.
    ChangeSet {
        files: Vec<FileChange>,
    },
    /// Background indexing started (TUI first run or `/index`). The engine
    /// reports no per-file progress, so only start and finish exist.
    IndexStarted,
    /// Background indexing ended. `files` is the indexed document count
    /// when the health check answered; `error` is set on failure.
    IndexFinished {
        files: Option<u64>,
        error: Option<String>,
        elapsed_ms: u64,
    },
    /// The task was answered from the code graph with zero model tokens.
    /// `text` is the plain-text answer (stored as the turn's response).
    GraphAnswered {
        query: String,
        symbol: String,
        latency_ms: u64,
        text: String,
    },
    /// Blast radius of one edited symbol, from the call graph.
    Impact {
        file: String,
        symbol: String,
        callers: u32,
        crates: u32,
        tests: u32,
    },
}

/// Broadcast bus for `KodeEvent`s.
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<KodeEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Emits an event. Ignores the error when there are no subscribers.
    pub fn emit(&self, event: KodeEvent) {
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<KodeEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribe_emit_receive() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        bus.emit(KodeEvent::AgentStarted);
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, KodeEvent::AgentStarted));
    }

    #[test]
    fn emit_with_no_subscribers_does_not_panic() {
        let bus = EventBus::new(8);
        bus.emit(KodeEvent::AgentStarted);
    }

    #[tokio::test]
    async fn sourced_note_round_trips_with_its_source() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        bus.emit(KodeEvent::SourcedNote {
            text: "zindeks index refreshed".to_string(),
            source: NoteSource::Zindeks,
        });
        match rx.recv().await.unwrap() {
            KodeEvent::SourcedNote { text, source } => {
                assert_eq!(text, "zindeks index refreshed");
                assert_eq!(source, NoteSource::Zindeks);
            }
            other => panic!("expected SourcedNote, got {other:?}"),
        }
    }

    #[test]
    fn router_summary_labels_every_source() {
        let answers = vec![
            RouteAnswer {
                key: "tier".to_string(),
                value: "heavy".to_string(),
                confidence: Some(0.82),
                source: RouteSource::Laya,
            },
            RouteAnswer {
                key: "plan".to_string(),
                value: "direct".to_string(),
                confidence: Some(0.41),
                source: RouteSource::Static("low confidence 0.41".to_string()),
            },
            RouteAnswer {
                key: "effort".to_string(),
                value: "config".to_string(),
                confidence: None,
                source: RouteSource::Static("models not installed — run `kode setup`".to_string()),
            },
        ];
        assert_eq!(
            router_summary(&answers),
            "router: tier=heavy (laya 0.82) · plan=direct (static: low confidence 0.41) · effort=config (static: models not installed — run `kode setup`)"
        );
    }
}
