//! Composer completion popup state.
//!
//! Phase D introduces three kinds of in-composer autocompletion:
//!
//! * **Slash commands** — when the composer buffer starts with `/`,
//!   matching slash commands are offered.
//! * **Context tokens** — when the buffer contains `@<name>` and
//!   `<name>` matches a known special token (e.g. `@plan`, `@changed`),
//!   the popup offers those tokens.
//! * **File paths** — `@<path>` outside the context-token namespace
//!   resolves to filesystem completions sandboxed to the workspace
//!   root.
//!
//! All trigger detection is pure (input = buffer + cursor, output =
//! optional trigger description) so the reducer can refresh popup
//! state after every keystroke without async / I/O.

/// The kind of completion currently being offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Slash,
    ContextToken,
    File,
}

/// One candidate the user can choose. `display` is what the popup
/// shows; `insert_text` is what gets pasted into the composer when
/// the user accepts the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub display: String,
    pub insert_text: String,
}

/// Active popup state. Owned by the reducer; `None` means no popup is
/// currently visible.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompletionPopup {
    pub kind: CompletionKind,
    pub candidates: Vec<Candidate>,
    /// Byte offset of the start of the token being completed.
    pub token_start: usize,
    /// Byte offset of the (cursor) end of the token.
    pub token_end: usize,
    pub selected: usize,
}

