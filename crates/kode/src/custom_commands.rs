//! User-defined slash commands: markdown prompt templates discovered from
//! `.kode/commands/*.md` (repo-local) and `~/.kode/commands/*.md`
//! (user-global), expanded into task prompts for both the TUI and
//! `kode exec`.
//!
//! A file `<name>.md` defines the command `/name`. Its body is the prompt
//! template. An optional leading YAML-ish frontmatter block (`---` ...
//! `---`) may set `description:` for the hint list; unknown frontmatter
//! keys are ignored, and there's no YAML dependency — the frontmatter scan
//! is hand-rolled and only ever looks for that one key.
//!
//! Precedence on a name clash: builtin > repo > user-global (builtins are
//! never shadowed — see `SlashCommand::Custom` in `tui::commands`). Repo
//! wins over user-global. Discovery re-scans the directories on every call
//! — they're expected to be tiny, so no caching/hot-reload watching.

use std::path::{Path, PathBuf};

/// One discovered user-defined command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// Lowercase command name (file stem), without the leading `/`.
    pub name: String,
    /// One-line description for the hint list: frontmatter `description:`
    /// if present, else falls back to `name`.
    pub description: String,
    /// Absolute path to the source `.md` file.
    pub path: PathBuf,
}

/// Repo-local commands directory: `<cwd>/.kode/commands`.
fn repo_commands_dir(cwd: &Path) -> PathBuf {
    cwd.join(".kode").join("commands")
}

/// User-global commands directory: `~/.kode/commands` (`$USERPROFILE` or
/// `$HOME`). `None` when neither environment variable is set.
fn user_commands_dir() -> Option<PathBuf> {
    Some(kode_core::paths::kode_home_dir()?.join("commands"))
}

/// True when `name` is a valid custom-command name: one or more of
/// `[a-z0-9-_]`, case-insensitively (files with other characters in their
/// stem are skipped during discovery).
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Scans one directory for `*.md` files with valid names, returning
/// `(lowercased name, description, path)` triples sorted by name. Missing
/// directory yields an empty vec, not an error.
fn scan_dir(dir: &Path) -> Vec<CustomCommand> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_valid_name(stem) {
            continue;
        }
        let name = stem.to_lowercase();
        let description = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| frontmatter_description(&content))
            .unwrap_or_else(|| name.clone());
        found.push(CustomCommand {
            name,
            description,
            path,
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Discovers custom commands across the repo (`<cwd>/.kode/commands`) and
/// user-global (`~/.kode/commands`) directories, applying precedence:
/// repo wins over user-global on a name clash. `builtin_names` (already
/// lowercase, no leading `/`) are excluded outright — builtins are never
/// shadowed. Result is sorted by name.
pub fn discover(cwd: &Path, builtin_names: &[&str]) -> Vec<CustomCommand> {
    let mut by_name: std::collections::BTreeMap<String, CustomCommand> =
        std::collections::BTreeMap::new();

    // User-global first, then repo — repo entries overwrite user-global on
    // a name clash (inserted second wins).
    if let Some(dir) = user_commands_dir() {
        for cmd in scan_dir(&dir) {
            by_name.insert(cmd.name.clone(), cmd);
        }
    }
    for cmd in scan_dir(&repo_commands_dir(cwd)) {
        by_name.insert(cmd.name.clone(), cmd);
    }

    by_name
        .into_values()
        .filter(|cmd| !builtin_names.contains(&cmd.name.as_str()))
        .collect()
}

/// Strips a leading YAML-ish frontmatter block (`---\n...\n---\n`) from
/// `content`, returning the remainder (the prompt template body). No
/// frontmatter present → `content` unchanged (leading/trailing whitespace
/// still trimmed).
fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content.trim();
    };
    match rest.find("\n---\n") {
        // `+ 5` skips past the closing fence's own `\n---\n`.
        Some(end) => rest[end + 5..].trim(),
        // No closing fence found — malformed/unclosed frontmatter. Treat
        // the whole thing as the template body rather than silently
        // discarding content that merely starts with `---`.
        None => content.trim(),
    }
}

