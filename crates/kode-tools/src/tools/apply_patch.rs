use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::path::resolve_in_workspace;
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

pub struct ApplyPatch;

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Replace an exact substring in a file (old_string -> new_string)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::Mutating
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: Args = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: e.to_string(),
        })?;

        if args.old_string.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "old_string must not be empty".to_string(),
            });
        }

        let resolved = resolve_in_workspace(&ctx.workspace_root, &args.path)?;
        let original = tokio::fs::read_to_string(&resolved).await?;

        let replace_all = args.replace_all.unwrap_or(false);
        let (updated, replaced) =
            apply_replacement(&original, &args.old_string, &args.new_string, replace_all)?;

        tokio::fs::write(&resolved, &updated).await?;

        tracing::debug!(path = %resolved.display(), replaced, "apply_patch executed");
        Ok(ToolOutput {
            content: format!(
                "applied {replaced} replacement(s) to {}",
                resolved.display()
            ),
        })
    }
}

/// Replaces `old` with `new` in `original`. Matching is exact first; when
/// that fails, both sides are compared with `\r\n` normalized to `\n` so a
/// model-emitted LF snippet still matches a CRLF file (and vice versa). The
/// file's original line ending is preserved on write.
pub(crate) fn apply_replacement(
    original: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, usize)> {
    let crlf = original.contains("\r\n");
    let normalize = |s: &str| s.replace("\r\n", "\n");

    let (haystack, needle, replacement) = if original.matches(old).count() > 0 {
        (original.to_string(), old.to_string(), new.to_string())
    } else {
        (normalize(original), normalize(old), normalize(new))
    };

    let match_count = haystack.matches(needle.as_str()).count();
    let updated = if replace_all {
        if match_count == 0 {
            return Err(not_found(original, old));
        }
        haystack.replace(&needle, &replacement)
    } else {
        match match_count {
            0 => return Err(not_found(original, old)),
            1 => haystack.replacen(&needle, &replacement, 1),
            n => {
                return Err(ToolError::Failed(format!(
                    "old_string is not unique; {n} matches — include more surrounding lines or set replace_all"
                )));
            }
        }
    };
    let replaced = if replace_all { match_count } else { 1 };

    // Re-apply CRLF only if we normalized (the exact-match path never
    // touched line endings).
    let updated = if crlf && !updated.contains("\r\n") {
        updated.replace('\n', "\r\n")
    } else {
        updated
    };
    Ok((updated, replaced))
}

/// Builds the not-found error with a hint naming the first old_string line
/// that does not occur in the file (after line-ending normalization), so
/// the model can see *where* its snippet drifted.
fn not_found(original: &str, old: &str) -> ToolError {
    let file = original.replace("\r\n", "\n");
    let missing = old
        .replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .find(|l| !file.contains(l))
        .map(|l| l.trim().chars().take(80).collect::<String>());
    match missing {
        Some(line) => ToolError::Failed(format!(
            "old_string not found — first line with no match in file: `{line}`. Re-read the file and copy the text exactly (whitespace matters)"
        )),
        None => ToolError::Failed(
            "old_string not found — every line exists in the file but not contiguously / with this exact indentation. Re-read the file and copy the block exactly"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn temp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-tools-patch-{}-{}-{}",
            std::process::id(),
            nanos(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            workspace_root: root,
            cancel: kode_core::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn unique_replace_works() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo bar baz").unwrap();
        let tool = ApplyPatch;
        let out = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "bar", "new_string": "qux"}),
                &ctx(dir.clone()),
            )
            .await
            .unwrap();
        assert!(out.content.contains("applied 1"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "foo qux baz"
        );
    }

    #[tokio::test]
    async fn not_found_fails() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo bar baz").unwrap();
        let tool = ApplyPatch;
        let err = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "nope", "new_string": "x"}),
                &ctx(dir),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
    }

    #[tokio::test]
    async fn duplicate_without_replace_all_fails() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo foo").unwrap();
        let tool = ApplyPatch;
        let err = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "bar"}),
                &ctx(dir),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
    }

    #[tokio::test]
    async fn replace_all_replaces_both() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo foo").unwrap();
        let tool = ApplyPatch;
        let out = tool
            .execute(
                serde_json::json!({"path": "a.txt", "old_string": "foo", "new_string": "bar", "replace_all": true}),
                &ctx(dir.clone()),
            )
            .await
            .unwrap();
        assert!(out.content.contains("applied 2"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "bar bar"
        );
    }

    #[test]
    fn apply_replacement_matches_lf_needle_against_crlf_file() {
        let file = "fn a() {\r\n    1\r\n}\r\n";
        let (out, n) =
            apply_replacement(file, "fn a() {\n    1\n}", "fn a() {\n    2\n}", false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, "fn a() {\r\n    2\r\n}\r\n");
    }

    #[test]
    fn apply_replacement_matches_crlf_needle_against_lf_file() {
        let file = "x\ny\n";
        let (out, _) = apply_replacement(file, "x\r\ny", "z", false).unwrap();
        assert_eq!(out, "z\n");
    }

    #[test]
    fn apply_replacement_exact_path_keeps_mixed_endings() {
        let file = "a\r\nb\nc";
        let (out, _) = apply_replacement(file, "b", "B", false).unwrap();
        assert_eq!(out, "a\r\nB\nc");
    }

    #[test]
    fn apply_replacement_not_found_names_first_missing_line() {
        let err = apply_replacement(
            "let x = 1;\nlet y = 2;\n",
            "let x = 1;\nlet q = 9;",
            "",
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("old_string not found"), "{msg}");
        assert!(msg.contains("let q = 9;"), "{msg}");
    }

    #[test]
    fn apply_replacement_non_unique_errors_unless_replace_all() {
        assert!(apply_replacement("a a", "a", "b", false).is_err());
        let (out, n) = apply_replacement("a a", "a", "b", true).unwrap();
        assert_eq!((out.as_str(), n), ("b b", 2));
    }
}
