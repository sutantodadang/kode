use std::path::Path;
use std::sync::Arc;

use kode_agent::Agent;
use kode_context::{CompiledContext, ContextCompiler, ContextRequest, ContextSource};
use kode_core::CancellationToken;
use kode_core::config::KodeConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_intel::{CodeIntelligence, ZindeksAdapter};
use kode_memory::{EngineeringMemory, IngatAdapter, RememberTool};
use kode_model::{OpenAiModel, OpenAiOptions};
use kode_tools::ToolContext;
use kode_tools::permission::PermissionHandler;
use kode_tools::registry::{ToolRegistry, ToolRuntime};

/// Runs one agentic task end-to-end: model/config setup, code intelligence
/// and engineering memory binding, context compilation, the agent loop, and
/// post-edit verification with a single retry. This is the single code path
/// shared by `kode exec` (headless) and the TUI — it communicates *only*
/// through `events`, never via stdout/stderr directly.
pub async fn run_task(
    task: &str,
    cwd: &Path,
    config: &KodeConfig,
    events: EventBus,
    handler: Arc<dyn PermissionHandler>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    if config.model.model.is_empty() {
        anyhow::bail!("set model.model in .kode/config.toml");
    }

    let model: Arc<dyn kode_model::Model> = match config.model.provider.as_str() {
        "openai" => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .or_else(|_| std::env::var("KODE_API_KEY"))
                .map_err(|_| anyhow::anyhow!("set OPENAI_API_KEY to run `kode exec`"))?;

            let mut opts = OpenAiOptions {
                api_key,
                model: config.model.model.clone(),
                ..Default::default()
            };
            if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
                opts.base_url = base_url;
            }
            Arc::new(OpenAiModel::new(opts)) as Arc<dyn kode_model::Model>
        }
        "codex" => {
            let auth_path = kode_model::codex::default_auth_path()
                .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory for codex auth"))?;
            let auth = kode_model::codex::load(&auth_path).map_err(|e| anyhow::anyhow!("{e}"))?;

            if auth.auth_mode == "apikey" {
                let api_key = auth.api_key.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "codex auth.json has auth_mode=apikey but no OPENAI_API_KEY — run: kode auth login codex"
                    )
                })?;
                let opts = OpenAiOptions {
                    api_key,
                    model: config.model.model.clone(),
                    ..Default::default()
                };
                Arc::new(OpenAiModel::new(opts)) as Arc<dyn kode_model::Model>
            } else {
                let codex_model =
                    kode_model::CodexModel::new(auth_path, config.model.model.clone())
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                Arc::new(codex_model) as Arc<dyn kode_model::Model>
            }
        }
        "opencode-go" | "opencode" | "kilo" | "lmstudio" => {
            let auth_path = kode_model::opencode::default_auth_path().ok_or_else(|| {
                anyhow::anyhow!("cannot resolve home directory for opencode auth")
            })?;
            // Base URLs come from the builtin gateway table only; no reads of
            // another tool's config.
            let opencode_model = kode_model::opencode::resolve(
                &config.model.provider,
                config.model.model.clone(),
                &auth_path,
                None,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            Arc::new(opencode_model) as Arc<dyn kode_model::Model>
        }
        other => anyhow::bail!(
            "provider {other} not supported yet (supported: openai, codex, opencode-go, opencode, kilo, lmstudio)"
        ),
    };

    let ctx = ToolContext {
        workspace_root: cwd.to_path_buf(),
        cancel,
    };

    // Keep a concrete handle alongside the `dyn CodeIntelligence` one so we
    // can call `ensure_bound` again (as an incremental refresh) after edits.
    let mut zindeks_adapter: Option<Arc<ZindeksAdapter>> = None;
    let intel: Option<Arc<dyn CodeIntelligence>> = if config.zindeks.enabled {
        match ZindeksAdapter::connect(&config.zindeks, cwd).await {
            Ok(adapter) => {
                let adapter = Arc::new(adapter);
                match adapter.ensure_bound().await {
                    Ok(()) => {
                        zindeks_adapter = Some(adapter.clone());
                        Some(adapter as Arc<dyn CodeIntelligence>)
                    }
                    Err(e) => {
                        events.emit(KodeEvent::Note {
                            text: format!("code intelligence unavailable: {e}"),
                        });
                        None
                    }
                }
            }
            Err(e) => {
                events.emit(KodeEvent::Note {
                    text: format!("code intelligence unavailable: {e}"),
                });
                None
            }
        }
    } else {
        None
    };

    let memory: Option<Arc<dyn EngineeringMemory>> = if config.ingat.enabled {
        let adapter = IngatAdapter::new(&config.ingat);
        match tokio::time::timeout(std::time::Duration::from_secs(3), adapter.health()).await {
            Ok(Ok(())) => Some(Arc::new(adapter) as Arc<dyn EngineeringMemory>),
            Ok(Err(e)) => {
                events.emit(KodeEvent::Note {
                    text: format!("engineering memory unavailable: {e}"),
                });
                None
            }
            Err(_) => {
                events.emit(KodeEvent::Note {
                    text: "engineering memory unavailable: request timed out".to_string(),
                });
                None
            }
        }
    } else {
        None
    };

    let mut registry = ToolRegistry::with_builtins();
    if let Some(mem) = &memory {
        let repository = cwd
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        registry.register(Arc::new(RememberTool::new(mem.clone(), repository)));
    }

    // Generic external MCP servers (kept architecturally separate from the
    // first-class Zindeks/Ingat integrations above). `_mcp_manager` owns the
    // spawned child processes and must outlive the agent run.
    let _mcp_manager = if !config.mcp.servers.is_empty() {
        let mut notes = Vec::new();
        let manager = kode_mcp::McpManager::connect_all(&config.mcp.servers, &mut notes).await;
        for text in notes {
            events.emit(KodeEvent::Note { text });
        }
        for handle in &manager.handles {
            for tool in &handle.tools {
                registry.register(tool.clone());
            }
        }
        Some(manager)
    } else {
        None
    };

    let tools = ToolRuntime::new(registry, config.permissions.default_mode, handler);
    let effort = if config.model.effort.is_empty() {
        None
    } else {
        Some(config.model.effort.clone())
    };
    let agent = Agent::new(model, tools, events.clone(), &config.agent).with_effort(effort);

    events.emit(KodeEvent::ContextCompilationStarted);
    let compiler = ContextCompiler::new(intel, memory, config.agent.context_budget_tokens as usize);
    let compiled = compiler
        .compile(
            &ContextRequest {
                task: task.to_string(),
                working_set: vec![],
            },
            cwd,
        )
        .await;
    events.emit(KodeEvent::ContextCompiled {
        token_estimate: compiled.token_estimate(),
        sections: compiled.sections.len(),
    });
    events.emit(knowledge_from(
        &compiled,
        config.agent.context_budget_tokens as usize,
    ));
    events.emit(KodeEvent::Note {
        text: compiled.summary_line(),
    });

    let mut outcome = agent
        .run_with_context(task, compiled.render().as_deref(), &ctx)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    let mut final_mutated = outcome.mutated;

    if outcome.mutated {
        let profile = kode_verify::detect(cwd);
        events.emit(KodeEvent::VerificationStarted);
        let mut report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
        emit_step_lines(&events, &report);
        events.emit(KodeEvent::VerificationFinished { ok: report.ok });

        if !report.ok {
            events.emit(KodeEvent::Note {
                text: "verification failed — asking agent to fix".to_string(),
            });
            let retry_task = format!(
                "Verification failed after your previous changes. Fix the failures, then stop.\n\n{}\n\nOriginal task: {}",
                report.render(),
                task
            );

            let retry_outcome = agent
                .run_with_context(&retry_task, compiled.render().as_deref(), &ctx)
                .await
                .map_err(|err| anyhow::anyhow!(err))?;

            final_mutated = retry_outcome.mutated;
            outcome = retry_outcome;

            if outcome.mutated {
                let profile = kode_verify::detect(cwd);
                events.emit(KodeEvent::VerificationStarted);
                report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
                emit_step_lines(&events, &report);
                events.emit(KodeEvent::VerificationFinished { ok: report.ok });
            }
            events.emit(KodeEvent::Note {
                text: report.summary_line(),
            });
        }
    }

    if let Some(adapter) = zindeks_adapter.as_ref()
        && final_mutated
    {
        match adapter.ensure_bound().await {
            Ok(()) => events.emit(KodeEvent::Note {
                text: "zindeks index refreshed".to_string(),
            }),
            Err(e) => events.emit(KodeEvent::Note {
                text: format!("zindeks refresh failed (non-fatal): {e}"),
            }),
        }
    }

    events.emit(KodeEvent::TaskFinished {
        iterations: outcome.iterations,
        tool_calls: outcome.tool_calls,
        input_tokens: outcome.usage.input_tokens,
        output_tokens: outcome.usage.output_tokens,
    });

    Ok(())
}

