//! Composer state: multiline input buffer, prompt history, and slash/file
//! autocomplete. The state is fully synchronous and pure — terminal I/O is
//! handled by the caller — so unit tests can exercise it deterministically.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use code_agent_core::{ComposerConfig, SubmitKey};

/// Raw key event from the terminal. Mirrors crossterm's KeyEvent without
/// pulling crossterm into tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Char(char),
    Enter,
    ShiftEnter,
    AltEnter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    CtrlC,
    CtrlD,
    CtrlR,
    Esc,
}

/// Action the host application should take after the composer processes a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    None,
    Submit(String),
    Interrupt,
    Exit,
    HistorySearch,
}

/// Simple ring buffer of past prompts. Capped at the configured limit and
/// transparently rejects entries that look like secrets.
#[derive(Debug, Clone)]
pub struct PromptHistory {
    buf: VecDeque<String>,
    limit: usize,
}

impl PromptHistory {
    pub fn new(limit: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    /// Push a prompt onto the history. Empty strings and entries that look
    /// like obvious secrets are dropped.
    pub fn push(&mut self, entry: &str) {
        let trimmed = entry.trim();
        if trimmed.is_empty() || looks_like_secret(trimmed) {
            return;
        }
        if let Some(last) = self.buf.back() {
            if last == trimmed {
                return;
            }
        }
        self.buf.push_back(trimmed.to_string());
        while self.buf.len() > self.limit {
            self.buf.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn entries(&self) -> Vec<String> {
        self.buf.iter().cloned().collect()
    }

    pub fn nth_from_end(&self, n: usize) -> Option<&str> {
        if n == 0 || n > self.buf.len() {
            return None;
        }
        self.buf.get(self.buf.len() - n).map(|s| s.as_str())
    }
}

fn looks_like_secret(s: &str) -> bool {
    // Conservative — drop obvious key/token shapes so they never land in
    // history. Anything more advanced lives in the existing secret hooks.
    let low = s.to_ascii_lowercase();
    if low.contains("api_key") || low.contains("api-key") || low.contains("password=") {
        return true;
    }
    if low.starts_with("export ") && low.contains("=") {
        return true;
    }
    // Long base64-ish blobs.
    if s.len() > 40
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '/' || c == '+' || c == '=')
    {
        return true;
    }
    false
}

/// Composer state machine. Owns the input buffer + cursor position and
/// processes keys into actions.
#[derive(Debug, Clone)]
pub struct ComposerState {
    pub buffer: String,
    pub cursor: usize,
    pub config: ComposerConfig,
    pub history: PromptHistory,
    history_cursor: Option<usize>,
}

impl ComposerState {
    pub fn new(config: ComposerConfig) -> Self {
        let limit = config.history_limit;
        Self {
            buffer: String::new(),
            cursor: 0,
            config,
            history: PromptHistory::new(limit),
            history_cursor: None,
        }
    }

    /// Process a key. Returns the resulting action and mutates the buffer.
    pub fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        match key {
            KeyEvent::Char(c) => {
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.history_cursor = None;
                KeyAction::None
            }
            KeyEvent::Backspace => {
                if self.cursor > 0 {
                    let prev = self.buffer[..self.cursor]
                        .chars()
                        .next_back()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                    let new_cursor = self.cursor - prev;
                    self.buffer.replace_range(new_cursor..self.cursor, "");
                    self.cursor = new_cursor;
                }
                KeyAction::None
            }
            KeyEvent::Enter => match self.config.submit_key {
                SubmitKey::Enter => self.submit(),
                SubmitKey::CtrlEnter => {
                    if self.config.multiline {
                        self.buffer.insert(self.cursor, '\n');
                        self.cursor += 1;
                    }
                    KeyAction::None
                }
            },
            KeyEvent::ShiftEnter | KeyEvent::AltEnter => {
                if self.config.multiline {
                    self.buffer.insert(self.cursor, '\n');
                    self.cursor += 1;
                }
                KeyAction::None
            }
            KeyEvent::Up => {
                let n = self.history_cursor.unwrap_or(0).saturating_add(1);
                if let Some(prev) = self.history.nth_from_end(n) {
                    self.buffer = prev.to_string();
                    self.cursor = self.buffer.len();
                    self.history_cursor = Some(n);
                }
                KeyAction::None
            }
            KeyEvent::Down => {
                if let Some(n) = self.history_cursor {
                    if n > 1 {
                        let new_n = n - 1;
                        if let Some(next) = self.history.nth_from_end(new_n) {
                            self.buffer = next.to_string();
                            self.cursor = self.buffer.len();
                            self.history_cursor = Some(new_n);
                        }
                    } else {
                        self.buffer.clear();
                        self.cursor = 0;
                        self.history_cursor = None;
                    }
                }
                KeyAction::None
            }
            KeyEvent::Left => {
                if self.cursor > 0 {
                    let prev = self.buffer[..self.cursor]
                        .chars()
                        .next_back()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                    self.cursor -= prev;
                }
                KeyAction::None
            }
            KeyEvent::Right => {
                if self.cursor < self.buffer.len() {
                    let next = self.buffer[self.cursor..]
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                    self.cursor += next;
                }
                KeyAction::None
            }
            KeyEvent::CtrlC => {
                if self.buffer.is_empty() {
                    KeyAction::Interrupt
                } else {
                    self.buffer.clear();
                    self.cursor = 0;
                    KeyAction::None
                }
            }
            KeyEvent::CtrlD => {
                if self.buffer.is_empty() {
                    KeyAction::Exit
                } else {
                    KeyAction::None
                }
            }
            KeyEvent::CtrlR => KeyAction::HistorySearch,
            KeyEvent::Esc => KeyAction::None,
            KeyEvent::Tab => KeyAction::None,
        }
    }

    fn submit(&mut self) -> KeyAction {
        let s = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history.push(&s);
        self.history_cursor = None;
        KeyAction::Submit(s)
    }
}

/// Slash autocomplete. Given a partial token (without the leading `/`)
/// return matching command summaries sorted by name.
pub fn autocomplete_slash(
    partial: &str,
    available: &[code_agent_core::SlashCommandSummary],
    limit: usize,
) -> Vec<String> {
    let mut hits: Vec<String> = available
        .iter()
        .map(|s| match &s.namespace {
            Some(ns) => format!("/{ns}:{}", s.name),
            None => format!("/{}", s.name),
        })
        .filter(|n| {
            n.trim_start_matches('/')
                .to_ascii_lowercase()
                .starts_with(&partial.to_ascii_lowercase())
        })
        .collect();
    hits.sort();
    hits.truncate(limit);
    hits
}

/// Filesystem autocomplete: given a partial path, return up to `limit`
/// candidate paths under `root`. Never reads file contents — only directory
/// entries — so it is safe to call repeatedly.
pub fn autocomplete_files(root: &Path, partial: &str, limit: usize) -> Vec<PathBuf> {
    if partial.contains("..") {
        return Vec::new();
    }
    // Resolve the parent dir from the partial; everything before the last
    // separator is fixed, and what follows is the prefix to match against.
    let (dir, prefix) = match partial.rfind('/') {
        Some(idx) => (root.join(&partial[..idx]), &partial[idx + 1..]),
        None => (root.to_path_buf(), partial),
    };
    let dir = match dir.canonicalize() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let canonical_root = match root.canonicalize() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    if !dir.starts_with(&canonical_root) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    for ent in read.flatten() {
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if !name_s.starts_with(prefix) {
            continue;
        }
        if name_s.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        if let Ok(rel) = ent.path().strip_prefix(&canonical_root) {
            out.push(rel.to_path_buf());
            if out.len() >= limit {
                break;
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::{SlashCommandSource, SlashCommandSummary};
    use tempfile::TempDir;

    fn cmd(name: &str) -> SlashCommandSummary {
        SlashCommandSummary {
            name: name.to_string(),
            namespace: None,
            description: String::new(),
            source: SlashCommandSource::BuiltIn,
            argument_hint: None,
        }
    }

    #[test]
    // Enter submits the buffer and pushes it onto history.
    fn enter_submits() {
        let mut s = ComposerState::new(ComposerConfig::default());
        for c in "hi".chars() {
            assert_eq!(s.handle_key(KeyEvent::Char(c)), KeyAction::None);
        }
        match s.handle_key(KeyEvent::Enter) {
            KeyAction::Submit(prompt) => assert_eq!(prompt, "hi"),
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.history.len(), 1);
    }

    #[test]
    // Shift+Enter inserts a newline when multiline is enabled.
    fn shift_enter_inserts_newline() {
        let mut s = ComposerState::new(ComposerConfig::default());
        s.handle_key(KeyEvent::Char('a'));
        s.handle_key(KeyEvent::ShiftEnter);
        s.handle_key(KeyEvent::Char('b'));
        assert_eq!(s.buffer, "a\nb");
    }

    #[test]
    // Ctrl+C with empty buffer signals interrupt; with text it clears.
    fn ctrl_c_interrupts_or_clears() {
        let mut s = ComposerState::new(ComposerConfig::default());
        assert_eq!(s.handle_key(KeyEvent::CtrlC), KeyAction::Interrupt);
        s.handle_key(KeyEvent::Char('x'));
        assert_eq!(s.handle_key(KeyEvent::CtrlC), KeyAction::None);
        assert_eq!(s.buffer, "");
    }

    #[test]
    // Ctrl+D exits when buffer empty.
    fn ctrl_d_exits_when_idle() {
        let mut s = ComposerState::new(ComposerConfig::default());
        assert_eq!(s.handle_key(KeyEvent::CtrlD), KeyAction::Exit);
    }

    #[test]
    // Up arrow recalls previous prompts.
    fn up_recalls_history() {
        let mut s = ComposerState::new(ComposerConfig::default());
        for c in "first".chars() {
            s.handle_key(KeyEvent::Char(c));
        }
        s.handle_key(KeyEvent::Enter);
        for c in "second".chars() {
            s.handle_key(KeyEvent::Char(c));
        }
        s.handle_key(KeyEvent::Enter);
        s.handle_key(KeyEvent::Up);
        assert_eq!(s.buffer, "second");
        s.handle_key(KeyEvent::Up);
        assert_eq!(s.buffer, "first");
    }

    #[test]
    // History rejects entries that look like secrets.
    fn history_rejects_secrets() {
        let mut h = PromptHistory::new(10);
        h.push("export OPENAI_API_KEY=sk-abc");
        h.push("ANTHROPIC_API_KEY=secret-token");
        h.push("normal prompt");
        assert_eq!(h.entries(), vec!["normal prompt".to_string()]);
    }

    #[test]
    // History caps at the configured limit (oldest evicted first).
    fn history_caps_at_limit() {
        let mut h = PromptHistory::new(2);
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.entries(), vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    // Slash autocomplete returns matching commands sorted, capped at limit.
    fn slash_autocomplete_filters() {
        let cmds = [cmd("help"), cmd("doctor"), cmd("config"), cmd("changes")];
        let hits = autocomplete_slash("c", &cmds, 10);
        assert_eq!(hits, vec!["/changes".to_string(), "/config".to_string()]);
    }

    #[test]
    // File autocomplete is sandboxed to the workspace root.
    fn file_autocomplete_respects_root() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("readme.md"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), b"x").unwrap();
        let hits = autocomplete_files(dir.path(), "r", 10);
        assert_eq!(hits, vec![PathBuf::from("readme.md")]);
        let nested = autocomplete_files(dir.path(), "src/l", 10);
        assert_eq!(nested, vec![PathBuf::from("src/lib.rs")]);
    }

    #[test]
    // Path traversal is refused.
    fn file_autocomplete_blocks_traversal() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("readme.md"), b"x").unwrap();
        let hits = autocomplete_files(dir.path(), "../etc", 10);
        assert!(hits.is_empty());
    }

    #[test]
    // Bracketed-paste data with embedded newlines is preserved verbatim.
    fn paste_inserts_each_char() {
        let mut s = ComposerState::new(ComposerConfig::default());
        for c in "ab\nc".chars() {
            // Simulating bracketed-paste: caller decodes Enter chars as
            // Char('\n') instead of Enter so they don't submit mid-paste.
            s.handle_key(KeyEvent::Char(c));
        }
        assert_eq!(s.buffer, "ab\nc");
    }
}
