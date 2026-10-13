//! Manual conversation compaction (M7).
//!
//! Compaction *summarizes* older conversation turns into a single synthetic
//! "system summary" message that is prepended to the conversation, while
//! preserving the most recent N turns verbatim. The original session JSONL
//! and any checkpoint history are untouched — compaction only changes what
//! gets sent to the model on the next turn.
//!
//! M7 implements a deterministic, provider-free fallback summarizer so it
//! works offline and in tests. The summary is intentionally compact: it
//! lists the role, length, and a one-line preview of each older message.
//! Callers that want a richer LLM-generated summary can substitute their
//! own implementation by calling `compact_conversation_with_summarizer`.

use serde::{Deserialize, Serialize};

use crate::conversation::{ContentBlock, Conversation, Message, MessageRole, ToolResultContent};

/// How aggressively older messages are preserved through compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPreservePolicy {
    /// All older content is summarized.
    #[default]
    SummarizeAll,
    /// Older content is summarized, but error tool-results are preserved
    /// verbatim so the model still sees recent failures.
    PreserveErrors,
    /// Older content is dropped from the model context entirely except for
    /// the most recent N turns. Most aggressive option.
    PreserveRecentOnly,
}

/// Knobs for the compaction engine.
#[derive(Debug, Clone)]
pub struct CompactionOptions {
    pub enabled: bool,
    pub preserve_recent_turns: usize,
    pub max_summary_tokens: usize,
    pub preserve_tool_results: CompactionPreservePolicy,
}

impl Default for CompactionOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            preserve_recent_turns: 6,
            max_summary_tokens: 4_000,
            preserve_tool_results: CompactionPreservePolicy::SummarizeAll,
        }
    }
}

/// Result of running compaction over a [`Conversation`].
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The new conversation that should be sent to the model.
    pub conversation: Conversation,
    /// Number of messages before compaction.
    pub pre_messages: usize,
    /// Number of messages after compaction.
    pub post_messages: usize,
    /// Number of trailing turns that were preserved verbatim.
    pub preserved_recent_turns: usize,
    /// One-line preview of the summary message.
    pub summary_preview: String,
    /// True if compaction had something to do (i.e. at least one message
    /// was rolled into a summary).
    pub compacted: bool,
}

/// Compact a conversation using the deterministic offline summarizer.
pub fn compact_conversation(
    conversation: &Conversation,
    options: &CompactionOptions,
) -> CompactionResult {
    compact_conversation_with_summarizer(conversation, options, deterministic_summary)
}

/// Compact a conversation with a caller-supplied summarizer. Used by
/// callers that want to delegate summarization to a real model.
pub fn compact_conversation_with_summarizer<F>(
    conversation: &Conversation,
    options: &CompactionOptions,
    summarizer: F,
) -> CompactionResult
where
    F: FnOnce(&[Message]) -> String,
{
    let pre = conversation.messages.len();
    if !options.enabled || pre == 0 {
        return CompactionResult {
            conversation: conversation.clone(),
            pre_messages: pre,
            post_messages: pre,
            preserved_recent_turns: pre,
            summary_preview: String::new(),
            compacted: false,
        };
    }
    let preserve = options.preserve_recent_turns.min(pre);
    if preserve >= pre {
        return CompactionResult {
            conversation: conversation.clone(),
            pre_messages: pre,
            post_messages: pre,
            preserved_recent_turns: preserve,
            summary_preview: String::new(),
            compacted: false,
        };
    }

    let split_at = pre - preserve;
    let older = &conversation.messages[..split_at];
    let recent = &conversation.messages[split_at..];

    let mut summary = match options.preserve_tool_results {
        CompactionPreservePolicy::PreserveRecentOnly => {
            // Drop older context entirely except for a one-line marker so
            // the model knows turns existed.
            format!(
                "[compacted] {} earlier turn(s) dropped from model context per preserve_recent_only policy.",
                older.len()
            )
        }
        _ => summarizer(older),
    };
    if let Some(max_chars) = options.max_summary_tokens.checked_mul(4) {
        if summary.len() > max_chars {
            truncate_utf8_in_place(&mut summary, max_chars);
            summary.push_str("\n...(summary truncated)");
        }
    }

    let mut new_conv = Conversation::new();
    new_conv.push(Message {
        role: MessageRole::System,
        content: vec![ContentBlock::Text {
            text: summary.clone(),
        }],
    });
    if matches!(
        options.preserve_tool_results,
        CompactionPreservePolicy::PreserveErrors
    ) {
        // Walk older messages and keep any error tool-results. The
        // originating assistant turn (with the matching `tool_use`) was
        // summarized away, so a bare `tool_result` would be an orphan that
        // the Anthropic API rejects (every `tool_result` must be
        // immediately preceded by an assistant message whose `tool_use`
        // shares its id). Synthesize a placeholder assistant `tool_use`
        // with the original id directly before each preserved result to
        // restore that pairing while keeping the error content intact.
        for msg in older {
            for block in &msg.content {
                if let ContentBlock::ToolResult(tr) = block {
                    if tr.is_error {
                        let tool_name = find_tool_name_for_id(older, &tr.tool_use_id)
                            .unwrap_or_else(|| "unknown".to_string());
                        new_conv.push(Message {
                            role: MessageRole::Assistant,
                            content: vec![ContentBlock::ToolUse(crate::conversation::ToolCall {
                                id: tr.tool_use_id.clone(),
                                name: tool_name,
                                input: serde_json::json!({}),
                            })],
                        });
                        new_conv.push(Message {
                            role: MessageRole::User,
                            content: vec![ContentBlock::ToolResult(tr.clone())],
                        });
                    }
                }
            }
        }
    }
    for m in recent {
        new_conv.push(m.clone());
    }
    let summary_preview = first_line_preview(&summary);

    CompactionResult {
        post_messages: new_conv.messages.len(),
        conversation: new_conv,
        pre_messages: pre,
        preserved_recent_turns: preserve,
        summary_preview,
        compacted: true,
    }
}