/// Builds a `KodeEvent::Knowledge` digest from a compiled context. Pure —
/// no I/O, safe to unit test with a hand-built `CompiledContext`.
fn knowledge_from(compiled: &CompiledContext, budget: usize) -> KodeEvent {
    KodeEvent::Knowledge {
        zindeks: zindeks_lines(compiled),
        ingat: ingat_lines(compiled),
        git: git_lines(compiled),
        context_tokens: compiled.stats.compiled_tokens,
        budget_tokens: budget,
    }
}

/// Up to 3 distinct `**path**`-style file headers pulled from the
/// CodeIntelligence section body(ies), rendered as `path (score)` when a
/// trailing `(...)` is present, else just `path`. Falls back to a section
/// count/token summary when no such headers parse.
fn zindeks_lines(compiled: &CompiledContext) -> Vec<String> {
    let intel_sections: Vec<&kode_context::ContextSection> = compiled
        .sections
        .iter()
        .filter(|s| s.source == ContextSource::CodeIntelligence)
        .collect();
    if intel_sections.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    for section in &intel_sections {
        for raw in section.body.lines() {
            if let Some(parsed) = parse_zindeks_header(raw)
                && !lines.contains(&parsed)
            {
                lines.push(parsed);
                if lines.len() == 3 {
                    return lines;
                }
            }
        }
    }

    if lines.is_empty() {
        let tokens: usize = intel_sections.iter().map(|s| s.tokens).sum();
        vec![format!(
            "{} context sections · {tokens} tokens",
            intel_sections.len()
        )]
    } else {
        lines
    }
}

