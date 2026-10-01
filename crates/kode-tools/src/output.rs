//! Shaping of tool results before they reach the model.

/// Upper bound for one tool result. `PromptBudget` retains at most 2,048
/// tokens (8,192 bytes) of any tool result, so anything beyond this would be
/// cut later anyway, blindly.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 8_000;

/// Room kept for the omission marker so the result stays within the limit.
const MARKER_RESERVE: usize = 120;

/// Returns `content` unchanged when it fits in `max_bytes`. Otherwise keeps
/// the beginning and the end (errors and summaries usually sit at the end)
/// and states how much was removed. Cuts prefer line boundaries and never
/// split a character. Deterministic. A `max_bytes` below the marker size
/// yields only the marker.
pub fn clip(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }
    let keep = max_bytes.saturating_sub(MARKER_RESERVE);
    let head_budget = keep * 3 / 5;
    let tail_budget = keep - head_budget;

    let head_end = head_boundary(content, head_budget);
    let tail_start = tail_boundary(content, content.len() - tail_budget).max(head_end);
    let omitted = &content[head_end..tail_start];

    format!(
        "{}\n[... {} lines ({} bytes) omitted; narrow the request to see them ...]\n{}",
        &content[..head_end],
        omitted.lines().count(),
        omitted.len(),
        &content[tail_start..]
    )
}

/// Largest cut point at or below `budget`, moved back to the end of a whole
/// line when that gives up less than half of the budget.
fn head_boundary(content: &str, budget: usize) -> usize {
    let mut end = budget.min(content.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    match content[..end].rfind('\n') {
        Some(newline) if newline.saturating_add(1) >= end / 2 => newline + 1,
        _ => end,
    }
}

/// Smallest cut point at or above `from`, moved forward to the start of a
/// whole line when that gives up less than half of the tail.
fn tail_boundary(content: &str, from: usize) -> usize {
    let mut start = from.min(content.len());
    while !content.is_char_boundary(start) {
        start += 1;
    }
    let tail = &content[start..];
    match tail.find('\n') {
        Some(newline) if newline < tail.len() / 2 => start + newline + 1,
        _ => start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_is_returned_unchanged() {
        assert_eq!(
            clip("hello\nworld\n", MAX_TOOL_OUTPUT_BYTES),
            "hello\nworld\n"
        );
        assert_eq!(clip("", MAX_TOOL_OUTPUT_BYTES), "");
    }

    #[test]
    fn long_content_keeps_head_and_tail_within_the_limit() {
        let content: String = (1..=5_000).map(|n| format!("line {n}\n")).collect();
        let clipped = clip(&content, MAX_TOOL_OUTPUT_BYTES);

        assert!(clipped.len() <= MAX_TOOL_OUTPUT_BYTES, "{}", clipped.len());
        assert!(clipped.starts_with("line 1\nline 2\n"));
        assert!(clipped.ends_with("line 5000\n"));
        assert!(clipped.contains("omitted; narrow the request to see them"));
    }

    #[test]
    fn marker_reports_exactly_what_was_removed() {
        let content: String = (1..=5_000).map(|n| format!("line {n}\n")).collect();
        let clipped = clip(&content, MAX_TOOL_OUTPUT_BYTES);

        let marker_start = clipped.find("\n[... ").unwrap();
        let marker_end = clipped[marker_start..].find("...]\n").unwrap() + marker_start + 5;
        let head = &clipped[..marker_start];
        let tail = &clipped[marker_end..];
        let marker = &clipped[marker_start..marker_end];

        let omitted_bytes = content.len() - head.len() - tail.len();
        assert!(
            marker.contains(&format!("({omitted_bytes} bytes)")),
            "{marker}"
        );
        // Cuts land on line boundaries when the text has lines.
        assert!(head.ends_with('\n'));
        assert!(tail.starts_with("line "));
        assert_eq!(
            format!(
                "{head}{}{tail}",
                &content[head.len()..content.len() - tail.len()]
            ),
            content
        );
    }

    #[test]
    fn content_without_newlines_is_cut_on_char_boundaries() {
        let content = "é".repeat(20_000); // 2 bytes each
        let clipped = clip(&content, MAX_TOOL_OUTPUT_BYTES);
        assert!(clipped.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(clipped.starts_with('é'));
        assert!(clipped.ends_with('é'));
    }

    #[test]
    fn clipping_is_deterministic() {
        let content = "x\n".repeat(30_000);
        assert_eq!(
            clip(&content, MAX_TOOL_OUTPUT_BYTES),
            clip(&content, MAX_TOOL_OUTPUT_BYTES)
        );
    }
}
