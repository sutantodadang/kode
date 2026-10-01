use kode_model::{Message, ToolSpec};

use crate::{AgentError, Result};

const MAX_RETAINED_TOOL_OUTPUT_TOKENS: usize = 2_048;
const TOOL_OUTPUT_TRUNCATED: &str = "\n[tool output truncated to fit context window]";
const CONTEXT_TRUNCATED: &str = "\n[repository context truncated to fit context window]";
const TASK_TRUNCATED: &str = "\n[user task truncated to fit context window]";
const HISTORY_DROPPED: &str = "(older conversation dropped to fit context window)";
const TOOL_ROUNDS_DROPPED: &str = "(older agent tool interactions dropped to fit context window)";
const IMAGE_TOKEN_ESTIMATE: usize = 1600;
const AUTO_COMPACT_TRIGGER_PERCENT: usize = 80;

const MASK_TRIGGER_PERCENT: usize = 60;
/// Tool rounds whose outputs always stay in full: the model is still
/// working with them.
pub(crate) const KEEP_RECENT_TOOL_ROUNDS: usize = 4;
pub(crate) const TOOL_OUTPUT_MASKED: &str =
    "[output omitted to save context; call the tool again if you need it]";
/// Outputs at or below this size stay: masking them saves nothing, and short
/// outputs are usually errors the model should keep seeing.
const MASK_MIN_BYTES: usize = 200;
/// How far `enforce` shrinks an over-budget prompt. Dropping well below the
/// limit in one go means the next several requests need no further drops, so
/// the prompt prefix shifts once instead of on every request.
const DROP_TARGET_PERCENT: usize = 75;

/// Opens the message that carries the compiled repository context.
pub(crate) const CONTEXT_PREFIX: &str = "Repository and session context:";

