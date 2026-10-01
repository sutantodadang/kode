use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, ToolError};

const MAX_DISCOVERY_DEPTH: usize = 5;
const MAX_SKILL_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROMPT_SKILLS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 140;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub root: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    entries: BTreeMap<String, SkillEntry>,
}

impl SkillCatalog {
    pub fn discover(workspace: &Path) -> Self {
        let mut roots = vec![
            (workspace.join(".kode/skills"), "project:.kode"),
            (workspace.join(".agents/skills"), "project:.agents"),
            (workspace.join(".codex/skills"), "project:.codex"),
            (workspace.join(".claude/skills"), "project:.claude"),
        ];
        if let Some(home) = user_home() {
            roots.extend([
                (home.join(".kode/skills"), "user:.kode"),
                (home.join(".agents/skills"), "user:.agents"),
                (home.join(".codex/skills"), "user:.codex"),
                (home.join(".claude/skills"), "user:.claude"),
            ]);
        }
        Self::discover_roots(roots)
    }

    fn discover_roots<I, S>(roots: I) -> Self
    where
        I: IntoIterator<Item = (PathBuf, S)>,
        S: Into<String>,
    {
        let mut catalog = Self::default();
        let mut visited = HashSet::new();
        for (root, source) in roots {
            catalog.scan_dir(&root, &source.into(), 0, &mut visited);
        }
        catalog
    }

    fn scan_dir(&mut self, dir: &Path, source: &str, depth: usize, visited: &mut HashSet<PathBuf>) {
        if depth > MAX_DISCOVERY_DEPTH {
            return;
        }
        let Ok(canonical) = std::fs::canonicalize(dir) else {
            return;
        };
        if !visited.insert(canonical) {
            return;
        }
        let Ok(children) = std::fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<_> = children.flatten().collect();
        children.sort_by_key(|child| child.path());
        for child in children {
            let path = child.path();
            if path.is_dir() {
                self.scan_dir(&path, source, depth + 1, visited);
            } else if path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
                && std::fs::metadata(&path)
                    .is_ok_and(|metadata| metadata.len() <= MAX_SKILL_FILE_BYTES)
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let fallback = path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unnamed-skill".to_string());
                let (name, description) = parse_frontmatter(&content, &fallback);
                let key = normalize_name(&name);
                self.entries.entry(key).or_insert_with(|| SkillEntry {
                    name,
                    description,
                    root: path.parent().unwrap_or(dir).to_path_buf(),
                    source: source.to_string(),
                });
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn prompt_summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::from(
            "Available skills (progressive disclosure; call `use_skill` before following one):\n",
        );
        for entry in self.entries.values().take(MAX_PROMPT_SKILLS) {
            out.push_str("- ");
            out.push_str(&entry.name);
            out.push_str(": ");
            out.push_str(&truncate_chars(&entry.description, MAX_DESCRIPTION_CHARS));
            out.push('\n');
        }
        if self.len() > MAX_PROMPT_SKILLS {
            out.push_str(&format!(
                "- ... and {} more discoverable through `use_skill`\n",
                self.len() - MAX_PROMPT_SKILLS
            ));
        }
        Some(out)
    }

    pub fn read(&self, name: &str, relative_path: Option<&str>) -> Result<String> {
        let key = normalize_name(name.trim_start_matches('$'));
        let entry = self
            .entries
            .get(&key)
            .ok_or_else(|| ToolError::Failed(format!("skill not found: {name}")))?;
        let relative = relative_path.unwrap_or("SKILL.md");
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ToolError::Failed(format!(
                "skill resource path must stay inside the skill: {relative}"
            )));
        }

