/// Estimates the token cost of `text` using a `chars/4` heuristic (ceil).
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn format_tokens(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// A request to compile context for an agent task.
pub struct ContextRequest {
    pub task: String,
    pub working_set: Vec<String>,
}

/// Where a [`ContextSection`] came from.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextSource {
    Git,
    CodeIntelligence,
    Memory,
}

/// A single titled block of compiled context, already token-budgeted.
#[derive(Debug, Clone)]
pub struct ContextSection {
    pub source: ContextSource,
    pub title: String,
    pub body: String,
    /// Token estimate of `body` after any truncation.
    pub tokens: usize,
}

/// Retrieval/retention accounting for a [`CompiledContext`].
#[derive(Debug, Clone, Default)]
pub struct ContextStats {
    /// Sum of section token estimates before budgeting.
    pub raw_tokens: usize,
    /// Sum of section token estimates after budgeting.
    pub compiled_tokens: usize,
    pub sections_retrieved: usize,
    pub sections_retained: usize,
    /// "ok" | "disabled" | "unavailable: <err>" | "not indexed"
    pub intel_status: String,
    /// "ok" | "disabled" | "unavailable: <err>"
    pub memory_status: String,
    pub memories_retrieved: usize,
    pub memories_retained: usize,
    pub memories_dropped: usize,
    pub sections_truncated: usize,
    pub sections_dropped: usize,
}

/// The result of [`crate::ContextCompiler::compile`]: a deterministic,
/// priority-ordered, token-budgeted set of context sections.
pub struct CompiledContext {
    pub sections: Vec<ContextSection>,
    pub stats: ContextStats,
}

impl CompiledContext {
    pub fn token_estimate(&self) -> usize {
        self.stats.compiled_tokens
    }

    /// Renders the compiled context as markdown, or `None` when there are no
    /// sections to render.
    pub fn render(&self) -> Option<String> {
        if self.sections.is_empty() {
            return None;
        }
        let mut out = String::new();
        for section in &self.sections {
            out.push_str(&format!("## {}\n\n{}\n", section.title, section.body));
        }
        Some(out)
    }

    /// A one-line human-readable summary, e.g.
    /// `context: git 412, zindeks 3.1k — 3.5k tokens (raw 9.2k, -62%)`.
    pub fn summary_line(&self) -> String {
        if self.sections.is_empty() {
            return "context: none — 0 tokens".to_string();
        }

        let mut parts: Vec<String> = Vec::new();
        for (source, label) in [
            (ContextSource::Git, "git"),
            (ContextSource::CodeIntelligence, "zindeks"),
            (ContextSource::Memory, "ingat"),
        ] {
            if !self.sections.iter().any(|s| s.source == source) {
                continue;
            }
            let tokens: usize = self
                .sections
                .iter()
                .filter(|s| s.source == source)
                .map(|s| s.tokens)
                .sum();
            parts.push(format!("{label} {}", format_tokens(tokens)));
        }

        let mut line = format!(
            "context: {} — {} tokens",
            parts.join(", "),
            format_tokens(self.stats.compiled_tokens)
        );

        if self.stats.raw_tokens != self.stats.compiled_tokens {
            let pct = if self.stats.raw_tokens > 0 {
                100.0 * (1.0 - self.stats.compiled_tokens as f64 / self.stats.raw_tokens as f64)
            } else {
                0.0
            };
            line.push_str(&format!(
                " (raw {}, -{pct:.0}%)",
                format_tokens(self.stats.raw_tokens)
            ));
        }

        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn render_none_when_empty() {
        let compiled = CompiledContext {
            sections: vec![],
            stats: ContextStats::default(),
        };
        assert_eq!(compiled.render(), None);
        assert_eq!(compiled.summary_line(), "context: none — 0 tokens");
    }

    #[test]
    fn render_contains_headers() {
        let compiled = CompiledContext {
            sections: vec![ContextSection {
                source: ContextSource::Git,
                title: "Uncommitted changes".to_string(),
                body: "status:\nM foo.rs".to_string(),
                tokens: 4,
            }],
            stats: ContextStats {
                raw_tokens: 4,
                compiled_tokens: 4,
                sections_retrieved: 1,
                sections_retained: 1,
                intel_status: "disabled".to_string(),
                memory_status: "disabled".to_string(),
                memories_retrieved: 0,
                memories_retained: 0,
                memories_dropped: 0,
                sections_truncated: 0,
                sections_dropped: 0,
            },
        };
        let rendered = compiled.render().unwrap();
        assert!(rendered.contains("## Uncommitted changes"));
        assert!(rendered.contains("M foo.rs"));
        assert!(compiled.summary_line().contains("git 4"));
    }
}