/// The compiled repository context travels as a user message next to the
/// task, so the system prefix stays identical from task to task.
pub(crate) fn is_context_message(message: &Message) -> bool {
    matches!(message, Message::User(text)
        if text.starts_with(CONTEXT_PREFIX) || text.ends_with(CONTEXT_TRUNCATED))
}

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
        // Reasoning-heavy models (e.g. GLM via opencode-go) can spend most of
        // an output budget on hidden thinking before emitting a tool call; a
        // flat 4k reserve truncates mid-tool-call. Scale with the window,
        // clamped so tiny windows keep a usable input budget (never reserve
        // more than a quarter of the window).
        let output_tokens = (max_context_tokens / 8)
            .clamp(2_048, 16_384)
            .min((max_context_tokens / 4).max(1)) as u32;
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

    pub(crate) fn context_window(&self) -> usize {
        self.max_context_tokens
    }

    pub(crate) fn estimate(&self, messages: &[Message], tools: &[ToolSpec]) -> usize {
        estimate_request(messages, tools)
    }

    pub(crate) fn should_compact(&self, messages: &[Message], tools: &[ToolSpec]) -> bool {
        self.input_budget() > 0
            && estimate_request(messages, tools).saturating_mul(100)
                >= self
                    .input_budget()
                    .saturating_mul(AUTO_COMPACT_TRIGGER_PERCENT)
    }

    /// True once the prompt is large enough that stale tool outputs should be
    /// omitted. Lower than the compaction trigger: masking is free, a
    /// summary is a model call.
    pub(crate) fn should_mask(&self, messages: &[Message], tools: &[ToolSpec]) -> bool {
        self.input_budget() > 0
            && estimate_request(messages, tools).saturating_mul(100)
                >= self.input_budget().saturating_mul(MASK_TRIGGER_PERCENT)
    }

    /// When the prompt no longer fits, removes the oldest completed tool
    /// rounds from `messages` itself until it is back under
    /// [`DROP_TARGET_PERCENT`] of the budget, and returns how many rounds it
    /// removed. The newest round always stays. A user-role marker is left
    /// where the rounds were: a system marker would change the system block
    /// and invalidate the provider's cache for the whole conversation.
    pub(crate) fn enforce(&self, messages: &mut Vec<Message>, tools: &[ToolSpec]) -> usize {
        let budget = self.input_budget();
        if estimate_request(messages, tools) <= budget {
            return 0;
        }
        let target = budget * DROP_TARGET_PERCENT / 100;
        let mut dropped = 0;
        let mut gap = None;
        while estimate_request(messages, tools) > target {
            let rounds = completed_tool_rounds(messages);
            if rounds.len() <= 1 {
                break;
            }
            let (start, end) = rounds[0];
            messages.drain(start..end);
            gap.get_or_insert(start);
            dropped += 1;
        }
        let marked = messages
            .iter()
            .any(|message| matches!(message, Message::User(text) if text == TOOL_ROUNDS_DROPPED));
        if let Some(index) = gap
            && !marked
        {
            messages.insert(index, Message::User(TOOL_ROUNDS_DROPPED.to_string()));
        }
        dropped
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
            is_context_message,
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
            |message| {
                matches!(message, Message::User(_) | Message::UserWithImages { .. })
                    && !is_context_message(message)
            },
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
        Message::UserWithImages { content, images } => {
            estimate_text(content) + images.len() * IMAGE_TOKEN_ESTIMATE
        }
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

pub(crate) fn completed_tool_rounds(messages: &[Message]) -> Vec<(usize, usize)> {
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

/// Replaces the output of every completed tool round except the newest
/// `keep_recent` with a short placeholder and returns how many outputs it
/// replaced. Calls, arguments and ids stay, so the model still sees what it
/// did and the tool protocol stays valid. Idempotent.
pub(crate) fn mask_old_tool_outputs(messages: &mut [Message], keep_recent: usize) -> usize {
    let rounds = completed_tool_rounds(messages);
    let maskable = rounds.len().saturating_sub(keep_recent);
    let mut masked = 0;
    for &(start, end) in &rounds[..maskable] {
        for message in &mut messages[start..end] {
            if let Message::Tool { content, .. } = message
                && content.len() > MASK_MIN_BYTES
            {
                *content = TOOL_OUTPUT_MASKED.to_string();
                masked += 1;
            }
        }
    }
    masked
}

fn oldest_history_turn(messages: &[Message]) -> Option<(usize, usize)> {
    messages.windows(2).enumerate().find_map(|(index, pair)| {
        if matches!(pair[0], Message::User(_) | Message::UserWithImages { .. })
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
            Message::UserWithImages { content, .. } => content,
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

        // 20k window / 8 = 2500, clamped up to the 2048 floor... 2500 > 2048
        // so the raw proportional value wins.
        assert_eq!(budget.output_tokens(), 2_500);
        assert_eq!(prepared, messages);
        assert!(estimate_request(&prepared, &tools()) <= budget.input_budget());
    }

    #[test]
    fn output_reserve_scales_with_window() {
        // Tiny windows stay quarter-capped so the input budget survives.
        assert_eq!(PromptBudget::new(1_000).output_tokens(), 250);
        assert_eq!(PromptBudget::new(8_000).output_tokens(), 2_000);
        assert_eq!(PromptBudget::new(16_000).output_tokens(), 2_048);
        assert_eq!(PromptBudget::new(128_000).output_tokens(), 16_000);
        assert_eq!(PromptBudget::new(872_000).output_tokens(), 16_384);
    }

    #[test]
    fn oversized_context_is_truncated_honestly() {
        let budget = PromptBudget::new(1_000);
        let messages = vec![
            Message::System("system".into()),
            Message::User(format!("{CONTEXT_PREFIX}\n\n{}", "x".repeat(8_000))),
            Message::User("task".into()),
        ];
        let prepared = budget.prepare(&messages, &[]).unwrap();

        assert!(estimate_request(&prepared, &[]) <= budget.input_budget());
        assert!(matches!(
            &prepared[1],
            Message::User(text) if text.starts_with(CONTEXT_PREFIX) && text.ends_with(CONTEXT_TRUNCATED)
        ));
        assert_eq!(prepared[2], Message::User("task".into()));
    }

    #[test]
    fn task_truncation_never_targets_the_context_message() {
        let budget = PromptBudget::new(600);
        let messages = vec![
            Message::System("system".into()),
            Message::User(format!("{CONTEXT_PREFIX}\n\nshort")),
            Message::User("t".repeat(8_000)),
        ];
        let prepared = budget.prepare(&messages, &[]).unwrap();

        assert!(estimate_request(&prepared, &[]) <= budget.input_budget());
        assert!(matches!(
            &prepared[2],
            Message::User(text) if text.ends_with(TASK_TRUNCATED)
        ));
        assert!(is_context_message(&prepared[1]));
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

    fn round(id: &str, output: String) -> [Message; 2] {
        [
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": format!("{id}.txt")}),
                }],
            },
            Message::Tool {
                tool_call_id: id.into(),
                content: output,
            },
        ]
    }

    fn conversation(rounds: usize, output_bytes: usize) -> Vec<Message> {
        let mut messages = vec![
            Message::System("system".into()),
            Message::User("task".into()),
        ];
        for n in 0..rounds {
            messages.extend(round(&format!("r{n}"), "o".repeat(output_bytes)));
        }
        messages
    }

    fn tool_content<'a>(messages: &'a [Message], id: &str) -> &'a str {
        messages
            .iter()
            .find_map(|message| match message {
                Message::Tool {
                    tool_call_id,
                    content,
                } if tool_call_id == id => Some(content.as_str()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no tool result for {id}"))
    }

    #[test]
    fn masking_keeps_the_newest_rounds_and_is_idempotent() {
        let mut messages = conversation(6, 1_000);

        let masked = mask_old_tool_outputs(&mut messages, KEEP_RECENT_TOOL_ROUNDS);

        assert_eq!(masked, 2);
        assert_eq!(tool_content(&messages, "r0"), TOOL_OUTPUT_MASKED);
        assert_eq!(tool_content(&messages, "r1"), TOOL_OUTPUT_MASKED);
        for id in ["r2", "r3", "r4", "r5"] {
            assert_eq!(tool_content(&messages, id).len(), 1_000, "{id}");
        }
        // Every call still has its result: the tool protocol is intact.
        assert_eq!(completed_tool_rounds(&messages).len(), 6);
        assert_eq!(messages.len(), 2 + 6 * 2);

        assert_eq!(
            mask_old_tool_outputs(&mut messages, KEEP_RECENT_TOOL_ROUNDS),
            0
        );
    }

    #[test]
    fn masking_leaves_short_outputs_such_as_errors_readable() {
        let mut messages = vec![
            Message::System("system".into()),
            Message::User("task".into()),
        ];
        messages.extend(round("failed", "error: file not found".into()));
        for n in 0..5 {
            messages.extend(round(&format!("r{n}"), "o".repeat(1_000)));
        }

        mask_old_tool_outputs(&mut messages, KEEP_RECENT_TOOL_ROUNDS);

        assert_eq!(tool_content(&messages, "failed"), "error: file not found");
        assert_eq!(tool_content(&messages, "r0"), TOOL_OUTPUT_MASKED);
    }

    #[test]
    fn masking_never_touches_user_or_system_messages() {
        let mut messages = conversation(3, 1_000);
        messages.insert(4, Message::User("steer: focus on the parser".into()));
        messages.extend(conversation(4, 1_000).into_iter().skip(2));
        let non_tool_before: Vec<Message> = messages
            .iter()
            .filter(|message| !matches!(message, Message::Tool { .. }))
            .cloned()
            .collect();

        mask_old_tool_outputs(&mut messages, KEEP_RECENT_TOOL_ROUNDS);

        let non_tool_after: Vec<Message> = messages
            .iter()
            .filter(|message| !matches!(message, Message::Tool { .. }))
            .cloned()
            .collect();
        assert_eq!(non_tool_before, non_tool_after);
    }

    #[test]
    fn nothing_is_masked_when_there_are_few_rounds() {
        let mut messages = conversation(KEEP_RECENT_TOOL_ROUNDS, 1_000);
        assert_eq!(
            mask_old_tool_outputs(&mut messages, KEEP_RECENT_TOOL_ROUNDS),
            0
        );
    }

    #[test]
    fn should_mask_triggers_before_should_compact() {
        // 4,000-token window: 1,000 reserved for output, 3,000 input budget.
        let budget = PromptBudget::new(4_000);
        let empty = conversation(0, 0);
        let at_65_percent = conversation(1, 7_600); // about 1,950 tokens
        let at_85_percent = conversation(1, 10_000); // about 2,550 tokens

        assert!(!budget.should_mask(&empty, &[]));
        assert!(budget.should_mask(&at_65_percent, &[]));
        assert!(!budget.should_compact(&at_65_percent, &[]));
        assert!(budget.should_mask(&at_85_percent, &[]));
        assert!(budget.should_compact(&at_85_percent, &[]));
    }

    #[test]
    fn enforce_drops_a_block_and_marks_the_gap() {
        // 3,000-token input budget; eight rounds of about 530 tokens each.
        let budget = PromptBudget::new(4_000);
        let mut messages = conversation(8, 2_000);
        assert!(estimate_request(&messages, &[]) > budget.input_budget());

        let dropped = budget.enforce(&mut messages, &[]);

        assert!(dropped >= 2, "one block, not one round: {dropped}");
        assert!(estimate_request(&messages, &[]) <= budget.input_budget() * 75 / 100);
        // The marker sits where the rounds were, as a user message.
        assert_eq!(messages[0], Message::System("system".into()));
        assert_eq!(messages[1], Message::User("task".into()));
        assert_eq!(messages[2], Message::User(TOOL_ROUNDS_DROPPED.into()));
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::System(_)))
                .count(),
            1
        );
        // The newest round survives in full.
        assert_eq!(tool_content(&messages, "r7").len(), 2_000);
        assert_eq!(completed_tool_rounds(&messages).len(), 8 - dropped);

        // Stable afterwards: the next request does not shift the prefix again.
        let snapshot = messages.clone();
        assert_eq!(budget.enforce(&mut messages, &[]), 0);
        assert_eq!(messages, snapshot);
    }

    #[test]
    fn enforce_is_a_noop_within_budget() {
        let budget = PromptBudget::new(4_000);
        let mut messages = conversation(2, 2_000);
        let snapshot = messages.clone();

        assert_eq!(budget.enforce(&mut messages, &[]), 0);
        assert_eq!(messages, snapshot);
    }

    #[test]
    fn enforce_never_drops_the_only_round() {
        let budget = PromptBudget::new(4_000);
        let mut messages = conversation(1, 20_000);
        let snapshot = messages.clone();

        assert_eq!(budget.enforce(&mut messages, &[]), 0);
        assert_eq!(messages, snapshot);
    }

    #[test]
    fn enforce_keeps_user_steering_between_rounds() {
        let budget = PromptBudget::new(4_000);
        let mut messages = conversation(8, 2_000);
        // After round r0 (indices 2 and 3).
        messages.insert(4, Message::User("steer: focus on the parser".into()));

        budget.enforce(&mut messages, &[]);

        assert!(
            messages
                .iter()
                .any(|message| message == &Message::User("steer: focus on the parser".into()))
        );
    }

    #[test]
    fn enforce_adds_only_one_marker_across_repeated_overflows() {
        let budget = PromptBudget::new(4_000);
        let mut messages = conversation(8, 2_000);
        budget.enforce(&mut messages, &[]);
        for n in 8..14 {
            messages.extend(round(&format!("r{n}"), "o".repeat(2_000)));
        }

        budget.enforce(&mut messages, &[]);

        assert_eq!(
            messages
                .iter()
                .filter(
                    |message| matches!(message, Message::User(text) if text == TOOL_ROUNDS_DROPPED)
                )
                .count(),
            1
        );
    }
}
