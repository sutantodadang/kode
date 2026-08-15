//! Minimal, total (never-panic) markdown rendering for transcript prose
//! lines. Per `DESIGN.md`'s anti-slop rule, styling stays within the
//! existing palette + bold/dim/italic vocabulary — no new colors are
//! introduced here; color decisions live in `tui.rs`'s draw code, which
//! maps [`MdKind`]/[`MdStyle`] onto the theme.
//!
//! `render_line` is called once per line of a flushed prose chunk, with
//! `in_code_block` threaded through by the caller so a ``` fence toggle on
//! one line affects how the next lines of the *same message* are parsed.
//! Callers should use a fresh `bool` (starting `false`) per agent message
//! so state never leaks across messages.

/// Inline emphasis for one span of rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdStyle {
    Plain,
    Bold,
    Italic,
    InlineCode,
}

/// What kind of markdown construct a whole line parsed as. Drives
/// line-level draw decisions (heading spacer, bullet/fence/code coloring)
/// that don't fit the per-span `MdStyle` model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdKind {
    Heading,
    Bullet,
    Code,
    CodeFence,
    Plain,
}

/// One rendered transcript line: its spans (text + emphasis) and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedLine {
    pub spans: Vec<(String, MdStyle)>,
    pub kind: MdKind,
}

/// Renders one line of markdown prose. `in_code_block` is toggled on a
/// ``` fence line and read to decide whether the current line is inside a
/// fenced block (rendered verbatim, no inline parsing). Never panics —
/// unterminated inline markers fall back to literal text.
pub fn render_line(line: &str, in_code_block: &mut bool) -> RenderedLine {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        *in_code_block = !*in_code_block;
        return RenderedLine {
            spans: vec![("\u{2504}\u{2504}".to_string(), MdStyle::Plain)],
            kind: MdKind::CodeFence,
        };
    }

    if *in_code_block {
        return RenderedLine {
            spans: vec![(line.to_string(), MdStyle::Plain)],
            kind: MdKind::Code,
        };
    }

    if let Some(text) = heading_text(trimmed) {
        return RenderedLine {
            spans: vec![(text, MdStyle::Bold)],
            kind: MdKind::Heading,
        };
    }

    if let Some((depth, content)) = bullet_parts(line) {
        let marker = format!("{}\u{2022} ", "  ".repeat(depth));
        let mut spans = vec![(marker, MdStyle::Plain)];
        spans.extend(parse_inline(content));
        return RenderedLine {
            spans,
            kind: MdKind::Bullet,
        };
    }

    RenderedLine {
        spans: parse_inline(line),
        kind: MdKind::Plain,
    }
}

/// Parses a `#`..`####` heading prefix (must be followed by a space or end
/// of line, so `#hashtag` isn't mistaken for a heading). Returns the
/// stripped, trimmed heading text.
fn heading_text(trimmed: &str) -> Option<String> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 4 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    Some(rest.trim().to_string())
}