/// Look up the originating tool name for a `tool_use_id` by scanning the
/// older messages for the matching assistant `tool_use` block. Used to give
/// the synthesized placeholder assistant message a faithful tool name so the
/// reconstructed `tool_use`/`tool_result` pair matches the original call.
fn find_tool_name_for_id(messages: &[Message], tool_use_id: &str) -> Option<String> {
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse(call) = block {
                if call.id == tool_use_id {
                    return Some(call.name.clone());
                }
            }
        }
    }
    None
}

fn first_line_preview(s: &str) -> String {
    let mut out = s.lines().next().unwrap_or("").to_string();
    if out.len() > 200 {
        truncate_utf8_in_place(&mut out, 200);
        out.push_str("...");
    }
    out
}

/// Truncate `s` in place to at most `max_bytes` bytes without splitting a
/// UTF-8 codepoint. `String::truncate` panics on a non-codepoint boundary;
/// this helper backs up to the nearest boundary instead.
pub(crate) fn truncate_utf8_in_place(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Provider-free fallback summary. Lists each older message with its role
/// and a short content preview. Deterministic for tests.
pub fn deterministic_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[compacted-summary] {} prior turn(s) collapsed for context.\n",
        messages.len()
    ));
    for (i, m) in messages.iter().enumerate() {
        let role = match m.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        };
        let mut preview = String::new();
        for block in &m.content {
            match block {
                ContentBlock::Text { text } => {
                    preview.push_str(text);
                    preview.push(' ');
                }
                ContentBlock::ToolUse(call) => {
                    preview.push_str(&format!("[tool:{}] ", call.name));
                }
                ContentBlock::ToolResult(tr) => {
                    let mut text = String::new();
                    for c in &tr.content {
                        match c {
                            ToolResultContent::Text { text: t } => {
                                text.push_str(t);
                                text.push(' ');
                            }
                        }
                    }
                    preview.push_str(&format!(
                        "[tool-result{}] {}",
                        if tr.is_error { " error" } else { "" },
                        text.trim()
                    ));
                }
            }
        }
        let preview = first_line_preview(preview.trim());
        let preview = if preview.len() > 160 {
            let mut s = preview.clone();
            // Reserve room for the ellipsis so the final string never
            // exceeds 160 chars.
            truncate_utf8_in_place(&mut s, 157);
            s.push_str("...");
            s
        } else {
            preview
        };
        out.push_str(&format!("- [{i:>3}] {role}: {preview}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{ContentBlock, Conversation, Message, MessageRole};

    fn make_text(role: MessageRole, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn convo(n: usize) -> Conversation {
        let mut c = Conversation::new();
        for i in 0..n {
            c.push(make_text(MessageRole::User, &format!("msg {i}")));
        }
        c
    }

    #[test]
    // Compaction with no enabled flag is a no-op.
    fn disabled_is_noop() {
        let opts = CompactionOptions {
            enabled: false,
            ..CompactionOptions::default()
        };
        let r = compact_conversation(&convo(20), &opts);
        assert!(!r.compacted);
        assert_eq!(r.pre_messages, 20);
        assert_eq!(r.post_messages, 20);
    }

    #[test]
    // Compaction preserves the most recent N turns verbatim.
    fn preserves_recent_turns() {
        let opts = CompactionOptions {
            preserve_recent_turns: 3,
            ..Default::default()
        };
        let r = compact_conversation(&convo(10), &opts);
        assert!(r.compacted);
        // 1 summary + 3 recent.
        assert_eq!(r.post_messages, 4);
        assert_eq!(r.preserved_recent_turns, 3);
    }

    #[test]
    // Conversations shorter than the preserve window are not compacted.
    fn small_conversation_not_compacted() {
        let opts = CompactionOptions::default();
        let r = compact_conversation(&convo(2), &opts);
        assert!(!r.compacted);
    }

    #[test]
    // The summary first line is non-empty for a compacted conversation.
    fn summary_preview_set() {
        let opts = CompactionOptions {
            preserve_recent_turns: 1,
            ..Default::default()
        };
        let r = compact_conversation(&convo(5), &opts);
        assert!(!r.summary_preview.is_empty());
        assert!(r.summary_preview.contains("compacted-summary"));
    }

    #[test]
    // PreserveErrors policy keeps error tool-results in the new context.
    fn preserve_errors_keeps_errors() {
        let mut c = Conversation::new();
        c.push(make_text(MessageRole::User, "first"));
        c.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult(crate::conversation::ToolResult {
                tool_use_id: "1".into(),
                content: vec![ToolResultContent::Text {
                    text: "boom".into(),
                }],
                is_error: true,
            })],
        });
        for i in 0..6 {
            c.push(make_text(MessageRole::User, &format!("msg {i}")));
        }
        let opts = CompactionOptions {
            preserve_recent_turns: 3,
            preserve_tool_results: CompactionPreservePolicy::PreserveErrors,
            ..Default::default()
        };
        let r = compact_conversation(&c, &opts);
        // 1 summary + 1 placeholder tool_use + 1 preserved error + 3 recent
        assert!(r.post_messages >= 5);
    }

    #[test]
    // Regression for #163: every preserved error `tool_result` must be
    // immediately preceded by an assistant message containing a matching
    // `tool_use` id. Otherwise the Anthropic API rejects the orphan
    // `tool_result` with a 400.
    fn preserve_errors_pairs_tool_use_with_result() {
        let mut c = Conversation::new();
        // An originating assistant turn with the tool_use that produced the
        // error, plus a non-error result that must NOT be preserved.
        c.push(Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::ToolUse(crate::conversation::ToolCall {
                    id: "call_err".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "false"}),
                }),
                ContentBlock::ToolUse(crate::conversation::ToolCall {
                    id: "call_ok".into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                }),
            ],
        });
        c.push(Message {
            role: MessageRole::User,
            content: vec![
                ContentBlock::ToolResult(crate::conversation::ToolResult {
                    tool_use_id: "call_err".into(),
                    content: vec![ToolResultContent::Text {
                        text: "boom".into(),
                    }],
                    is_error: true,
                }),
                ContentBlock::ToolResult(crate::conversation::ToolResult {
                    tool_use_id: "call_ok".into(),
                    content: vec![ToolResultContent::Text { text: "ok".into() }],
                    is_error: false,
                }),
            ],
        });
        for i in 0..6 {
            c.push(make_text(MessageRole::User, &format!("msg {i}")));
        }
        let opts = CompactionOptions {
            preserve_recent_turns: 3,
            preserve_tool_results: CompactionPreservePolicy::PreserveErrors,
            ..Default::default()
        };
        let r = compact_conversation(&c, &opts);

        // Structural validity: for each message, any tool_result block must
        // have its id present as a tool_use in the immediately preceding
        // assistant message.
        let msgs = &r.conversation.messages;
        let mut saw_preserved_error = false;
        for (i, msg) in msgs.iter().enumerate() {
            for block in &msg.content {
                if let ContentBlock::ToolResult(tr) = block {
                    if !tr.is_error {
                        continue;
                    }
                    saw_preserved_error = true;
                    assert!(i > 0, "tool_result must have a preceding message");
                    let prev = &msgs[i - 1];
                    assert_eq!(
                        prev.role,
                        MessageRole::Assistant,
                        "tool_result must follow an assistant message"
                    );
                    let has_match = prev.content.iter().any(
                        |b| matches!(b, ContentBlock::ToolUse(call) if call.id == tr.tool_use_id),
                    );
                    assert!(
                        has_match,
                        "tool_result id {} has no matching tool_use in the preceding assistant message",
                        tr.tool_use_id
                    );
                }
            }
        }
        assert!(
            saw_preserved_error,
            "expected at least one preserved error tool_result"
        );

        // The placeholder reuses the original id and recovers the tool name.
        let placeholder = msgs.iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::ToolUse(call) if call.id == "call_err" => Some(call.clone()),
                _ => None,
            })
        });
        let placeholder = placeholder.expect("placeholder tool_use should exist");
        assert_eq!(placeholder.name, "Bash");

        // The non-error result must not be preserved (only errors are).
        let preserved_non_error = msgs.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult(tr) if tr.tool_use_id == "call_ok"))
        });
        assert!(
            !preserved_non_error,
            "non-error result should be summarized away"
        );
    }
}
