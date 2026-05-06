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
        // Walk older messages and keep any error tool-results.
        for msg in older {
            for block in &msg.content {
                if let ContentBlock::ToolResult(tr) = block {
                    if tr.is_error {
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
        // 1 summary + 1 preserved error + 3 recent = 5
        assert!(r.post_messages >= 5);
    }
}
