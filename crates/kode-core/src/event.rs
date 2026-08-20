use tokio::sync::broadcast;

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
        input_tokens: u64,
        output_tokens: u64,
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
}