/// Parses a `- `/`* `/`N. ` bullet prefix, preserving indent depth (2
/// spaces per level). Returns `(depth, content-after-marker)`.
fn bullet_parts(line: &str) -> Option<(usize, &str)> {
    let indent = line.chars().take_while(|&c| c == ' ').count();
    let rest = &line[indent..];
    let depth = indent / 2;

    if let Some(content) = rest.strip_prefix("- ") {
        return Some((depth, content));
    }
    if let Some(content) = rest.strip_prefix("* ") {
        return Some((depth, content));
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0
        && let Some(content) = rest[digits..].strip_prefix(". ")
    {
        return Some((depth, content));
    }
    None
}

/// Inline emphasis parser: `**bold**`, `*italic*`/`_italic_`, `` `code` ``.
/// Unterminated markers (no matching close on the line) render literally —
/// this never panics and never drops text.
fn parse_inline(text: &str) -> Vec<(String, MdStyle)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < n {
        if chars[i] == '*' && i + 1 < n && chars[i + 1] == '*' {
            if let Some(end) = find_double(&chars, i + 2, '*') {
                flush(&mut spans, &mut buf);
                spans.push((chars[i + 2..end].iter().collect(), MdStyle::Bold));
                i = end + 2;
                continue;
            }
        } else if chars[i] == '`' {
            if let Some(end) = find_single(&chars, i + 1, '`') {
                flush(&mut spans, &mut buf);
                spans.push((chars[i + 1..end].iter().collect(), MdStyle::InlineCode));
                i = end + 1;
                continue;
            }
        } else if chars[i] == '*' || chars[i] == '_' {
            let delim = chars[i];
            if let Some(end) = find_single(&chars, i + 1, delim) {
                flush(&mut spans, &mut buf);
                spans.push((chars[i + 1..end].iter().collect(), MdStyle::Italic));
                i = end + 1;
                continue;
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut spans, &mut buf);
    spans
}

/// Index of the next `c c` (doubled marker, e.g. `**`) at or after `start`.
fn find_double(chars: &[char], start: usize, c: char) -> Option<usize> {
    if start >= chars.len() {
        return None;
    }
    (start..chars.len() - 1).find(|&i| chars[i] == c && chars[i + 1] == c)
}

/// Index of the next single occurrence of `c` at or after `start`.
fn find_single(chars: &[char], start: usize, c: char) -> Option<usize> {
    chars
        .get(start..)?
        .iter()
        .position(|&ch| ch == c)
        .map(|p| p + start)
}

fn flush(spans: &mut Vec<(String, MdStyle)>, buf: &mut String) {
    if !buf.is_empty() {
        spans.push((std::mem::take(buf), MdStyle::Plain));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_strips_hashes_and_bolds_whole_line() {
        let mut code = false;
        let r = render_line("## Section title", &mut code);
        assert_eq!(r.kind, MdKind::Heading);
        assert_eq!(r.spans, vec![("Section title".to_string(), MdStyle::Bold)]);
    }

    #[test]
    fn hash_without_space_is_not_a_heading() {
        let mut code = false;
        let r = render_line("#hashtag", &mut code);
        assert_eq!(r.kind, MdKind::Plain);
    }

    #[test]
    fn bullet_nesting_tracks_indent_depth() {
        let mut code = false;
        let top = render_line("- top item", &mut code);
        assert_eq!(top.kind, MdKind::Bullet);
        assert_eq!(top.spans[0].0, "\u{2022} ");

        let nested = render_line("  - nested item", &mut code);
        assert_eq!(nested.kind, MdKind::Bullet);
        assert_eq!(nested.spans[0].0, "  \u{2022} ");

        let numbered = render_line("1. first", &mut code);
        assert_eq!(numbered.kind, MdKind::Bullet);
        assert_eq!(numbered.spans[0].0, "\u{2022} ");
    }

    #[test]
    fn inline_bold_and_code_split_into_styled_spans() {
        let mut code = false;
        let r = render_line("do **this** then `run it`", &mut code);
        assert_eq!(r.kind, MdKind::Plain);
        assert_eq!(
            r.spans,
            vec![
                ("do ".to_string(), MdStyle::Plain),
                ("this".to_string(), MdStyle::Bold),
                (" then ".to_string(), MdStyle::Plain),
                ("run it".to_string(), MdStyle::InlineCode),
            ]
        );
    }

    #[test]
    fn inline_italic_supports_star_and_underscore() {
        let mut code = false;
        let r = render_line("*a* and _b_", &mut code);
        assert_eq!(
            r.spans,
            vec![
                ("a".to_string(), MdStyle::Italic),
                (" and ".to_string(), MdStyle::Plain),
                ("b".to_string(), MdStyle::Italic),
            ]
        );
    }

    #[test]
    fn fence_toggles_and_interior_is_verbatim() {
        let mut code = false;
        let open = render_line("```rust", &mut code);
        assert_eq!(open.kind, MdKind::CodeFence);
        assert!(code);

        let interior = render_line("  let x = **not bold**;", &mut code);
        assert_eq!(interior.kind, MdKind::Code);
        assert_eq!(
            interior.spans,
            vec![("  let x = **not bold**;".to_string(), MdStyle::Plain)]
        );

        let close = render_line("```", &mut code);
        assert_eq!(close.kind, MdKind::CodeFence);
        assert!(!code);
    }

    #[test]
    fn unterminated_bold_marker_renders_literally() {
        let mut code = false;
        let r = render_line("this **never closes", &mut code);
        assert_eq!(r.kind, MdKind::Plain);
        assert_eq!(
            r.spans,
            vec![("this **never closes".to_string(), MdStyle::Plain)]
        );
    }

    #[test]
    fn per_message_code_block_state_resets_between_messages() {
        let mut msg1 = false;
        render_line("```", &mut msg1);
        assert!(msg1); // still inside the fence at end of message 1

        // A fresh message starts its own state — no leakage from msg1.
        let mut msg2 = false;
        let r = render_line("plain text", &mut msg2);
        assert_eq!(r.kind, MdKind::Plain);
        assert!(!msg2);
    }

    #[test]
    fn never_panics_on_empty_line() {
        let mut code = false;
        let r = render_line("", &mut code);
        assert_eq!(r.kind, MdKind::Plain);
        assert_eq!(r.spans, Vec::new());
    }
}
