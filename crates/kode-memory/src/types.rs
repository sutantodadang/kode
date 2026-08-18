/// The kind of durable engineering knowledge a [`Memory`] captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    ProjectRule,
    ArchitectureDecision,
    Convention,
    KnownIssue,
    BuildKnowledge,
    RejectedApproach,
    UserPreference,
    HistoricalSolution,
}

impl MemoryKind {
    /// All variants, in a stable order — used for CLI help/validation.
    pub const ALL: [MemoryKind; 8] = [
        MemoryKind::ProjectRule,
        MemoryKind::ArchitectureDecision,
        MemoryKind::Convention,
        MemoryKind::KnownIssue,
        MemoryKind::BuildKnowledge,
        MemoryKind::RejectedApproach,
        MemoryKind::UserPreference,
        MemoryKind::HistoricalSolution,
    ];

    pub fn as_kebab(&self) -> &'static str {
        match self {
            MemoryKind::ProjectRule => "project-rule",
            MemoryKind::ArchitectureDecision => "architecture-decision",
            MemoryKind::Convention => "convention",
            MemoryKind::KnownIssue => "known-issue",
            MemoryKind::BuildKnowledge => "build-knowledge",
            MemoryKind::RejectedApproach => "rejected-approach",
            MemoryKind::UserPreference => "user-preference",
            MemoryKind::HistoricalSolution => "historical-solution",
        }
    }

    pub fn from_kebab(s: &str) -> Option<Self> {
        match s {
            "project-rule" => Some(MemoryKind::ProjectRule),
            "architecture-decision" => Some(MemoryKind::ArchitectureDecision),
            "convention" => Some(MemoryKind::Convention),
            "known-issue" => Some(MemoryKind::KnownIssue),
            "build-knowledge" => Some(MemoryKind::BuildKnowledge),
            "rejected-approach" => Some(MemoryKind::RejectedApproach),
            "user-preference" => Some(MemoryKind::UserPreference),
            "historical-solution" => Some(MemoryKind::HistoricalSolution),
            _ => None,
        }
    }
}

/// Where a [`Memory`] came from — how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    ExplicitUser,
    VerifiedCode,
    VerifiedTest,
    ArchitectureDecision,
    AgentInference,
}

impl Provenance {
    pub fn as_kebab(&self) -> &'static str {
        match self {
            Provenance::ExplicitUser => "explicit-user",
            Provenance::VerifiedCode => "verified-code",
            Provenance::VerifiedTest => "verified-test",
            Provenance::ArchitectureDecision => "architecture-decision",
            Provenance::AgentInference => "agent-inference",
        }
    }

    pub fn from_kebab(s: &str) -> Option<Self> {
        match s {
            "explicit-user" => Some(Provenance::ExplicitUser),
            "verified-code" => Some(Provenance::VerifiedCode),
            "verified-test" => Some(Provenance::VerifiedTest),
            "architecture-decision" => Some(Provenance::ArchitectureDecision),
            "agent-inference" => Some(Provenance::AgentInference),
            _ => None,
        }
    }
}

/// Code-relation metadata attached to a memory: what repository state it was
/// captured against, and which files/symbols it concerns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryContext {
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
}

/// A memory to be written via [`crate::EngineeringMemory::remember`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemory {
    pub kind: MemoryKind,
    pub summary: String,
    pub body: String,
    pub tags: Vec<String>,
    pub provenance: Provenance,
    pub context: MemoryContext,
    /// Explicit opt-in to share this memory with the team via the
    /// git-backed `.kode/memory/team.jsonl` file (see `kode remember
    /// --team` / `RememberTool`'s `team` arg). Defaults to `false` —
    /// sharing is always explicit, never inferred from `kind`.
    pub team: bool,
}

/// A memory read back via [`crate::EngineeringMemory::search`].
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: String,
    /// `None` when the stored kind doesn't map back to a Kode
    /// [`MemoryKind`] (e.g. content written by another Ingat client).
    pub kind: Option<MemoryKind>,
    pub summary: String,
    pub body: String,
    /// User-authored tags only; Kode's provenance/kind/code-relation tags
    /// are parsed out into their own fields.
    pub tags: Vec<String>,
    pub provenance: Option<Provenance>,
    pub score: f32,
    pub project: String,
    pub created_at: String,
}

/// A search over stored memories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryQuery {
    pub text: String,
    /// Maps to Ingat's `filters.project`.
    pub repository: Option<String>,
    pub kind: Option<MemoryKind>,
    pub limit: u32,
}

impl Default for MemoryQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            repository: None,
            kind: None,
            limit: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_kind_kebab_round_trips() {
        for kind in MemoryKind::ALL {
            let kebab = kind.as_kebab();
            assert_eq!(MemoryKind::from_kebab(kebab), Some(kind));
        }
    }

    #[test]
    fn memory_kind_from_kebab_rejects_unknown() {
        assert_eq!(MemoryKind::from_kebab("not-a-kind"), None);
    }

    #[test]
    fn provenance_kebab_round_trips() {
        let all = [
            Provenance::ExplicitUser,
            Provenance::VerifiedCode,
            Provenance::VerifiedTest,
            Provenance::ArchitectureDecision,
            Provenance::AgentInference,
        ];
        for provenance in all {
            let kebab = provenance.as_kebab();
            assert_eq!(Provenance::from_kebab(kebab), Some(provenance));
        }
    }

    #[test]
    fn provenance_from_kebab_rejects_unknown() {
        assert_eq!(Provenance::from_kebab("not-a-provenance"), None);
    }

    #[test]
    fn memory_query_default_has_empty_text_and_limit_eight() {
        let q = MemoryQuery::default();
        assert_eq!(q.text, "");
        assert_eq!(q.limit, 8);
        assert_eq!(q.repository, None);
        assert_eq!(q.kind, None);
    }
}
