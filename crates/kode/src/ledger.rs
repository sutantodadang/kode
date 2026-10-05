//! Per-turn provenance record: what the agent knew, decided, changed and
//! verified. Persisted on `session::Turn` and read by `/why`, memory
//! proposals and receipts.

use std::collections::HashSet;

use kode_core::event::{KodeEvent, NoteSource, RouteAnswer, RouteSource};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    Zindeks,
    Ingat,
    Git,
}

impl From<NoteSource> for FactSource {
    fn from(source: NoteSource) -> Self {
        match source {
            NoteSource::Zindeks => FactSource::Zindeks,
            NoteSource::Ingat => FactSource::Ingat,
            NoteSource::Git => FactSource::Git,
        }
    }
}

/// One router answer as stored on disk. `laya == false` means the static
/// fallback answered; `reason` says why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRecord {
    pub key: String,
    pub value: String,
    pub confidence: Option<f32>,
    pub laya: bool,
    pub reason: Option<String>,
}

impl From<&RouteAnswer> for RouteRecord {
    fn from(a: &RouteAnswer) -> Self {
        let (laya, reason) = match &a.source {
            RouteSource::Laya => (true, None),
            RouteSource::Static(reason) => (false, Some(reason.clone())),
        };
        Self {
            key: a.key.clone(),
            value: a.value.clone(),
            confidence: a.confidence,
            laya,
            reason,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOutcome {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum LedgerEntry {
    Fact {
        source: FactSource,
        text: String,
    },
    Route {
        answers: Vec<RouteRecord>,
    },
    Change {
        files: Vec<ChangeRecord>,
        #[serde(default)]
        reverted: Vec<String>,
    },
    GraphAnswer {
        query: String,
        symbol: String,
        latency_ms: u64,
    },
    Impact {
        file: String,
        symbol: String,
        callers: u32,
        crates: u32,
        tests: u32,
    },
    Memory {
        id: String,
        text: String,
        team: bool,
    },
    Verify {
        name: String,
        outcome: VerifyOutcome,
        duration_ms: u64,
    },
    Usage {
        input: u64,
        output: u64,
        cached: Option<u64>,
    },
}

/// Non-ledger signals the recorder collects for memory-proposal triggers.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TurnSignals {
    pub steering: Vec<String>,
    pub tool_errors: Vec<(String, String)>,
}

/// Collects one task's ledger from the event stream. Records only between
/// `begin` and `take`, so events emitted outside a task (a `/map` run, an
/// index finishing) never leak into a turn.
#[derive(Debug, Default)]
pub struct LedgerRecorder {
    active: bool,
    entries: Vec<LedgerEntry>,
    signals: TurnSignals,
    seen_facts: HashSet<(FactSource, String)>,
}

impl LedgerRecorder {
    pub fn begin(&mut self) {
        self.active = true;
        self.entries.clear();
        self.signals = TurnSignals::default();
        self.seen_facts.clear();
    }

    pub fn observe(&mut self, ev: &KodeEvent) {
        if !self.active {
            return;
        }
        match ev {
            KodeEvent::SourcedNote { text, source } => self.fact((*source).into(), text),
            KodeEvent::Knowledge {
                zindeks,
                ingat,
                git,
                ..
            } => {
                for text in zindeks {
                    self.fact(FactSource::Zindeks, text);
                }
                for text in ingat {
                    self.fact(FactSource::Ingat, text);
                }
                for text in git {
                    self.fact(FactSource::Git, text);
                }
            }
            KodeEvent::RouterDecision { answers } => self.entries.push(LedgerEntry::Route {
                answers: answers.iter().map(RouteRecord::from).collect(),
            }),
            KodeEvent::ChangeSet { files, reverted } => self.entries.push(LedgerEntry::Change {
                files: files
                    .iter()
                    .map(|f| ChangeRecord {
                        path: f.path.clone(),
                        added: f.added,
                        removed: f.removed,
                    })
                    .collect(),
                reverted: reverted.clone(),
            }),
            KodeEvent::GraphAnswered {
                query,
                symbol,
                latency_ms,
                ..
            } => self.entries.push(LedgerEntry::GraphAnswer {
                query: query.clone(),
                symbol: symbol.clone(),
                latency_ms: *latency_ms,
            }),
            KodeEvent::Impact {
                file,
                symbol,
                callers,
                crates,
                tests,
            } => self.entries.push(LedgerEntry::Impact {
                file: file.clone(),
                symbol: symbol.clone(),
                callers: *callers,
                crates: *crates,
                tests: *tests,
            }),
            KodeEvent::SteeringAccepted { message } => {
                self.signals.steering.push(message.text.clone())
            }
            KodeEvent::ToolFinished {
                name,
                ok: false,
                error: Some(error),
            } => self.signals.tool_errors.push((name.clone(), error.clone())),
            KodeEvent::VerifyStep {
                name,
                passed,
                skipped,
                duration_ms,
            } => {
                let outcome = if *skipped {
                    VerifyOutcome::Skipped
                } else if *passed {
                    VerifyOutcome::Passed
                } else {
                    VerifyOutcome::Failed
                };
                self.entries.push(LedgerEntry::Verify {
                    name: name.clone(),
                    outcome,
                    duration_ms: *duration_ms,
                });
            }
            KodeEvent::TaskFinished {
                input_tokens,
                output_tokens,
                cached_tokens,
                ..
            } => self.entries.push(LedgerEntry::Usage {
                input: *input_tokens,
                output: *output_tokens,
                cached: *cached_tokens,
            }),
            _ => {}
        }
    }

    pub fn take_turn(&mut self) -> (Vec<LedgerEntry>, TurnSignals) {
        self.active = false;
        self.seen_facts.clear();
        (
            std::mem::take(&mut self.entries),
            std::mem::take(&mut self.signals),
        )
    }

    #[cfg(test)]
    pub fn take(&mut self) -> Vec<LedgerEntry> {
        self.take_turn().0
    }

    /// `Knowledge` lines and `SourcedNote`s can repeat one fact; keep it once.
    fn fact(&mut self, source: FactSource, text: &str) {
        if self.seen_facts.insert((source, text.to_string())) {
            self.entries.push(LedgerEntry::Fact {
                source,
                text: text.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_core::event::{FileChange, KodeEvent, NoteSource, RouteAnswer, RouteSource};

    fn route(key: &str, value: &str, laya: bool) -> RouteAnswer {
        RouteAnswer {
            key: key.into(),
            value: value.into(),
            confidence: laya.then_some(0.8),
            source: if laya {
                RouteSource::Laya
            } else {
                RouteSource::Static("disabled".into())
            },
        }
    }

    #[test]
    fn inactive_recorder_ignores_events() {
        let mut r = LedgerRecorder::default();
        r.observe(&KodeEvent::SourcedNote {
            text: "stray".into(),
            source: NoteSource::Zindeks,
        });
        r.begin();
        assert!(r.take().is_empty());
    }

    #[test]
    fn maps_task_events_to_entries_in_order() {
        let mut r = LedgerRecorder::default();
        r.begin();
        r.observe(&KodeEvent::RouterDecision {
            answers: vec![route("tier", "heavy", true), route("plan", "direct", false)],
        });
        r.observe(&KodeEvent::Knowledge {
            zindeks: vec!["fetch ← 4 callers".into()],
            ingat: vec!["30s timeout reverted".into()],
            git: vec![],
            context_tokens: 10,
            budget_tokens: 100,
        });
        r.observe(&KodeEvent::SourcedNote {
            text: "fetch ← 4 callers".into(),
            source: NoteSource::Zindeks,
        });
        r.observe(&KodeEvent::Note {
            text: "chatter".into(),
        });
        r.observe(&KodeEvent::ChangeSet {
            files: vec![FileChange {
                path: "a.rs".into(),
                added: 3,
                removed: 1,
            }],
            reverted: vec![],
        });
        r.observe(&KodeEvent::VerifyStep {
            name: "lint".into(),
            passed: false,
            skipped: true,
            duration_ms: 0,
        });
        r.observe(&KodeEvent::TaskFinished {
            iterations: 1,
            tool_calls: 2,
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: None,
        });

        let entries = r.take();
        assert_eq!(
            entries,
            vec![
                LedgerEntry::Route {
                    answers: vec![
                        RouteRecord {
                            key: "tier".into(),
                            value: "heavy".into(),
                            confidence: Some(0.8),
                            laya: true,
                            reason: None
                        },
                        RouteRecord {
                            key: "plan".into(),
                            value: "direct".into(),
                            confidence: None,
                            laya: false,
                            reason: Some("disabled".into())
                        },
                    ]
                },
                LedgerEntry::Fact {
                    source: FactSource::Zindeks,
                    text: "fetch ← 4 callers".into()
                },
                LedgerEntry::Fact {
                    source: FactSource::Ingat,
                    text: "30s timeout reverted".into()
                },
                LedgerEntry::Change {
                    files: vec![ChangeRecord {
                        path: "a.rs".into(),
                        added: 3,
                        removed: 1
                    }],
                    reverted: vec![],
                },
                LedgerEntry::Verify {
                    name: "lint".into(),
                    outcome: VerifyOutcome::Skipped,
                    duration_ms: 0
                },
                LedgerEntry::Usage {
                    input: 100,
                    output: 20,
                    cached: None
                },
            ]
        );
        // take() stops recording until the next begin().
        r.observe(&KodeEvent::Note {
            text: "after".into(),
        });
        assert!(r.take().is_empty());
    }

    #[test]
    fn graph_answered_is_recorded() {
        let mut r = LedgerRecorder::default();
        r.begin();
        r.observe(&KodeEvent::GraphAnswered {
            query: "callers".into(),
            symbol: "append_turn".into(),
            latency_ms: 12,
            text: "…".into(),
        });
        assert_eq!(
            r.take(),
            vec![LedgerEntry::GraphAnswer {
                query: "callers".into(),
                symbol: "append_turn".into(),
                latency_ms: 12
            }]
        );
    }

    #[test]
    fn impact_is_recorded() {
        let mut r = LedgerRecorder::default();
        r.begin();
        r.observe(&KodeEvent::Impact {
            file: "a.rs".into(),
            symbol: "fetch".into(),
            callers: 4,
            crates: 2,
            tests: 3,
        });
        assert_eq!(
            r.take(),
            vec![LedgerEntry::Impact {
                file: "a.rs".into(),
                symbol: "fetch".into(),
                callers: 4,
                crates: 2,
                tests: 3
            }]
        );
    }

    #[test]
    fn signals_capture_steering_and_tool_errors() {
        let mut r = LedgerRecorder::default();
        r.begin();
        r.observe(&KodeEvent::SteeringAccepted {
            message: kode_core::UserInput::text("use the async client"),
        });
        r.observe(&KodeEvent::ToolFinished {
            name: "run_command".into(),
            ok: false,
            error: Some("port in use".into()),
        });
        r.observe(&KodeEvent::ToolFinished {
            name: "run_command".into(),
            ok: true,
            error: None,
        });
        let (_, signals) = r.take_turn();
        assert_eq!(signals.steering, vec!["use the async client"]);
        assert_eq!(
            signals.tool_errors,
            vec![("run_command".to_string(), "port in use".to_string())]
        );
        r.begin();
        assert_eq!(r.take_turn().1, TurnSignals::default());
    }

    #[test]
    fn every_entry_round_trips_through_json() {
        let entries = vec![
            LedgerEntry::Fact {
                source: FactSource::Git,
                text: "3 files co-change".into(),
            },
            LedgerEntry::Route {
                answers: vec![RouteRecord {
                    key: "tier".into(),
                    value: "light".into(),
                    confidence: Some(0.5),
                    laya: true,
                    reason: None,
                }],
            },
            LedgerEntry::Change {
                files: vec![ChangeRecord {
                    path: "x".into(),
                    added: 1,
                    removed: 0,
                }],
                reverted: vec![],
            },
            LedgerEntry::Verify {
                name: "test".into(),
                outcome: VerifyOutcome::Passed,
                duration_ms: 1200,
            },
            LedgerEntry::Usage {
                input: 1,
                output: 2,
                cached: Some(0),
            },
        ];
        let json = serde_json::to_string(&entries).unwrap();
        assert!(json.contains("\"k\":\"fact\""));
        let back: Vec<LedgerEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entries);

        // A `change` written before `reverted` existed loads with an empty list.
        let old: LedgerEntry = serde_json::from_str(r#"{"k":"change","files":[]}"#).unwrap();
        assert_eq!(
            old,
            LedgerEntry::Change {
                files: vec![],
                reverted: vec![]
            }
        );
    }
}