/// Parses a markdown file-header line like `**src/foo.rs** (0.83)` into
/// `"src/foo.rs (0.83)"`, or `**src/foo.rs**` into `"src/foo.rs"`. Returns
/// `None` when `line` isn't a `**...**`-style header.
fn parse_zindeks_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("**")?;
    let (path, after) = rest.split_once("**")?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let after = after.trim();
    if after.starts_with('(') && after.ends_with(')') {
        Some(format!("{path} {after}"))
    } else {
        Some(path.to_string())
    }
}

/// First bullet's summary text (leading `- **[kind]** ` stripped) per
/// Memory-source section, max 2, each truncated to 60 chars (char-safe).
fn ingat_lines(compiled: &CompiledContext) -> Vec<String> {
    let mut lines = Vec::new();
    for section in compiled
        .sections
        .iter()
        .filter(|s| s.source == ContextSource::Memory)
    {
        if let Some(first_bullet) = section.body.lines().next() {
            lines.push(truncate_chars(strip_bullet_prefix(first_bullet), 60));
            if lines.len() == 2 {
                break;
            }
        }
    }
    lines
}

/// Strips the `- **[kind]** ` prefix `format_memory_bullet` in
/// `kode-context` adds ahead of a memory's summary text.
fn strip_bullet_prefix(bullet: &str) -> &str {
    let trimmed = bullet.trim_start();
    let Some(after_dash) = trimmed.strip_prefix("- ") else {
        return trimmed;
    };
    let Some(after_open) = after_dash.strip_prefix("**[") else {
        return after_dash;
    };
    match after_open.split_once("]** ") {
        Some((_, rest)) => rest,
        None => after_open,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max).collect();
        truncated.push('…');
        truncated
    }
}

/// `"{n} files changed"` from the Git section's `status:` block, or an
/// empty vec when there is no Git section (clean tree / not a repo).
fn git_lines(compiled: &CompiledContext) -> Vec<String> {
    let Some(section) = compiled
        .sections
        .iter()
        .find(|s| s.source == ContextSource::Git)
    else {
        return Vec::new();
    };
    let after_status = section
        .body
        .strip_prefix("status:\n")
        .unwrap_or(&section.body);
    let status_block = after_status
        .split("\n\ndiff:\n")
        .next()
        .unwrap_or(after_status);
    let n = status_block
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    if n == 0 {
        Vec::new()
    } else {
        vec![format!("{n} files changed")]
    }
}