        let root = std::fs::canonicalize(&entry.root)?;
        let target = std::fs::canonicalize(entry.root.join(relative_path))?;
        if !target.starts_with(&root) || !target.is_file() {
            return Err(ToolError::Failed(format!(
                "skill resource path escapes the skill: {relative}"
            )));
        }
        let metadata = std::fs::metadata(&target)?;
        if metadata.len() > MAX_SKILL_FILE_BYTES {
            return Err(ToolError::Failed(format!(
                "skill resource exceeds 1 MiB: {relative}"
            )));
        }
        let content = std::fs::read_to_string(&target)?;
        let mut output = format!(
            "Skill: {}\nSource: {}\nRoot: {}\nResource: {}\n",
            entry.name,
            entry.source,
            root.display(),
            relative
        );
        if relative.eq_ignore_ascii_case("SKILL.md") {
            let resources = list_resources(&root);
            if !resources.is_empty() {
                output.push_str("Available resources:\n");
                for resource in resources {
                    output.push_str("- ");
                    output.push_str(&resource);
                    output.push('\n');
                }
            }
        }
        output.push_str("\n---\n");
        output.push_str(&content);
        Ok(output)
    }
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn parse_frontmatter(content: &str, fallback_name: &str) -> (String, String) {
    let normalized = content.replace("\r\n", "\n");
    let mut lines = normalized.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (
            fallback_name.to_string(),
            "No description provided".to_string(),
        );
    }

    let mut name = None;
    let mut description = None;
    let mut collect_description = false;
    let mut description_lines = Vec::new();
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if collect_description {
            if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
                if !line.trim().is_empty() {
                    description_lines.push(line.trim().to_string());
                }
                continue;
            }
            collect_description = false;
        }
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(unquote(value));
        } else if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim();
            if matches!(value, "|" | "|-" | ">" | ">-") {
                collect_description = true;
            } else {
                description = Some(unquote(value));
            }
        }
    }
    if description.is_none() && !description_lines.is_empty() {
        description = Some(description_lines.join(" "));
    }
    (
        name.filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback_name.to_string()),
        description
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "No description provided".to_string()),
    )
}

fn list_resources(root: &Path) -> Vec<String> {
    fn visit(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
        if depth > 3 || out.len() >= 50 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, depth + 1, out);
            } else if path.file_name().is_some_and(|name| name != "SKILL.md")
                && let Ok(relative) = path.strip_prefix(root)
            {
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
            if out.len() >= 50 {
                break;
            }
        }
    }

    let mut resources = Vec::new();
    visit(root, root, 0, &mut resources);
    resources.sort();
    resources
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-skills-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_folded_frontmatter_and_reads_resources() {
        let root = temp_dir("discover");
        let skill = root.join("review");
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: review\ndescription: >-\n  Review code carefully.\n  Report concrete findings.\n---\n# Review\nRead references/checklist.md\n",
        )
        .unwrap();
        std::fs::write(skill.join("references/checklist.md"), "check it").unwrap();

        let catalog = SkillCatalog::discover_roots([(root, "test")]);
        assert_eq!(catalog.len(), 1);
        let summary = catalog.prompt_summary().unwrap();
        assert!(summary.contains("review: Review code carefully. Report concrete findings."));
        let instructions = catalog.read("$review", None).unwrap();
        assert!(instructions.contains("references/checklist.md"));
        assert_eq!(
            catalog
                .read("review", Some("references/checklist.md"))
                .unwrap(),
            format!(
                "Skill: review\nSource: test\nRoot: {}\nResource: references/checklist.md\n\n---\ncheck it",
                std::fs::canonicalize(skill).unwrap().display()
            )
        );
    }

    #[test]
    fn earlier_root_wins_and_traversal_is_rejected() {
        let project = temp_dir("project");
        let user = temp_dir("user");
        for (root, body) in [(&project, "project body"), (&user, "user body")] {
            let skill = root.join("same");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: same\ndescription: same\n---\n{body}"),
            )
            .unwrap();
        }

        let catalog = SkillCatalog::discover_roots([(project, "project"), (user, "user")]);
        assert!(catalog.read("same", None).unwrap().contains("project body"));
        let error = catalog.read("same", Some("../outside.md")).unwrap_err();
        assert!(error.to_string().contains("must stay inside"));
    }

    #[test]
    fn prompt_summary_is_sorted_and_repeatable() {
        let root = temp_dir("sorted");
        // Created in reverse alphabetical order on purpose.
        for name in ["zeta", "alpha"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\nbody\n"),
            )
            .unwrap();
        }

        let first = SkillCatalog::discover_roots([(root.clone(), "test")])
            .prompt_summary()
            .unwrap();
        let second = SkillCatalog::discover_roots([(root, "test")])
            .prompt_summary()
            .unwrap();

        // The summary is part of the cached system prefix: it must be
        // byte-identical for an unchanged skill set.
        assert_eq!(first, second);
        assert!(first.find("- alpha:").unwrap() < first.find("- zeta:").unwrap());
    }
}
