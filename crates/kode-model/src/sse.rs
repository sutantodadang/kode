/// Split incoming SSE bytes into complete `data: ` payload lines, buffering
/// any trailing partial line across calls. Shared by all SSE-based model
/// backends (OpenAI-compatible chat/completions, Codex Responses API).
pub(crate) fn extract_data_lines(buffer: &mut String, incoming: &str) -> Vec<String> {
    buffer.push_str(incoming);
    let mut out = Vec::new();
    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(data) = line.strip_prefix("data: ") {
            out.push(data.to_string());
        } else if let Some(data) = line.strip_prefix("data:") {
            out.push(data.trim_start().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_data_lines_reassembles_partial_lines_across_calls() {
        let mut buffer = String::new();

        let first = extract_data_lines(&mut buffer, "data: {\"a\":1}\ndata: parti");
        assert_eq!(first, vec!["{\"a\":1}".to_string()]);
        assert_eq!(buffer, "data: parti");

        let second = extract_data_lines(&mut buffer, "al}\ndata: [DONE]\n");
        assert_eq!(second, vec!["partial}".to_string(), "[DONE]".to_string()]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn extract_data_lines_ignores_non_data_fields() {
        let mut buffer = String::new();
        let lines = extract_data_lines(&mut buffer, "event: ping\n\ndata: {\"x\":1}\n");
        assert_eq!(lines, vec!["{\"x\":1}".to_string()]);
    }
}
