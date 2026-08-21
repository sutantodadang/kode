use kode_model::{Message, ToolSpec};

use crate::{AgentError, Result};

const DEFAULT_OUTPUT_TOKEN_RESERVE: usize = 4_096;
const MAX_RETAINED_TOOL_OUTPUT_TOKENS: usize = 2_048;
const TOOL_OUTPUT_TRUNCATED: &str = "\n[tool output truncated to fit context window]";
const CONTEXT_TRUNCATED: &str = "\n[repository context truncated to fit context window]";
const TASK_TRUNCATED: &str = "\n[user task truncated to fit context window]";
const HISTORY_DROPPED: &str = "(older conversation dropped to fit context window)";
const TOOL_ROUNDS_DROPPED: &str = "(older agent tool interactions dropped to fit context window)";

/// Applies a conservative, provider-independent context-window budget.
///
/// Exact tokenization differs by provider, so Kode uses the same four-byte
/// estimate as the context compiler and includes per-message/tool-schema
/// overhead. The output reserve is sent to the provider as `max_tokens`; the
/// remainder is the hard input budget used here.
pub(crate) struct PromptBudget {
    max_context_tokens: usize,
    output_tokens: u32,
}

impl PromptBudget {
    pub(crate) fn new(max_context_tokens: u32) -> Self {
        let max_context_tokens = max_context_tokens as usize;
        let proportional_reserve = (max_context_tokens / 4).max(1);
        let output_tokens = DEFAULT_OUTPUT_TOKEN_RESERVE
            .min(proportional_reserve)
            .min(u32::MAX as usize) as u32;
        Self {
            max_context_tokens,
            output_tokens,
        }
    }

    pub(crate) fn output_tokens(&self) -> u32 {
        self.output_tokens
    }

    pub(crate) fn input_budget(&self) -> usize {
        self.max_context_tokens
            .saturating_sub(self.output_tokens as usize)
    }

    pub(crate) fn prepare(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Vec<Message>> {
        let input_budget = self.input_budget();
        let mut prepared = messages.to_vec();

        // A single command result should never crowd all other evidence out
        // of the next turn, even when the total prompt still technically fits.
        for message in &mut prepared {
            if let Message::Tool { content, .. } = message
                && estimate_text(content) > MAX_RETAINED_TOOL_OUTPUT_TOKENS
            {
                truncate_to_tokens(
                    content,
                    MAX_RETAINED_TOOL_OUTPUT_TOKENS,
                    TOOL_OUTPUT_TRUNCATED,
                );
            }
        }

        // Preserve the most recent tool round where possible. Older rounds
        // are less useful than the current result and can carry large call
        // arguments even after their output has been compacted.
        while estimate_request(&prepared, tools) > input_budget {
            let rounds = completed_tool_rounds(&prepared);
            if rounds.len() <= 1 {
                break;
            }
            let (start, end) = rounds[0];
            prepared.drain(start..end);
            insert_marker(&mut prepared, TOOL_ROUNDS_DROPPED);
        }

        // Session history is replayed oldest-first, so discard the oldest
        // complete turn first and state that omission explicitly.
        while estimate_request(&prepared, tools) > input_budget {
            let Some((start, end)) = oldest_history_turn(&prepared) else {
                break;
            };
            prepared.drain(start..end);
            insert_marker(&mut prepared, HISTORY_DROPPED);
        }

        // The compiled repository blob is independently bounded, but its
        // configured budget may still be too large for a smaller model.
        shrink_matching_message(
            &mut prepared,
            tools,
            input_budget,
            |message| matches!(message, Message::System(text) if text.starts_with("Repository and session context:")),
            CONTEXT_TRUNCATED,
        );

        // Keep the newest tool round, but reduce its result if that is what
        // remains above budget.
        while estimate_request(&prepared, tools) > input_budget {
            let excess = estimate_request(&prepared, tools) - input_budget;
            let candidate = prepared.iter_mut().rev().find_map(|message| match message {
                Message::Tool { content, .. } if content != TOOL_OUTPUT_TRUNCATED => Some(content),
                _ => None,
            });
            let Some(content) = candidate else { break };
            let current = estimate_text(content);
            let target = current.saturating_sub(excess.max(1));
            if !truncate_to_tokens(content, target, TOOL_OUTPUT_TRUNCATED) {
                break;
            }
        }

        // If call arguments/protocol overhead alone are too large, remove the
        // remaining completed interaction rather than sending an invalid
        // partial tool protocol to providers.
        while estimate_request(&prepared, tools) > input_budget {
            let Some((start, end)) = completed_tool_rounds(&prepared).first().copied() else {
                break;
            };
            prepared.drain(start..end);
            insert_marker(&mut prepared, TOOL_ROUNDS_DROPPED);
        }

        // The original task is the last content sacrificed. Truncating it is
        // still preferable to silently exceeding the configured model window.
        shrink_matching_message(
            &mut prepared,
            tools,
            input_budget,
            |message| matches!(message, Message::User(_)),
            TASK_TRUNCATED,
        );

        let estimated = estimate_request(&prepared, tools);
        if estimated > input_budget {
            return Err(AgentError::ContextWindowExceeded {
                estimated,
                available: input_budget,
            });
        }

        Ok(prepared)
    }
}

fn estimate_request(messages: &[Message], tools: &[ToolSpec]) -> usize {
    // Provider wrappers and the request envelope carry a small fixed cost.
    let message_tokens = messages.iter().map(estimate_message).sum::<usize>();
    let tool_tokens = tools
        .iter()
        .map(|tool| {
            12 + estimate_text(&tool.name)
                + estimate_text(&tool.description)
                + estimate_text(&tool.parameters.to_string())
        })
        .sum::<usize>();
    8 + message_tokens + tool_tokens
}

fn estimate_message(message: &Message) -> usize {
    let content = match message {
        Message::System(content) | Message::User(content) => estimate_text(content),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            estimate_text(content)
                + tool_calls
                    .iter()
                    .map(|call| {
                        10 + estimate_text(&call.id)
                            + estimate_text(&call.name)
                            + estimate_text(&call.arguments.to_string())
                    })
                    .sum::<usize>()
        }
        Message::Tool {
            tool_call_id,
            content,
        } => estimate_text(tool_call_id) + estimate_text(content),
    };
    6 + content
}