fn emit_step_lines(events: &EventBus, report: &kode_verify::VerificationReport) {
    for step in &report.steps {
        let tag = match &step.status {
            kode_verify::StepStatus::Passed => "PASS",
            kode_verify::StepStatus::Failed => "FAIL",
            kode_verify::StepStatus::Skipped(_) => "SKIP",
        };
        events.emit(KodeEvent::Note {
            text: format!("{}: {tag}", step.name),
        });
    }
}

#[cfg(test)]
mod knowledge_tests {
    use super::*;
    use kode_context::{ContextSection, ContextStats};

    fn section(source: ContextSource, title: &str, body: &str) -> ContextSection {
        ContextSection {
            source,
            title: title.to_string(),
            body: body.to_string(),
            tokens: body.len().div_ceil(4),
        }
    }

    fn compiled(sections: Vec<ContextSection>, compiled_tokens: usize) -> CompiledContext {
        CompiledContext {
            sections,
            stats: ContextStats {
                compiled_tokens,
                ..Default::default()
            },
        }
    }

    #[test]
    fn knowledge_from_empty_compiled_yields_all_empty_vecs() {
        let c = compiled(vec![], 0);
        let ev = knowledge_from(&c, 16_000);
        match ev {
            KodeEvent::Knowledge {
                zindeks,
                ingat,
                git,
                context_tokens,
                budget_tokens,
            } => {
                assert!(zindeks.is_empty());
                assert!(ingat.is_empty());
                assert!(git.is_empty());
                assert_eq!(context_tokens, 0);
                assert_eq!(budget_tokens, 16_000);
            }
            other => panic!("expected Knowledge, got {other:?}"),
        }
    }

    #[test]
    fn knowledge_from_full_sections_extracts_all_three_sources() {
        let intel_body = "**src/foo.rs** (0.91)\nsome context\n**src/bar.rs** (0.80)\n**src/baz.rs**\n**src/qux.rs**\n";
        let memory_body = "- **[project-rule]** always prefix shell commands with rtk immediately every single time no exceptions — full body text here";
        let git_body = "status:\nM foo.rs\nA bar.rs\n\ndiff:\n+ line\n- line";

        let c = compiled(
            vec![
                section(
                    ContextSource::CodeIntelligence,
                    "Repository context",
                    intel_body,
                ),
                section(
                    ContextSource::Memory,
                    "Project rules & conventions",
                    memory_body,
                ),
                section(ContextSource::Git, "Uncommitted changes", git_body),
            ],
            4200,
        );

        let ev = knowledge_from(&c, 16_000);
        match ev {
            KodeEvent::Knowledge {
                zindeks,
                ingat,
                git,
                context_tokens,
                budget_tokens,
            } => {
                assert_eq!(
                    zindeks,
                    vec![
                        "src/foo.rs (0.91)".to_string(),
                        "src/bar.rs (0.80)".to_string(),
                        "src/baz.rs".to_string(),
                    ]
                );
                assert_eq!(
                    ingat,
                    vec![
                        "always prefix shell commands with rtk immediately every sing…".to_string()
                    ]
                );
                assert_eq!(git, vec!["2 files changed".to_string()]);
                assert_eq!(context_tokens, 4200);
                assert_eq!(budget_tokens, 16_000);
            }
            other => panic!("expected Knowledge, got {other:?}"),
        }
    }

    #[test]
    fn knowledge_from_unparseable_zindeks_markdown_falls_back_to_summary() {
        let intel_body = "no bold headers here\njust plain repository context text";
        let c = compiled(
            vec![section(
                ContextSource::CodeIntelligence,
                "Repository context",
                intel_body,
            )],
            10,
        );

        let ev = knowledge_from(&c, 16_000);
        match ev {
            KodeEvent::Knowledge { zindeks, .. } => {
                assert_eq!(zindeks.len(), 1);
                assert!(zindeks[0].contains("context sections"));
                assert!(zindeks[0].contains("tokens"));
            }
            other => panic!("expected Knowledge, got {other:?}"),
        }
    }

    #[test]
    fn git_lines_empty_when_no_git_section() {
        let c = compiled(
            vec![section(
                ContextSource::CodeIntelligence,
                "Repository context",
                "**src/foo.rs**",
            )],
            10,
        );
        assert!(git_lines(&c).is_empty());
    }
}