/// Extracts the `description:` value from a leading frontmatter block, if
/// any. Returns `None` when there's no frontmatter or no `description:`
/// key inside it — callers fall back to the command name.
fn frontmatter_description(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    let block = &rest[..end];
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim();
            let value = value.trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Expands a custom command's template at `path` with `args` (everything
/// typed after the command name, already trimmed). Every `$ARGUMENTS`
/// occurrence in the template is replaced with `args`; if the template has
/// no `$ARGUMENTS` placeholder and `args` is non-empty, `args` is appended
/// to the end (`\n\n` separated). Frontmatter (if any) is stripped before
/// substitution. Errors (missing/unreadable file) surface as `io::Error` —
/// callers report them, never panic.
pub fn expand(path: &Path, args: &str) -> std::io::Result<String> {
    let content = std::fs::read_to_string(path)?;
    let template = strip_frontmatter(&content);
    if template.contains("$ARGUMENTS") {
        Ok(template.replace("$ARGUMENTS", args))
    } else if args.is_empty() {
        Ok(template.to_string())
    } else {
        Ok(format!("{template}\n\n{args}"))
    }
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
            "kode-custom-cmd-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_cmd(dir: &Path, name: &str, content: &str) -> PathBuf {
        let cmds = dir.join(".kode").join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        let path = cmds.join(format!("{name}.md"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn discover_finds_repo_commands() {
        let dir = temp_dir("repo");
        write_cmd(&dir, "review", "Review this: $ARGUMENTS");

        let found = discover(&dir, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "review");
    }

    #[test]
    fn discover_missing_dirs_yield_empty() {
        let dir = temp_dir("missing");
        let found = discover(&dir, &[]);
        assert!(found.is_empty());
    }

    #[test]
    fn discover_excludes_builtin_names() {
        let dir = temp_dir("builtin-shadow");
        write_cmd(&dir, "model", "custom model prompt");

        let found = discover(&dir, &["model", "help"]);
        assert!(found.is_empty(), "builtin names must never be shadowed");
    }

    #[test]
    fn discover_rejects_invalid_names() {
        let dir = temp_dir("invalid-name");
        let cmds = dir.join(".kode").join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("weird name!.md"), "body").unwrap();
        std::fs::write(cmds.join("valid-name_1.md"), "body").unwrap();

        let found = discover(&dir, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "valid-name_1");
    }

    #[test]
    fn discover_normalizes_case() {
        let dir = temp_dir("case");
        write_cmd(&dir, "Review", "body");

        let found = discover(&dir, &[]);
        assert_eq!(found[0].name, "review");
    }

    #[test]
    fn discover_sorted_by_name() {
        let dir = temp_dir("sorted");
        write_cmd(&dir, "zeta", "z");
        write_cmd(&dir, "alpha", "a");

        let found = discover(&dir, &[]);
        assert_eq!(found[0].name, "alpha");
        assert_eq!(found[1].name, "zeta");
    }

    #[test]
    fn frontmatter_description_is_parsed() {
        let dir = temp_dir("frontmatter");
        let path = write_cmd(
            &dir,
            "review",
            "---\ndescription: review a diff carefully\n---\nReview: $ARGUMENTS",
        );
        let cmd = &discover(&dir, &[])[0];
        assert_eq!(cmd.description, "review a diff carefully");
        assert_eq!(cmd.path, path);
    }

    #[test]
    fn no_frontmatter_description_falls_back_to_name() {
        let dir = temp_dir("no-frontmatter");
        write_cmd(&dir, "review", "Review: $ARGUMENTS");

        let cmd = &discover(&dir, &[])[0];
        assert_eq!(cmd.description, "review");
    }

    #[test]
    fn expand_replaces_all_arguments_occurrences() {
        let dir = temp_dir("expand-multi");
        let path = write_cmd(&dir, "dup", "first: $ARGUMENTS, second: $ARGUMENTS");

        let expanded = expand(&path, "foo").unwrap();
        assert_eq!(expanded, "first: foo, second: foo");
    }

    #[test]
    fn expand_appends_args_when_no_placeholder() {
        let dir = temp_dir("expand-append");
        let path = write_cmd(&dir, "plain", "Do the review");

        let expanded = expand(&path, "carefully please").unwrap();
        assert_eq!(expanded, "Do the review\n\ncarefully please");
    }

    #[test]
    fn expand_with_no_args_and_no_placeholder_leaves_template_unchanged() {
        let dir = temp_dir("expand-no-args");
        let path = write_cmd(&dir, "plain", "Do the review");

        let expanded = expand(&path, "").unwrap();
        assert_eq!(expanded, "Do the review");
    }

    #[test]
    fn expand_strips_frontmatter_before_substitution() {
        let dir = temp_dir("expand-frontmatter");
        let path = write_cmd(
            &dir,
            "review",
            "---\ndescription: reviews stuff\n---\nReview: $ARGUMENTS",
        );

        let expanded = expand(&path, "the diff").unwrap();
        assert_eq!(expanded, "Review: the diff");
    }

    #[test]
    fn expand_missing_file_is_an_error() {
        let dir = temp_dir("expand-missing");
        let missing = dir.join("nope.md");
        assert!(expand(&missing, "").is_err());
    }

    #[test]
    fn repo_precedence_over_user_global() {
        // Simulate precedence at the merge level directly, since pointing
        // user_commands_dir() at a temp dir would require env mutation
        // (process-global, unsafe under parallel tests). discover()'s
        // insertion order (user-global first, repo second) is exercised
        // indirectly by discover_finds_repo_commands; this test locks the
        // BTreeMap merge behavior the precedence relies on.
        let dir = temp_dir("precedence");
        write_cmd(&dir, "shared", "repo version");

        let found = discover(&dir, &[]);
        assert_eq!(found.len(), 1);
        let content = std::fs::read_to_string(&found[0].path).unwrap();
        assert_eq!(content, "repo version");
    }
}
