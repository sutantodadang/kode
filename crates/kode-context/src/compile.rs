use std::cmp::Ordering;
use std::path::Path;
use std::sync::Arc;

use kode_intel::error::IntelError;
use kode_intel::{CodeContextRequest, CodeIntelligence};
use kode_memory::{EngineeringMemory, Memory, MemoryKind, MemoryQuery, Provenance};

use crate::git;
use crate::types::{
    CompiledContext, ContextRequest, ContextSection, ContextSource, ContextStats, estimate_tokens,
};

/// A candidate section will not be truncated to fit the remaining budget
/// unless at least this many tokens remain — below that, it is dropped
/// instead of kept as a near-empty fragment.
const MIN_TRUNCATE_BUDGET: usize = 200;

/// Number of memories requested per `EngineeringMemory::search` call.
const MEMORY_SEARCH_LIMIT: u32 = 12;

/// Per-memory body is truncated to this many characters when formatted into
/// a section bullet.
const MEMORY_BODY_TRUNCATE_CHARS: usize = 600;

/// Fuses git working-tree state, Zindeks repository context, and Ingat
/// engineering memories into a deterministic, priority-ordered,
/// token-budgeted [`CompiledContext`].
///
/// Pure: performs no logging/event emission itself. Callers wrap `compile`
/// with whatever events/telemetry they need. Intel/memory failures degrade
/// the result (recorded in `stats.intel_status`/`stats.memory_status`)
/// rather than aborting compile.
pub struct ContextCompiler {
    intel: Option<Arc<dyn CodeIntelligence>>,
    memory: Option<Arc<dyn EngineeringMemory>>,
    budget_tokens: usize,
}

impl ContextCompiler {
    pub fn new(
        intel: Option<Arc<dyn CodeIntelligence>>,
        memory: Option<Arc<dyn EngineeringMemory>>,
        budget_tokens: usize,
    ) -> Self {
        Self {
            intel,
            memory,
            budget_tokens,
        }
    }

    pub async fn compile(&self, request: &ContextRequest, root: &Path) -> CompiledContext {
        let git_fut = git::git_state(root);
        let intel_fut = async {
            match &self.intel {
                Some(intel) => {
                    let max_tokens = self.budget_tokens.min(8_000) as u32;
                    Some(
                        intel
                            .get_context(CodeContextRequest {
                                query: request.task.clone(),
                                working_set: request.working_set.clone(),
                                max_tokens: Some(max_tokens),
                            })
                            .await,
                    )
                }
                None => None,
            }
        };
        let memory_fut = async {
            match &self.memory {
                Some(memory) => {
                    let repository = root
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned());
                    Some(
                        memory
                            .search(&MemoryQuery {
                                text: request.task.clone(),
                                repository,
                                kind: None,
                                limit: MEMORY_SEARCH_LIMIT,
                            })
                            .await,
                    )
                }
                None => None,
            }
        };

        let (git_state, intel_result, memory_result) = tokio::join!(git_fut, intel_fut, memory_fut);

        // (section, memory_count) — memory_count is 0 for non-memory
        // sections, and the number of memory bullets folded into the
        // section body otherwise. Used to compute `memories_retained` after
        // the budget pass without re-deriving it from section text.
        let mut candidates: Vec<(ContextSection, usize)> = Vec::new();
        let mut intel_status = "disabled".to_string();
        let mut memory_status = "disabled".to_string();
        let mut memories_retrieved = 0usize;

        if let Some(state) = git_state
            && !(state.status.is_empty() && state.diff.is_empty())
        {
            let mut body = format!("status:\n{}", state.status);
            if !state.diff.is_empty() {
                body.push_str(&format!("\n\ndiff:\n{}", state.diff));
            }
            let tokens = estimate_tokens(&body);
            candidates.push((
                ContextSection {
                    source: ContextSource::Git,
                    title: "Uncommitted changes".to_string(),
                    body,
                    tokens,
                },
                0,
            ));
        }

