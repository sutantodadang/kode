use serde::Deserialize;

use crate::error::{Result, ToolError};
use crate::path::resolve_in_workspace;
use crate::{RequiredPermission, Tool, ToolContext, ToolOutput};

const MAX_UNBOUNDED_BYTES: u64 = 1024 * 1024;

/// File text returned per call. Leaves room for the footer under
/// `output::MAX_TOOL_OUTPUT_BYTES`, so the runtime clip never cuts a window.
const WINDOW_BYTES: usize = 7_600;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the workspace, optionally by line range. Long files come back one window at a time; the footer gives the offset of the next window."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer", "description": "1-based starting line" },
                "limit": { "type": "integer", "description": "number of lines to read" }
            },
            "required": ["path"]
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        RequiredPermission::ReadOnly
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: Args = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: e.to_string(),
        })?;

        let resolved = resolve_in_workspace(&ctx.workspace_root, &args.path)?;

        if args.offset.is_none() && args.limit.is_none() {
            let meta = tokio::fs::metadata(&resolved).await?;
            if meta.len() > MAX_UNBOUNDED_BYTES {
                return Err(ToolError::Failed(
                    "file too large, use offset/limit".to_string(),
                ));
            }
        }

        let content = tokio::fs::read_to_string(&resolved).await?;

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(usize::MAX);
        let total_lines = content.lines().count();

        let mut shown = String::new();
        // 1-based number of the last line placed in `shown`.
        let mut last_line = offset - 1;
        // Set when a single line is longer than the whole window.
        let mut cut_line: Option<(usize, usize)> = None;
        for (index, line) in content.lines().enumerate().skip(offset - 1).take(limit) {
            let separator = usize::from(!shown.is_empty());
            if shown.len() + separator + line.len() > WINDOW_BYTES {
                if shown.is_empty() {
                    let mut end = WINDOW_BYTES.min(line.len());
                    while !line.is_char_boundary(end) {
                        end -= 1;
                    }
                    shown.push_str(&line[..end]);
                    last_line = index + 1;
                    cut_line = Some((line.len(), end));
                }
                break;
            }
            if separator == 1 {
                shown.push('\n');
            }
            shown.push_str(line);
            last_line = index + 1;
        }

        let requested_end = (offset - 1).saturating_add(limit).min(total_lines);
        let whole_file = args.offset.is_none() && args.limit.is_none();
        let content = if let Some((line_bytes, shown_bytes)) = cut_line {
            format!(
                "{shown}\n[line {last_line} of {total_lines} is {line_bytes} bytes; only its first {shown_bytes} bytes are shown. Continue with offset={}]",
                last_line + 1
            )
        } else if last_line < requested_end {
            format!(
                "{shown}\n[lines {offset}-{last_line} of {total_lines} shown; call read_file with offset={} to continue]",
                last_line + 1
            )
        } else if whole_file {
            // Fits in one window: byte-exact, trailing newline included.
            content
        } else {
            shown
        };

        tracing::debug!(path = %resolved.display(), "read_file executed");
        Ok(ToolOutput { content })
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
            "kode-tools-read-{}-{}-{}",
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
    async fn roundtrip() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "hello world").unwrap();
        let tool = ReadFile;
        let out = tool
            .execute(serde_json::json!({"path": "a.txt"}), &ctx(dir))
            .await
            .unwrap();
        assert_eq!(out.content, "hello world");
    }

    #[tokio::test]
    async fn offset_limit_slice() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "l1\nl2\nl3\nl4\n").unwrap();
        let tool = ReadFile;
        let out = tool
            .execute(
                serde_json::json!({"path": "a.txt", "offset": 2, "limit": 2}),
                &ctx(dir),
            )
            .await
            .unwrap();
        assert_eq!(out.content, "l2\nl3");
    }

    #[tokio::test]
    async fn traversal_rejected() {
        let dir = temp_dir();
        let tool = ReadFile;
        let err = tool
            .execute(serde_json::json!({"path": "../../etc/passwd"}), &ctx(dir))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }

    fn next_offset(output: &str) -> Option<usize> {
        let footer = output.rsplit_once("\n[")?.1;
        let after = footer.split_once("offset=")?.1;
        after
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    }

    fn body(output: &str) -> &str {
        match output.rsplit_once("\n[") {
            Some((body, footer)) if footer.contains("offset=") => body,
            _ => output,
        }
    }

    #[tokio::test]
    async fn long_file_pages_through_without_losing_a_line() {
        let dir = temp_dir();
        let lines: Vec<String> = (1..=1_000)
            .map(|n| format!("line {n:04} {}", "x".repeat(30)))
            .collect();
        std::fs::write(dir.join("big.txt"), lines.join("\n")).unwrap();
        let tool = ReadFile;

        let mut collected: Vec<String> = Vec::new();
        let mut offset: Option<usize> = None;
        let mut calls = 0;
        loop {
            calls += 1;
            assert!(calls < 50, "paging did not terminate");
            let args = match offset {
                Some(offset) => serde_json::json!({"path": "big.txt", "offset": offset}),
                None => serde_json::json!({"path": "big.txt"}),
            };
            let out = tool.execute(args, &ctx(dir.clone())).await.unwrap();
            assert!(
                out.content.len() <= crate::output::MAX_TOOL_OUTPUT_BYTES,
                "{}",
                out.content.len()
            );
            collected.extend(body(&out.content).lines().map(str::to_string));
            match next_offset(&out.content) {
                Some(next) => offset = Some(next),
                None => break,
            }
        }

        assert!(calls > 1, "a 41 KB file must need more than one window");
        assert_eq!(collected, lines);
    }

    #[tokio::test]
    async fn first_window_footer_names_the_range_and_total() {
        let dir = temp_dir();
        let lines: Vec<String> = (1..=1_000)
            .map(|n| format!("line {n:04} {}", "x".repeat(30)))
            .collect();
        std::fs::write(dir.join("big.txt"), lines.join("\n")).unwrap();

        let out = ReadFile
            .execute(serde_json::json!({"path": "big.txt"}), &ctx(dir))
            .await
            .unwrap();

        let shown = body(&out.content).lines().count();
        assert!(out.content.ends_with(&format!(
            "\n[lines 1-{shown} of 1000 shown; call read_file with offset={} to continue]",
            shown + 1
        )));
    }

    #[tokio::test]
    async fn one_giant_line_shows_its_start_and_says_so() {
        let dir = temp_dir();
        std::fs::write(
            dir.join("min.js"),
            format!("{}\nsecond", "a".repeat(20_000)),
        )
        .unwrap();

        let out = ReadFile
            .execute(serde_json::json!({"path": "min.js"}), &ctx(dir))
            .await
            .unwrap();

        assert!(out.content.len() <= crate::output::MAX_TOOL_OUTPUT_BYTES);
        assert!(out.content.starts_with("aaaa"));
        assert!(
            out.content
                .contains("[line 1 of 2 is 20000 bytes; only its first ")
        );
        assert!(out.content.ends_with("Continue with offset=2]"));
    }

    #[tokio::test]
    async fn multibyte_text_is_never_split_inside_a_character() {
        let dir = temp_dir();
        std::fs::write(dir.join("utf8.txt"), "é".repeat(10_000)).unwrap();

        let out = ReadFile
            .execute(serde_json::json!({"path": "utf8.txt"}), &ctx(dir))
            .await
            .unwrap();

        assert!(out.content.len() <= crate::output::MAX_TOOL_OUTPUT_BYTES);
        assert!(out.content.starts_with('é'));
    }

    #[tokio::test]
    async fn offset_past_end_is_empty_without_footer() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "l1\nl2\n").unwrap();

        let out = ReadFile
            .execute(
                serde_json::json!({"path": "a.txt", "offset": 99}),
                &ctx(dir),
            )
            .await
            .unwrap();

        assert_eq!(out.content, "");
    }

    #[tokio::test]
    async fn small_file_is_still_byte_exact() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "l1\nl2\n").unwrap();

        let out = ReadFile
            .execute(serde_json::json!({"path": "a.txt"}), &ctx(dir))
            .await
            .unwrap();

        assert_eq!(out.content, "l1\nl2\n");
    }
}
