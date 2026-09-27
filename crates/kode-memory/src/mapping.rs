//! Shared Kode <-> Ingat conversions used by both the HTTP and the embedded
//! memory backends, so the kind/tag/provenance/scope rules live in one place.

use crate::types::{MemoryKind, NewMemory, Provenance};

/// Maximum number of `sym:` tags attached to a stored memory, so a memory with
/// a long symbol list doesn't blow up Ingat's tag list unboundedly.
pub(crate) const MAX_SYMBOL_TAGS: usize = 8;

/// Maps a Kode [`MemoryKind`] onto Ingat's wire kind name: `HistoricalSolution`
/// has a direct counterpart (`"fix-history"`); everything else rides as its
/// kebab name (Ingat `Other(<kebab>)`).
pub(crate) fn kind_wire_name(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::HistoricalSolution => "fix-history",
        other => other.as_kebab(),
    }
}

/// Best-effort reverse of [`kind_wire_name`], used only as a fallback when a
/// result carries no (or an unrecognized) `kode-kind:` tag.
pub(crate) fn kind_from_wire_name(name: &str) -> Option<MemoryKind> {
    match name {
        "fix-history" => Some(MemoryKind::HistoricalSolution),
        other => MemoryKind::from_kebab(other),
    }
}

/// The stored content: `body`, falling back to `summary` when body is empty.
pub(crate) fn remember_body(memory: &NewMemory) -> String {
    if memory.body.is_empty() {
        memory.summary.clone()
    } else {
        memory.body.clone()
    }
}

/// Project name sent to Ingat: the repository, or `"unknown"` when unset.
pub(crate) fn remember_project(memory: &NewMemory) -> String {
    memory
        .context
        .repository
        .clone()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Builds the tag list sent to Ingat: user tags followed by Kode's bookkeeping
/// tags (`kode`, `kode-kind:*`, `provenance:*`, `branch:*`, `commit:*`,
/// `sym:*`).
pub(crate) fn remember_tags(memory: &NewMemory) -> Vec<String> {
    let mut tags = memory.tags.clone();
    tags.push("kode".to_string());
    tags.push(format!("kode-kind:{}", memory.kind.as_kebab()));
    tags.push(format!("provenance:{}", memory.provenance.as_kebab()));
    if let Some(branch) = &memory.context.branch {
        tags.push(format!("branch:{branch}"));
    }
    if let Some(commit) = &memory.context.commit {
        tags.push(format!("commit:{commit}"));
    }
    for symbol in memory.context.symbols.iter().take(MAX_SYMBOL_TAGS) {
        tags.push(format!("sym:{symbol}"));
    }
    tags
}

/// Splits an Ingat tag list into Kode's parsed-out fields (kind, provenance)
/// plus the remaining user-authored tags. Kode's own bookkeeping tags never
/// surface as user-visible tags.
pub(crate) fn split_tags(
    tags: Vec<String>,
) -> (Option<MemoryKind>, Option<Provenance>, Vec<String>) {
    let mut kind = None;
    let mut provenance = None;
    let mut rest = Vec::new();

    for tag in tags {
        if tag == "kode" {
            continue;
        } else if let Some(kebab) = tag.strip_prefix("kode-kind:") {
            kind = MemoryKind::from_kebab(kebab);
        } else if let Some(kebab) = tag.strip_prefix("provenance:") {
            provenance = Provenance::from_kebab(kebab);
        } else if tag.starts_with("branch:")
            || tag.starts_with("commit:")
            || tag.starts_with("sym:")
        {
            continue;
        } else {
            rest.push(tag);
        }
    }

    (kind, provenance, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryContext;

    fn memory(kind: MemoryKind, body: &str) -> NewMemory {
        NewMemory {
            kind,
            summary: "summary".to_string(),
            body: body.to_string(),
            tags: vec!["user-tag".to_string()],
            provenance: Provenance::ExplicitUser,
            context: MemoryContext {
                repository: Some("kode".to_string()),
                branch: Some("main".to_string()),
                commit: Some("abc".to_string()),
                files: vec![],
                symbols: vec!["Foo::bar".to_string()],
            },
            team: false,
        }
    }

    #[test]
    fn kind_wire_name_maps_historical_solution_and_round_trips() {
        assert_eq!(
            kind_wire_name(MemoryKind::HistoricalSolution),
            "fix-history"
        );
        assert_eq!(
            kind_from_wire_name("fix-history"),
            Some(MemoryKind::HistoricalSolution)
        );
        assert_eq!(kind_wire_name(MemoryKind::ProjectRule), "project-rule");
        assert_eq!(
            kind_from_wire_name("project-rule"),
            Some(MemoryKind::ProjectRule)
        );
    }

    #[test]
    fn remember_body_falls_back_to_summary() {
        let mut m = memory(MemoryKind::Convention, "");
        assert_eq!(remember_body(&m), "summary");
        m.body = "real body".to_string();
        assert_eq!(remember_body(&m), "real body");
    }

    #[test]
    fn remember_tags_include_bookkeeping_and_user_tags() {
        let tags = remember_tags(&memory(MemoryKind::ProjectRule, "b"));
        assert!(tags.contains(&"user-tag".to_string()));
        assert!(tags.contains(&"kode".to_string()));
        assert!(tags.contains(&"kode-kind:project-rule".to_string()));
        assert!(tags.contains(&"provenance:explicit-user".to_string()));
        assert!(tags.contains(&"branch:main".to_string()));
        assert!(tags.contains(&"commit:abc".to_string()));
        assert!(tags.contains(&"sym:Foo::bar".to_string()));
    }

    #[test]
    fn split_tags_recovers_kind_provenance_and_user_tags() {
        let (kind, prov, rest) = split_tags(vec![
            "kode".to_string(),
            "kode-kind:convention".to_string(),
            "provenance:verified-code".to_string(),
            "sym:X".to_string(),
            "user-tag".to_string(),
        ]);
        assert_eq!(kind, Some(MemoryKind::Convention));
        assert_eq!(prov, Some(Provenance::VerifiedCode));
        assert_eq!(rest, vec!["user-tag".to_string()]);
    }
}