        match intel_result {
            None => {}
            Some(Ok(code_context)) => {
                intel_status = "ok".to_string();
                let tokens = estimate_tokens(&code_context.text);
                candidates.push((
                    ContextSection {
                        source: ContextSource::CodeIntelligence,
                        title: "Repository context (zindeks)".to_string(),
                        body: code_context.text,
                        tokens,
                    },
                    0,
                ));
            }
            Some(Err(IntelError::NotIndexed(_))) => {
                intel_status = "not indexed".to_string();
            }
            Some(Err(e)) => {
                intel_status = format!("unavailable: {e}");
            }
        }

        match memory_result {
            None => {}
            Some(Ok(memories)) => {
                memory_status = "ok".to_string();
                memories_retrieved = memories.len();

                let mut rules: Vec<&Memory> = Vec::new();
                let mut decisions: Vec<&Memory> = Vec::new();
                let mut past: Vec<&Memory> = Vec::new();
                for memory in &memories {
                    match memory.kind {
                        Some(MemoryKind::ProjectRule)
                        | Some(MemoryKind::Convention)
                        | Some(MemoryKind::UserPreference) => rules.push(memory),
                        Some(MemoryKind::ArchitectureDecision)
                        | Some(MemoryKind::RejectedApproach) => decisions.push(memory),
                        Some(MemoryKind::HistoricalSolution)
                        | Some(MemoryKind::KnownIssue)
                        | Some(MemoryKind::BuildKnowledge)
                        | None => past.push(memory),
                    }
                }

                for (mut group, title) in [
                    (rules, "Project rules & conventions"),
                    (decisions, "Architecture decisions"),
                    (past, "Past solutions & known issues"),
                ] {
                    if group.is_empty() {
                        continue;
                    }
                    group.sort_by(|a, b| {
                        provenance_rank(a.provenance)
                            .cmp(&provenance_rank(b.provenance))
                            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
                    });
                    let count = group.len();
                    let body = group
                        .iter()
                        .map(|m| format_memory_bullet(m))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let tokens = estimate_tokens(&body);
                    candidates.push((
                        ContextSection {
                            source: ContextSource::Memory,
                            title: title.to_string(),
                            body,
                            tokens,
                        },
                        count,
                    ));
                }
            }
            Some(Err(e)) => {
                memory_status = format!("unavailable: {e}");
            }
        }

        let raw_tokens: usize = candidates.iter().map(|(c, _)| c.tokens).sum();
        let sections_retrieved = candidates.len();

        let mut sections = Vec::new();
        let mut remaining = self.budget_tokens;
        let mut memories_retained = 0usize;
        for (mut section, mem_count) in candidates {
            if section.tokens <= remaining {
                remaining -= section.tokens;
                memories_retained += mem_count;
                sections.push(section);
            } else if remaining >= MIN_TRUNCATE_BUDGET {
                let char_budget = remaining * 4;
                let mut truncated: String = section.body.chars().take(char_budget).collect();
                truncated.push_str("\n[truncated to fit context budget]");
                section.body = truncated;
                section.tokens = estimate_tokens(&section.body);
                memories_retained += mem_count;
                sections.push(section);
                break;
            } else {
                break;
            }
        }

        let compiled_tokens: usize = sections.iter().map(|s| s.tokens).sum();
        let sections_retained = sections.len();

        CompiledContext {
            sections,
            stats: ContextStats {
                raw_tokens,
                compiled_tokens,
                sections_retrieved,
                sections_retained,
                intel_status,
                memory_status,
                memories_retrieved,
                memories_retained,
            },
        }
    }
}

/// Ordering rank for a memory's provenance within a section — lower sorts
/// first. Ties broken by descending score.
fn provenance_rank(provenance: Option<Provenance>) -> u8 {
    match provenance {
        Some(Provenance::ExplicitUser) => 0,
        Some(Provenance::VerifiedCode) => 1,
        Some(Provenance::VerifiedTest) => 1,
        Some(Provenance::ArchitectureDecision) => 2,
        Some(Provenance::AgentInference) => 9,
        None => 5,
    }
}