fn estimate_text(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn completed_tool_rounds(messages: &[Message]) -> Vec<(usize, usize)> {
    let mut rounds = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let Message::Assistant { tool_calls, .. } = &messages[index] else {
            index += 1;
            continue;
        };
        if tool_calls.is_empty() {
            index += 1;
            continue;
        }

        let mut end = index + 1;
        while end < messages.len() && matches!(messages[end], Message::Tool { .. }) {
            end += 1;
        }
        if end > index + 1 {
            rounds.push((index, end));
        }
        index = end;
    }
    rounds
}

fn oldest_history_turn(messages: &[Message]) -> Option<(usize, usize)> {
    messages.windows(2).enumerate().find_map(|(index, pair)| {
        if matches!(pair[0], Message::User(_))
            && matches!(pair[1], Message::Assistant { ref tool_calls, .. } if tool_calls.is_empty())
            && index + 2 < messages.len()
        {
            Some((index, index + 2))
        } else {
            None
        }
    })
}

fn insert_marker(messages: &mut Vec<Message>, marker: &str) {
    if messages
        .iter()
        .any(|message| matches!(message, Message::System(text) if text == marker))
    {
        return;
    }
    let index = usize::from(!messages.is_empty());
    messages.insert(index, Message::System(marker.to_string()));
}

fn shrink_matching_message(
    messages: &mut [Message],
    tools: &[ToolSpec],
    input_budget: usize,
    predicate: impl Fn(&Message) -> bool,
    marker: &str,
) {
    while estimate_request(messages, tools) > input_budget {
        let excess = estimate_request(messages, tools) - input_budget;
        let Some(message) = messages.iter_mut().find(|message| predicate(message)) else {
            break;
        };
        let content = match message {
            Message::System(content) | Message::User(content) => content,
            _ => break,
        };
        let current = estimate_text(content);
        let target = current.saturating_sub(excess.max(1));
        if !truncate_to_tokens(content, target, marker) {
            break;
        }
    }
}

fn truncate_to_tokens(content: &mut String, target_tokens: usize, marker: &str) -> bool {
    if content == marker || estimate_text(content) <= target_tokens {
        return false;
    }

    let target_bytes = target_tokens.saturating_mul(4);
    if target_bytes <= marker.len() {
        *content = marker.to_string();
        return true;
    }

    let mut keep_bytes = target_bytes - marker.len();
    keep_bytes = keep_bytes.min(content.len());
    while keep_bytes > 0 && !content.is_char_boundary(keep_bytes) {
        keep_bytes -= 1;
    }
    content.truncate(keep_bytes);
    content.push_str(marker);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_model::ToolCall;

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read_file".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }]
    }

    #[test]
    fn reserves_output_and_keeps_small_prompt() {
        let budget = PromptBudget::new(20_000);
        let messages = vec![
            Message::System("system".into()),
            Message::User("task".into()),
        ];
        let prepared = budget.prepare(&messages, &tools()).unwrap();

        assert_eq!(budget.output_tokens(), 4_096);
        assert_eq!(prepared, messages);
        assert!(estimate_request(&prepared, &tools()) <= budget.input_budget());
    }

    #[test]
    fn oversized_context_is_truncated_honestly() {
        let budget = PromptBudget::new(1_000);
        let messages = vec![
            Message::System("system".into()),
            Message::System(format!(
                "Repository and session context:\n\n{}",
                "x".repeat(8_000)
            )),
            Message::User("task".into()),
        ];
        let prepared = budget.prepare(&messages, &[]).unwrap();

        assert!(estimate_request(&prepared, &[]) <= budget.input_budget());
        assert!(prepared.iter().any(|message| {
            matches!(message, Message::System(text) if text.contains(CONTEXT_TRUNCATED))
        }));
    }

    #[test]
    fn old_tool_rounds_are_removed_before_latest_round() {
        let budget = PromptBudget::new(900);
        let call = |id: &str| ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": format!("{id}.txt")}),
        };
        let messages = vec![
            Message::System("system".into()),
            Message::User("task".into()),
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![call("old")],
            },
            Message::Tool {
                tool_call_id: "old".into(),
                content: "a".repeat(3_000),
            },
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![call("new")],
            },
            Message::Tool {
                tool_call_id: "new".into(),
                content: "b".repeat(3_000),
            },
        ];
        let prepared = budget.prepare(&messages, &[]).unwrap();

        assert!(estimate_request(&prepared, &[]) <= budget.input_budget());
        assert!(!prepared.iter().any(
            |message| matches!(message, Message::Tool { tool_call_id, .. } if tool_call_id == "old")
        ));
        assert!(prepared.iter().any(|message| {
            matches!(message, Message::System(text) if text == TOOL_ROUNDS_DROPPED)
        }));
    }

    #[test]
    fn impossible_fixed_overhead_returns_error() {
        let budget = PromptBudget::new(8);
        let error = budget
            .prepare(&[Message::System("system".into())], &tools())
            .unwrap_err();
        assert!(matches!(error, AgentError::ContextWindowExceeded { .. }));
    }
}
