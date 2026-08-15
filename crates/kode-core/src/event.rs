use tokio::sync::broadcast;

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
    /// Emitted once a task's agent loop (including any verification retry)
    /// has fully completed, carrying the final summary counters.
    TaskFinished {
        iterations: u32,
        tool_calls: u32,
        input_tokens: u64,
        output_tokens: u64,
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
}