/// Formats one memory as a markdown bullet:
/// `- **[kind]** summary — body _(inferred, low confidence)_`.
fn format_memory_bullet(memory: &Memory) -> String {
    let kind_label = memory.kind.map(|k| k.as_kebab()).unwrap_or("memory");
    let body = truncate_memory_body(&memory.body);

    let mut line = format!("- **[{kind_label}]** {}", memory.summary);
    if body != memory.summary {
        line.push_str(&format!(" — {body}"));
    }
    if memory.provenance == Some(Provenance::AgentInference) {
        line.push_str(" _(inferred, low confidence)_");
    }
    line
}

/// Truncates `body` to at most [`MEMORY_BODY_TRUNCATE_CHARS`] chars,
/// char-boundary safe, appending "…" when cut.
fn truncate_memory_body(body: &str) -> String {
    if body.chars().count() <= MEMORY_BODY_TRUNCATE_CHARS {
        return body.to_string();
    }
    let mut truncated: String = body.chars().take(MEMORY_BODY_TRUNCATE_CHARS).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_intel::MockCodeIntelligence;
    use kode_intel::types::CodeContext;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-context-compile-{label}-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
    }

    fn request() -> ContextRequest {
        ContextRequest {
            task: "do the thing".to_string(),
            working_set: vec![],
        }
    }

    #[tokio::test]
    async fn intel_only_when_no_git_repo() {
        let dir = temp_dir("intel-only");
        let mock = MockCodeIntelligence {
            context: CodeContext {
                text: "some repo context".to_string(),
                token_estimate: 4,
            },
            ..Default::default()
        };

        let compiler = ContextCompiler::new(Some(Arc::new(mock)), None, 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.sections.len(), 1);
        assert_eq!(compiled.sections[0].source, ContextSource::CodeIntelligence);
        assert_eq!(compiled.stats.intel_status, "ok");
        assert_eq!(compiled.stats.sections_retrieved, 1);
        assert_eq!(compiled.stats.sections_retained, 1);
    }

    #[tokio::test]
    async fn intel_disabled_and_no_git_yields_no_sections() {
        let dir = temp_dir("disabled");
        let compiler = ContextCompiler::new(None, None, 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert!(compiled.sections.is_empty());
        assert_eq!(compiled.stats.intel_status, "disabled");
        assert_eq!(compiled.stats.memory_status, "disabled");
        assert_eq!(compiled.render(), None);
    }

    #[tokio::test]
    async fn budget_truncates_oversized_section() {
        let dir = temp_dir("truncate");
        let mock = MockCodeIntelligence {
            context: CodeContext {
                text: "x".repeat(40_000),
                token_estimate: 10_000,
            },
            ..Default::default()
        };

        let compiler = ContextCompiler::new(Some(Arc::new(mock)), None, 1_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.sections.len(), 1);
        assert!(
            compiled.sections[0]
                .body
                .ends_with("[truncated to fit context budget]")
        );
        assert!(compiled.stats.raw_tokens >= 9_000);
        assert!(compiled.stats.compiled_tokens <= 1_100);
    }

    #[tokio::test]
    async fn budget_exhaustion_drops_lower_priority_section() {
        let dir = temp_dir("exhaustion");
        init_repo(&dir);
        std::fs::write(dir.join("tracked.txt"), "line1\n").unwrap();
        git(&dir, &["add", "tracked.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("tracked.txt"), "line1\nline2\n").unwrap();

        let mock = MockCodeIntelligence {
            context: CodeContext {
                text: "some repo context that would normally show up".to_string(),
                token_estimate: 20,
            },
            ..Default::default()
        };
        let intel: Arc<dyn CodeIntelligence> = Arc::new(mock);

        // First, discover how many tokens the git section costs alone.
        let probe = ContextCompiler::new(None, None, 16_000)
            .compile(&request(), &dir)
            .await;
        assert_eq!(probe.sections.len(), 1);
        let git_tokens = probe.sections[0].tokens;

        // Leave less remaining budget than the intel section costs (12
        // tokens), and below MIN_TRUNCATE_BUDGET, so it is dropped outright
        // rather than truncated.
        let compiler = ContextCompiler::new(Some(intel), None, git_tokens + 5);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.stats.sections_retrieved, 2);
        assert_eq!(compiled.stats.sections_retained, 1);
        assert_eq!(compiled.sections[0].source, ContextSource::Git);
    }

    #[tokio::test]
    async fn intel_error_degrades_to_git_only() {
        let dir = temp_dir("intel-error");
        init_repo(&dir);
        std::fs::write(dir.join("new.txt"), "hi").unwrap();

        let mock = MockCodeIntelligence {
            context_error: Some("connection reset".to_string()),
            ..Default::default()
        };

        let compiler = ContextCompiler::new(Some(Arc::new(mock)), None, 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.sections.len(), 1);
        assert_eq!(compiled.sections[0].source, ContextSource::Git);
        assert!(compiled.stats.intel_status.starts_with("unavailable"));
    }

    fn new_memory(
        kind: Option<MemoryKind>,
        provenance: Option<Provenance>,
        summary: &str,
        body: &str,
        score: f32,
    ) -> Memory {
        Memory {
            id: format!("{summary}-{provenance:?}"),
            kind,
            summary: summary.to_string(),
            body: body.to_string(),
            tags: vec![],
            provenance,
            score,
            project: "kode".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn memory_sections_ordered_after_intel_and_titled() {
        use kode_memory::MockEngineeringMemory;

        let dir = temp_dir("memory-ordering");
        let intel = MockCodeIntelligence {
            context: CodeContext {
                text: "some repo context".to_string(),
                token_estimate: 4,
            },
            ..Default::default()
        };
        let memory = MockEngineeringMemory {
            search_results: vec![
                new_memory(
                    Some(MemoryKind::ProjectRule),
                    Some(Provenance::ExplicitUser),
                    "always prefix with rtk",
                    "always prefix with rtk",
                    0.5,
                ),
                new_memory(
                    Some(MemoryKind::ArchitectureDecision),
                    Some(Provenance::ArchitectureDecision),
                    "use event bus",
                    "use event bus for decoupling",
                    0.5,
                ),
                new_memory(
                    Some(MemoryKind::HistoricalSolution),
                    Some(Provenance::AgentInference),
                    "retry flaky test",
                    "retry flaky test with backoff",
                    0.5,
                ),
            ],
            ..Default::default()
        };

        let compiler = ContextCompiler::new(Some(Arc::new(intel)), Some(Arc::new(memory)), 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.sections.len(), 4);
        assert_eq!(compiled.sections[0].source, ContextSource::CodeIntelligence);
        assert_eq!(compiled.sections[1].source, ContextSource::Memory);
        assert_eq!(compiled.sections[1].title, "Project rules & conventions");
        assert_eq!(compiled.sections[2].source, ContextSource::Memory);
        assert_eq!(compiled.sections[2].title, "Architecture decisions");
        assert_eq!(compiled.sections[3].source, ContextSource::Memory);
        assert_eq!(compiled.sections[3].title, "Past solutions & known issues");

        assert!(
            !compiled.sections[1]
                .body
                .contains("_(inferred, low confidence)_")
        );
        assert!(
            !compiled.sections[2]
                .body
                .contains("_(inferred, low confidence)_")
        );
        assert!(
            compiled.sections[3]
                .body
                .contains("_(inferred, low confidence)_")
        );

        assert_eq!(compiled.stats.memory_status, "ok");
        assert_eq!(compiled.stats.memories_retrieved, 3);
        assert_eq!(compiled.stats.memories_retained, 3);
    }

    #[tokio::test]
    async fn provenance_orders_within_a_section() {
        use kode_memory::MockEngineeringMemory;

        let dir = temp_dir("memory-provenance");
        let memory = MockEngineeringMemory {
            search_results: vec![
                new_memory(
                    Some(MemoryKind::ProjectRule),
                    Some(Provenance::AgentInference),
                    "inferred rule",
                    "inferred rule",
                    0.9,
                ),
                new_memory(
                    Some(MemoryKind::ProjectRule),
                    Some(Provenance::ExplicitUser),
                    "explicit rule",
                    "explicit rule",
                    0.1,
                ),
            ],
            ..Default::default()
        };

        let compiler = ContextCompiler::new(None, Some(Arc::new(memory)), 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.sections.len(), 1);
        let body = &compiled.sections[0].body;
        let explicit_pos = body.find("explicit rule").unwrap();
        let inferred_pos = body.find("inferred rule").unwrap();
        assert!(
            explicit_pos < inferred_pos,
            "explicit-user bullet should sort before agent-inference bullet: {body}"
        );
    }

    #[tokio::test]
    async fn budget_drops_memory_sections_when_tight() {
        use kode_memory::MockEngineeringMemory;

        let dir = temp_dir("memory-budget");
        init_repo(&dir);
        std::fs::write(dir.join("new.txt"), "hi").unwrap();

        let intel = MockCodeIntelligence {
            context: CodeContext {
                text: "some repo context".to_string(),
                token_estimate: 4,
            },
            ..Default::default()
        };
        let memory = MockEngineeringMemory {
            search_results: vec![
                new_memory(
                    Some(MemoryKind::ProjectRule),
                    Some(Provenance::ExplicitUser),
                    "rule one",
                    "rule one",
                    0.5,
                ),
                new_memory(
                    Some(MemoryKind::ArchitectureDecision),
                    Some(Provenance::ArchitectureDecision),
                    "decision one",
                    "decision one",
                    0.5,
                ),
                new_memory(
                    Some(MemoryKind::HistoricalSolution),
                    Some(Provenance::VerifiedTest),
                    "solution one",
                    "solution one",
                    0.5,
                ),
            ],
            ..Default::default()
        };

        // First, discover how many tokens git + intel cost together.
        let probe = ContextCompiler::new(
            Some(Arc::new(MockCodeIntelligence {
                context: CodeContext {
                    text: "some repo context".to_string(),
                    token_estimate: 4,
                },
                ..Default::default()
            })),
            None,
            16_000,
        )
        .compile(&request(), &dir)
        .await;
        assert_eq!(probe.sections.len(), 2);
        let base_tokens: usize = probe.sections.iter().map(|s| s.tokens).sum();

        // Leave just enough for git + intel, nothing for memory sections.
        let compiler =
            ContextCompiler::new(Some(Arc::new(intel)), Some(Arc::new(memory)), base_tokens);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.stats.memories_retrieved, 3);
        assert_eq!(compiled.stats.memories_retained, 0);
        assert!(
            compiled
                .sections
                .iter()
                .all(|s| s.source != ContextSource::Memory)
        );
    }

    #[tokio::test]
    async fn memory_error_degrades_without_affecting_other_sections() {
        use kode_memory::MockEngineeringMemory;

        let dir = temp_dir("memory-error");
        init_repo(&dir);
        std::fs::write(dir.join("new.txt"), "hi").unwrap();

        let memory = MockEngineeringMemory {
            search_error: Some("index down".to_string()),
            ..Default::default()
        };

        let compiler = ContextCompiler::new(None, Some(Arc::new(memory)), 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.sections.len(), 1);
        assert_eq!(compiled.sections[0].source, ContextSource::Git);
        assert!(compiled.stats.memory_status.starts_with("unavailable"));
        assert_eq!(compiled.stats.memories_retrieved, 0);
    }

    #[tokio::test]
    async fn memory_none_yields_disabled_status() {
        let dir = temp_dir("memory-none");
        let compiler = ContextCompiler::new(None, None, 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert_eq!(compiled.stats.memory_status, "disabled");
    }

    #[tokio::test]
    async fn summary_line_includes_ingat_when_memory_sections_present() {
        use kode_memory::MockEngineeringMemory;

        let dir = temp_dir("memory-summary");
        let memory = MockEngineeringMemory {
            search_results: vec![new_memory(
                Some(MemoryKind::ProjectRule),
                Some(Provenance::ExplicitUser),
                "rule one",
                "rule one",
                0.5,
            )],
            ..Default::default()
        };

        let compiler = ContextCompiler::new(None, Some(Arc::new(memory)), 16_000);
        let compiled = compiler.compile(&request(), &dir).await;

        assert!(compiled.summary_line().contains("ingat"));
    }
}