impl CompletionPopup {
    pub fn new(
        kind: CompletionKind,
        candidates: Vec<Candidate>,
        token_start: usize,
        token_end: usize,
    ) -> Self {
        Self {
            kind,
            candidates,
            token_start,
            token_end,
            selected: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Move the selection cursor forward / backward, wrapping at the
    /// ends.
    pub fn next(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
    }

    pub fn prev(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.candidates.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn selected_candidate(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }
}

/// Trigger detected at the current cursor position. Built by
/// [`detect_trigger`] and consumed by the reducer to populate the
/// popup. `kind` decides which catalog to query for candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerSpec {
    pub kind: CompletionKind,
    pub token: String,
    pub token_start: usize,
    pub token_end: usize,
}

/// Inspect the composer buffer + cursor position and return a
/// [`TriggerSpec`] if one of the completion contexts applies. Pure
/// function — the reducer calls it after every key that mutates the
/// buffer or cursor.
pub fn detect_trigger(buffer: &str, cursor: usize) -> Option<TriggerSpec> {
    if cursor > buffer.len() || !buffer.is_char_boundary(cursor) {
        return None;
    }
    let before_cursor = &buffer[..cursor];

    // Slash commands: only when the buffer starts with `/` and no
    // whitespace precedes the cursor. The user typing "fix /var" must
    // not trigger slash completion. The matched `token` is the prefix
    // *before* the cursor (so mid-word edits filter correctly); the
    // matched `token_end` extends to the end of the surrounding word
    // (so accept replaces both prefix and suffix).
    if before_cursor.starts_with('/') && !before_cursor[1..].contains(char::is_whitespace) {
        let token = before_cursor[1..].to_string();
        let token_end = end_of_word(buffer, cursor);
        return Some(TriggerSpec {
            kind: CompletionKind::Slash,
            token,
            token_start: 0,
            token_end,
        });
    }

    // @ token: walk backwards from the cursor to find an `@` that
    // begins the current token. Only fires when the `@` is at the
    // start of input or preceded by whitespace. Like the slash path,
    // `token` is the prefix before the cursor and `token_end` extends
    // through any suffix the user already typed past the cursor.
    if let Some(at_offset) = find_at_token_start(buffer, cursor) {
        let token = &buffer[at_offset + 1..cursor];
        if token.is_empty() || token.chars().all(|c| !c.is_whitespace()) {
            let kind = if is_context_token_prefix(token) {
                CompletionKind::ContextToken
            } else {
                CompletionKind::File
            };
            return Some(TriggerSpec {
                kind,
                token: token.to_string(),
                token_start: at_offset,
                token_end: end_of_word(buffer, cursor),
            });
        }
    }

    None
}

/// Return the byte offset of the end of the (whitespace-delimited)
/// word that starts at or before `cursor`. If the cursor is at the
/// end of the buffer or already at whitespace, returns `cursor`.
fn end_of_word(buffer: &str, cursor: usize) -> usize {
    let after = &buffer[cursor..];
    match after.find(char::is_whitespace) {
        Some(w) => cursor + w,
        None => buffer.len(),
    }
}

/// Find the byte offset of the `@` that begins the @-token currently
/// being typed at `cursor`, or `None` if there isn't one.
fn find_at_token_start(buffer: &str, cursor: usize) -> Option<usize> {
    let before = &buffer[..cursor];
    let mut i = cursor;
    for (idx, ch) in before.char_indices().rev() {
        if ch.is_whitespace() {
            return None;
        }
        if ch == '@' {
            // Must be at start-of-input or preceded by whitespace.
            if idx == 0
                || before[..idx]
                    .chars()
                    .next_back()
                    .map(|c| c.is_whitespace())
                    .unwrap_or(true)
            {
                return Some(idx);
            }
            return None;
        }
        i = idx;
    }
    let _ = i;
    None
}

/// Static list of context tokens the TUI exposes. Filtered by prefix
/// in [`context_token_candidates`].
pub const CONTEXT_TOKENS: &[&str] = &[
    "changed",
    "plan",
    "last-error",
    "last-diff",
    "checkpoint",
    "session",
    "tool",
    "agent",
    "skill",
    "plugin",
    "mcp",
];

fn is_context_token_prefix(token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    CONTEXT_TOKENS.iter().any(|t| t.starts_with(token))
}

/// Build candidates for the context-token namespace, filtered by
/// `partial`. Each candidate inserts `@<token> ` (with a trailing
/// space so the user can keep typing) when accepted.
pub fn context_token_candidates(partial: &str, limit: usize) -> Vec<Candidate> {
    CONTEXT_TOKENS
        .iter()
        .filter(|t| t.starts_with(partial))
        .take(limit)
        .map(|t| Candidate {
            display: format!("@{t}"),
            insert_text: format!("@{t} "),
        })
        .collect()
}

/// Build candidates for slash commands. Caller supplies a static list
/// of available command names (`slash_commands`).
pub fn slash_candidates(
    partial: &str,
    slash_commands: &[&'static str],
    limit: usize,
) -> Vec<Candidate> {
    slash_commands
        .iter()
        .filter(|c| c.starts_with(partial))
        .take(limit)
        .map(|c| Candidate {
            display: format!("/{c}"),
            insert_text: format!("/{c}"),
        })
        .collect()
}

/// The canonical list of slash commands the TUI understands. Kept in
/// sync with `state::apply_slash_command`.
pub const SLASH_COMMANDS: &[&str] = &[
    // Side-panel switches.
    "plan",
    "changes",
    "files",
    "tools",
    "sessions",
    "diagnostics",
    "agents",
    "mcp",
    "plugins",
    "skills",
    "usage",
    "auth",
    "model",
    "provider",
    "subagents",
    // Modals.
    "help",
    "keys",
    "palette",
    "search",
    "history",
    // Toggles.
    "toggle-side",
    "side",
    "toggle-detail",
    "detail",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Slash trigger fires only at the start of a buffer that begins
    // with `/`. Embedded slashes elsewhere in the prompt do not.
    fn slash_trigger_fires_only_at_start() {
        let t = detect_trigger("/fil", 4).unwrap();
        assert_eq!(t.kind, CompletionKind::Slash);
        assert_eq!(t.token, "fil");
        assert!(detect_trigger("look at /etc/hosts", 18).is_none());
    }

    #[test]
    // A bare `/` triggers Slash with an empty token (full list).
    fn slash_trigger_with_empty_token_lists_all() {
        let t = detect_trigger("/", 1).unwrap();
        assert_eq!(t.kind, CompletionKind::Slash);
        assert!(t.token.is_empty());
    }

    #[test]
    // After whitespace, the slash trigger no longer applies.
    fn slash_trigger_loses_after_space() {
        assert!(detect_trigger("/files now", 10).is_none());
    }

    #[test]
    // `@changed` (known context token) triggers ContextToken kind.
    fn at_known_token_triggers_context() {
        let t = detect_trigger("look at @cha", 12).unwrap();
        assert_eq!(t.kind, CompletionKind::ContextToken);
        assert_eq!(t.token, "cha");
    }

    #[test]
    // `@src/` falls back to File kind because no context token
    // starts with `src/`.
    fn at_unknown_prefix_triggers_file() {
        let t = detect_trigger("look at @src/m", 14).unwrap();
        assert_eq!(t.kind, CompletionKind::File);
        assert_eq!(t.token, "src/m");
    }

    #[test]
    // An `@` not preceded by whitespace is not a trigger (e.g. an
    // email-like literal).
    fn at_after_letters_does_not_trigger() {
        assert!(detect_trigger("foo@bar", 7).is_none());
    }

    #[test]
    // Context-token candidates filter on prefix and cap at limit.
    fn context_token_candidates_filter_and_cap() {
        let cs = context_token_candidates("l", 10);
        let displays: Vec<&str> = cs.iter().map(|c| c.display.as_str()).collect();
        assert!(displays.contains(&"@last-error"));
        assert!(displays.contains(&"@last-diff"));
        // Cap test.
        let cs2 = context_token_candidates("", 3);
        assert_eq!(cs2.len(), 3);
    }

    #[test]
    // Popup next/prev wrap around at the ends.
    fn popup_navigation_wraps() {
        let mut p = CompletionPopup::new(
            CompletionKind::Slash,
            vec![
                Candidate {
                    display: "/a".into(),
                    insert_text: "/a".into(),
                },
                Candidate {
                    display: "/b".into(),
                    insert_text: "/b".into(),
                },
            ],
            0,
            1,
        );
        assert_eq!(p.selected, 0);
        p.next();
        assert_eq!(p.selected, 1);
        p.next();
        assert_eq!(p.selected, 0);
        p.prev();
        assert_eq!(p.selected, 1);
    }

    #[test]
    // Mid-word slash editing: cursor in the middle of `/plan` filters
    // by the prefix BEFORE the cursor, not the whole token.
    fn slash_trigger_mid_word_uses_prefix_only() {
        // `/pl|an`: cursor after `/pl`, suffix `an` follows.
        let t = detect_trigger("/plan", 3).unwrap();
        assert_eq!(t.kind, CompletionKind::Slash);
        assert_eq!(t.token, "pl");
        // token_end must span the whole word so the accept path
        // replaces both `pl` AND `an`.
        assert_eq!(t.token_end, 5);
    }

    #[test]
    // Mid-word `@` editing: cursor in the middle of `@changed`
    // filters by the prefix BEFORE the cursor.
    fn at_trigger_mid_word_uses_prefix_only() {
        // `@ch|anged`: cursor after `@ch`.
        let t = detect_trigger("@changed", 3).unwrap();
        assert_eq!(t.kind, CompletionKind::ContextToken);
        assert_eq!(t.token, "ch");
        assert_eq!(t.token_end, 8);
    }

    #[test]
    // Slash candidates filter on prefix.
    fn slash_candidates_filter() {
        let cs = slash_candidates("p", SLASH_COMMANDS, 10);
        let displays: Vec<&str> = cs.iter().map(|c| c.display.as_str()).collect();
        assert!(displays.contains(&"/plan"));
        assert!(displays.contains(&"/plugins"));
        assert!(displays.contains(&"/palette"));
        assert!(displays.contains(&"/provider"));
    }
}
