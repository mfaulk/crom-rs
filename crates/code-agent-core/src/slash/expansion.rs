//! Slash-command body expansion.
//!
//! Custom command bodies can contain three kinds of placeholders:
//! - `$1`, `$2`, ... `$9` — positional arguments.
//! - `$ARGUMENTS` — the full raw argument string.
//! - `@path/to/file` — a file reference (uses Read semantics through
//!   the supplied [`SlashExpansionExecutor`]).
//! - `` !`shell command` `` — a Bash expansion (uses Bash semantics through
//!   the executor).
//!
//! The engine **does not** execute Bash or read files itself — it routes
//! every operation through the executor so the existing tool/permission
//! pipeline stays the single source of truth on what's allowed.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::custom::CustomSlashCommandDefinition;
use super::errors::SlashCommandError;
use super::parser::ParsedSlashCommand;
use crate::skills::{SkillDefinition, SkillToolResult};

/// Knobs that gate which expansion forms are allowed.
#[derive(Debug, Clone)]
pub struct SlashExpansionOptions {
    pub allow_bash_expansion: bool,
    pub allow_file_references: bool,
    pub max_expanded_prompt_bytes: usize,
    pub max_expansion_output_bytes: usize,
}

impl Default for SlashExpansionOptions {
    fn default() -> Self {
        Self {
            allow_bash_expansion: true,
            allow_file_references: true,
            max_expanded_prompt_bytes: 131_072,
            max_expansion_output_bytes: 65_536,
        }
    }
}

/// Compact summary of a tool-call performed during expansion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandedToolCallSummary {
    pub kind: String,
    pub target: String,
    pub size_bytes: usize,
    pub truncated: bool,
}

/// Result of expanding a custom command body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashExpansionResult {
    pub command: String,
    pub expanded_prompt: String,
    pub file_refs: Vec<String>,
    pub bash_commands: Vec<String>,
    pub tool_calls: Vec<ExpandedToolCallSummary>,
    pub truncated: bool,
}

/// Trait used by the expansion engine to perform safe file reads and
/// Bash invocations. Callers wire this to the existing tool/permission
/// pipeline (CLI uses the built-in Read/Bash tools through their normal
/// `Tool::execute` path).
#[async_trait]
pub trait SlashExpansionExecutor: Send + Sync {
    /// Read a workspace-relative file. Implementations must apply path
    /// sandboxing and size caps (Read tool semantics).
    async fn read_file(&self, path: &str) -> Result<String, SlashCommandError>;
    /// Execute a Bash command. Implementations must apply the normal
    /// approval, timeout, and output-cap pipeline.
    async fn run_bash(&self, command: &str) -> Result<String, SlashCommandError>;
}

/// Build a synthetic user prompt for an explicit `/skill-name args` invocation.
/// Used by the AgentClient's slash dispatcher when the user invokes a Skill
/// without going through the model's tool-use channel.
pub fn render_synthetic_skill_prompt(
    parsed: &ParsedSlashCommand,
    skill: &SkillDefinition,
    loaded: &SkillToolResult,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "The user invoked the /{} skill command.\n\n",
        skill.name
    ));
    if !parsed.raw_arguments.is_empty() {
        out.push_str("Arguments:\n");
        out.push_str(&parsed.raw_arguments);
        out.push_str("\n\n");
    }
    out.push_str("Skill instructions:\n");
    out.push_str(&loaded.instructions);
    out.push_str("\n\n");
    if !loaded.resources.is_empty() {
        out.push_str("Available resources (loaded files):\n");
        for r in &loaded.resources {
            out.push_str(&format!(
                "- {} ({:?}, {} bytes, loaded={})\n",
                r.path, r.kind, r.size_bytes, r.loaded
            ));
        }
        out.push('\n');
    }
    out.push_str("Please proceed with the skill's procedure.\n");
    out
}

/// Expand a custom slash command body. Returns the expanded prompt and
/// summaries of all expansions performed.
pub async fn expand_custom_command(
    parsed: &ParsedSlashCommand,
    def: &CustomSlashCommandDefinition,
    options: &SlashExpansionOptions,
    executor: &dyn SlashExpansionExecutor,
) -> Result<SlashExpansionResult, SlashCommandError> {
    let body = &def.body;
    // Substitute `$1`, `$2`, ... `$ARGUMENTS` *with shell-quoting inside
    // bash blocks*. A textual substitution that puts user-controlled
    // arguments directly into a `!`...`` block would let a value like
    // `main; rm -rf ~` execute arbitrary commands. Quoting at the
    // substitution site keeps each positional confined to a single shell
    // word regardless of what characters it contains.
    let substituted = substitute_with_bash_aware_quoting(body, parsed)?;

    // Two passes: bash expansion first into placeholders, then file
    // references, then placeholder substitution. The placeholder
    // indirection ensures bash OUTPUT is never rescanned for
    // @-tokens — that was the #167 regression: an
    // `expand_file_refs(&after_bash, ...)` walked the post-bash text
    // and expanded any `@path` token the bash command happened to
    // print, including malicious or unintended ones.
    let mut summaries: Vec<ExpandedToolCallSummary> = Vec::new();
    let mut bash_commands = Vec::new();
    let mut file_refs = Vec::new();
    let mut truncated = false;

    let (with_placeholders, bash_outputs) = expand_bash_blocks_to_placeholders(
        &substituted,
        options,
        executor,
        &mut summaries,
        &mut bash_commands,
        &mut truncated,
    )
    .await?;

    let after_files = expand_file_refs(
        &with_placeholders,
        options,
        executor,
        &mut summaries,
        &mut file_refs,
        &mut truncated,
    )
    .await?;

    // Substitute bash placeholders with the real outputs AFTER the
    // file-ref pass has finished, so the output is not re-scanned.
    let after_files = substitute_bash_placeholders(after_files, &bash_outputs);

    if after_files.len() > options.max_expanded_prompt_bytes {
        return Err(SlashCommandError::PromptTooLarge {
            max: options.max_expanded_prompt_bytes,
        });
    }

    Ok(SlashExpansionResult {
        command: parsed.full_name(),
        expanded_prompt: after_files,
        file_refs,
        bash_commands,
        tool_calls: summaries,
        truncated,
    })
}

/// Walk the body, substituting `$1`, `$2`, ..., `$ARGUMENTS`. Substitutions
/// inside a `!`...`` bash block are *shell-quoted* so an argument containing
/// shell metacharacters cannot escape its lexical position and run an
/// attacker-controlled command. Substitutions outside bash blocks are
/// inserted verbatim.
fn substitute_with_bash_aware_quoting(
    body: &str,
    parsed: &ParsedSlashCommand,
) -> Result<String, SlashCommandError> {
    let mut out = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut in_bash = false;
    while i < bytes.len() {
        let c = bytes[i] as char;

        // Track entry/exit of `!`...`` regions. We only flip to `in_bash`
        // when we see the opening `!\``; the closing `\`` flips us back.
        if !in_bash && c == '!' && i + 1 < bytes.len() && bytes[i + 1] as char == '`' {
            out.push(c);
            out.push('`');
            i += 2;
            in_bash = true;
            continue;
        }
        if in_bash && c == '`' {
            out.push(c);
            i += 1;
            in_bash = false;
            continue;
        }

        // Positional: $1..$9.
        if c == '$' && i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_digit() {
            let digit = (bytes[i + 1] as char).to_digit(10).unwrap_or(0) as usize;
            if digit == 0 {
                out.push('$');
                out.push('0');
                i += 2;
                continue;
            }
            let idx = digit - 1;
            let arg = match parsed.arguments.get(idx) {
                Some(a) => a.as_str(),
                None => {
                    return Err(SlashCommandError::MissingPositional {
                        name: parsed.full_name(),
                        index: digit,
                    });
                }
            };
            if in_bash {
                out.push_str(&shell_single_quote(arg));
            } else {
                out.push_str(arg);
            }
            i += 2;
            continue;
        }

        // $ARGUMENTS — full raw argument string. Require a word boundary
        // after the token so longer identifiers like `$ARGUMENTSET` or
        // `$ARGUMENTS_LIST` are not mangled (#214).
        if c == '$' && body[i..].starts_with("$ARGUMENTS") {
            let end = i + "$ARGUMENTS".len();
            let bounded = match bytes.get(end) {
                None => true,
                Some(&b) => !(b as char).is_ascii_alphanumeric() && b != b'_',
            };
            if bounded {
                let raw = parsed.raw_arguments.as_str();
                if in_bash {
                    out.push_str(&shell_single_quote(raw));
                } else {
                    out.push_str(raw);
                }
                i = end;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
    Ok(out)
}

/// POSIX-safe single-quote escape: `it's a "trick"` becomes
/// `'it'\''s a "trick"'`. Single quotes disable every shell metacharacter
/// except `'` itself, which we close, escape, and reopen.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Sentinel used to mark the position of a bash-block output during the
/// two-pass expansion. `\x00` cannot appear in normal text input
/// (Rust `&str` UTF-8 still permits it, but real prompts and command
/// definitions don't include NULs); the `BASH_<N>_END` framing makes
/// it impossible to mistake one placeholder for another or for any
/// `@path` token the file-ref pass would otherwise match.
const BASH_PLACEHOLDER_PREFIX: &str = "\x00BASH_";
const BASH_PLACEHOLDER_SUFFIX: &str = "_END\x00";

fn bash_placeholder(idx: usize) -> String {
    format!("{BASH_PLACEHOLDER_PREFIX}{idx}{BASH_PLACEHOLDER_SUFFIX}")
}

/// Like [`expand_bash_blocks`], but instead of inlining each bash
/// block's output it writes a sentinel placeholder into the returned
/// string and returns the outputs in a parallel vector. The caller is
/// expected to substitute the placeholders back AFTER the file-ref
/// pass has run, so the bash output is never rescanned for @-tokens
/// (#167).
async fn expand_bash_blocks_to_placeholders(
    body: &str,
    options: &SlashExpansionOptions,
    executor: &dyn SlashExpansionExecutor,
    summaries: &mut Vec<ExpandedToolCallSummary>,
    bash_commands: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<(String, Vec<String>), SlashCommandError> {
    let mut out = String::new();
    let mut outputs: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '!' && i + 1 < bytes.len() && bytes[i + 1] as char == '`' {
            let start = i + 2;
            let rel_end = match body[start..].find('`') {
                Some(p) => p,
                None => {
                    out.push('!');
                    i += 1;
                    continue;
                }
            };
            let cmd = &body[start..start + rel_end];
            if !options.allow_bash_expansion {
                return Err(SlashCommandError::ExpansionDisabled {
                    token: format!("!`{cmd}`"),
                });
            }
            let result = executor.run_bash(cmd).await?;
            let mut output = result;
            if output.len() > options.max_expansion_output_bytes {
                crate::compaction::truncate_utf8_in_place(
                    &mut output,
                    options.max_expansion_output_bytes,
                );
                output.push_str("\n...(truncated)");
                *truncated = true;
            }
            summaries.push(ExpandedToolCallSummary {
                kind: "bash".to_string(),
                target: cmd.to_string(),
                size_bytes: output.len(),
                truncated: *truncated,
            });
            bash_commands.push(cmd.to_string());
            out.push_str(&bash_placeholder(outputs.len()));
            outputs.push(output);
            i = start + rel_end + 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok((out, outputs))
}

/// Replace every `\x00BASH_<N>_END\x00` placeholder in `body` with
/// `outputs[N]`. Indices that fall outside the outputs vector are
/// left as-is (defensive; we control both halves but a defensive
/// no-op is safer than a panic).
fn substitute_bash_placeholders(body: String, outputs: &[String]) -> String {
    if outputs.is_empty() {
        return body;
    }
    let mut out = String::with_capacity(body.len());
    let mut rest = body.as_str();
    while let Some(p) = rest.find(BASH_PLACEHOLDER_PREFIX) {
        out.push_str(&rest[..p]);
        let after_prefix = &rest[p + BASH_PLACEHOLDER_PREFIX.len()..];
        if let Some(end) = after_prefix.find(BASH_PLACEHOLDER_SUFFIX) {
            let idx_str = &after_prefix[..end];
            match idx_str.parse::<usize>() {
                Ok(idx) if idx < outputs.len() => {
                    out.push_str(&outputs[idx]);
                    rest = &after_prefix[end + BASH_PLACEHOLDER_SUFFIX.len()..];
                    continue;
                }
                _ => {
                    // Defensive: emit the literal prefix and skip past
                    // it so we don't loop forever on a malformed input.
                    out.push_str(BASH_PLACEHOLDER_PREFIX);
                    rest = after_prefix;
                    continue;
                }
            }
        } else {
            // No suffix: emit the rest verbatim.
            out.push_str(BASH_PLACEHOLDER_PREFIX);
            rest = after_prefix;
        }
    }
    out.push_str(rest);
    out
}

#[allow(dead_code)] // retained for downstream callers that may still
                    // want the legacy single-pass behavior; the
                    // expansion pipeline uses the placeholder variant
                    // above.
async fn expand_bash_blocks(
    body: &str,
    options: &SlashExpansionOptions,
    executor: &dyn SlashExpansionExecutor,
    summaries: &mut Vec<ExpandedToolCallSummary>,
    bash_commands: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<String, SlashCommandError> {
    let mut out = String::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '!' && i + 1 < bytes.len() && bytes[i + 1] as char == '`' {
            // Find the closing backtick.
            let start = i + 2;
            let rel_end = match body[start..].find('`') {
                Some(p) => p,
                None => {
                    out.push('!');
                    i += 1;
                    continue;
                }
            };
            let cmd = &body[start..start + rel_end];
            if !options.allow_bash_expansion {
                return Err(SlashCommandError::ExpansionDisabled {
                    token: format!("!`{cmd}`"),
                });
            }
            let result = executor.run_bash(cmd).await?;
            let mut output = result;
            if output.len() > options.max_expansion_output_bytes {
                crate::compaction::truncate_utf8_in_place(
                    &mut output,
                    options.max_expansion_output_bytes,
                );
                output.push_str("\n...(truncated)");
                *truncated = true;
            }
            summaries.push(ExpandedToolCallSummary {
                kind: "bash".to_string(),
                target: cmd.to_string(),
                size_bytes: output.len(),
                truncated: *truncated,
            });
            bash_commands.push(cmd.to_string());
            out.push_str(&output);
            i = start + rel_end + 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(out)
}

async fn expand_file_refs(
    body: &str,
    options: &SlashExpansionOptions,
    executor: &dyn SlashExpansionExecutor,
    summaries: &mut Vec<ExpandedToolCallSummary>,
    file_refs: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<String, SlashCommandError> {
    let mut out = String::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '@' && (i == 0 || !is_word_char(bytes[i - 1] as char)) && i + 1 < bytes.len() {
            // Take consecutive non-whitespace, non-special chars as the path.
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() {
                let ch = bytes[end] as char;
                if ch.is_whitespace() || matches!(ch, '`' | ')' | ']' | '}' | ',' | ';') {
                    break;
                }
                end += 1;
            }
            let path = &body[start..end];
            if path.is_empty() {
                out.push('@');
                i += 1;
                continue;
            }
            if !options.allow_file_references {
                return Err(SlashCommandError::ExpansionDisabled {
                    token: format!("@{path}"),
                });
            }
            let read = executor.read_file(path).await?;
            let mut content = read;
            if content.len() > options.max_expansion_output_bytes {
                crate::compaction::truncate_utf8_in_place(
                    &mut content,
                    options.max_expansion_output_bytes,
                );
                content.push_str("\n...(truncated)");
                *truncated = true;
            }
            summaries.push(ExpandedToolCallSummary {
                kind: "file".to_string(),
                target: path.to_string(),
                size_bytes: content.len(),
                truncated: *truncated,
            });
            file_refs.push(path.to_string());
            out.push_str(&format!("\n--- file: {path} ---\n"));
            out.push_str(&content);
            out.push_str("\n--- end ---\n");
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(out)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::custom::CustomSlashCommandDefinition;
    use crate::slash::parser::parse_slash_command;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct FakeExecutor {
        files: std::collections::HashMap<String, String>,
        bash: std::collections::HashMap<String, String>,
        invocations: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SlashExpansionExecutor for FakeExecutor {
        async fn read_file(&self, path: &str) -> Result<String, SlashCommandError> {
            self.invocations
                .lock()
                .unwrap()
                .push(format!("read:{path}"));
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| SlashCommandError::ExpansionFailed(format!("no such file: {path}")))
        }
        async fn run_bash(&self, command: &str) -> Result<String, SlashCommandError> {
            self.invocations
                .lock()
                .unwrap()
                .push(format!("bash:{command}"));
            self.bash
                .get(command)
                .cloned()
                .ok_or_else(|| SlashCommandError::ExpansionFailed(format!("no fake for {command}")))
        }
    }

    fn cmd(body: &str) -> CustomSlashCommandDefinition {
        CustomSlashCommandDefinition {
            name: "review".to_string(),
            namespace: None,
            description: "d".to_string(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            model: None,
            body: body.to_string(),
            source_path: PathBuf::from("/tmp/review.md"),
        }
    }

    #[tokio::test]
    // Positional + $ARGUMENTS substitution works.
    async fn substitutes_positionals_and_arguments() {
        let parsed = parse_slash_command("/review main file.rs").unwrap();
        let exec = FakeExecutor {
            files: Default::default(),
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("base=$1; all=$ARGUMENTS"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        assert!(r.expanded_prompt.contains("base=main"));
        assert!(r.expanded_prompt.contains("all=main file.rs"));
    }

    #[tokio::test]
    // $ARGUMENTS substitution respects word boundaries: longer identifiers
    // such as `$ARGUMENTSET` are left intact (#214).
    async fn arguments_substitution_respects_word_boundary() {
        let parsed = parse_slash_command("/review foo").unwrap();
        let exec = FakeExecutor {
            files: Default::default(),
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("set=$ARGUMENTSET list=$ARGUMENTS_LIST all=$ARGUMENTS end=$ARGUMENTS"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        // Longer identifiers untouched.
        assert!(r.expanded_prompt.contains("set=$ARGUMENTSET"));
        assert!(r.expanded_prompt.contains("list=$ARGUMENTS_LIST"));
        // Bare $ARGUMENTS (followed by space or end) expands.
        assert!(r.expanded_prompt.contains("all=foo end=foo"));
    }

    #[tokio::test]
    // Missing positional surfaces a clear error.
    async fn missing_positional_errors() {
        let parsed = parse_slash_command("/review").unwrap();
        let exec = FakeExecutor {
            files: Default::default(),
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let err = expand_custom_command(
            &parsed,
            &cmd("$1"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SlashCommandError::MissingPositional { .. }));
    }

    #[tokio::test]
    // Bash expansion goes through the executor.
    async fn bash_expansion_routes_through_executor() {
        let parsed = parse_slash_command("/review").unwrap();
        let mut bash = std::collections::HashMap::new();
        bash.insert("git status".to_string(), "On branch main\n".to_string());
        let exec = FakeExecutor {
            files: Default::default(),
            bash,
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("Status:\n!`git status`"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        assert!(r.expanded_prompt.contains("On branch main"));
        assert_eq!(r.bash_commands, vec!["git status"]);
    }

    #[tokio::test]
    // File-ref expansion routes through the executor and is wrapped in
    // boundaries so the model can tell where the content ends.
    async fn file_ref_expansion_routes_through_executor() {
        let parsed = parse_slash_command("/review").unwrap();
        let mut files = std::collections::HashMap::new();
        files.insert("CHANGELOG.md".to_string(), "## v0.4.0\n".to_string());
        let exec = FakeExecutor {
            files,
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("see @CHANGELOG.md"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        assert!(r.expanded_prompt.contains("v0.4.0"));
        assert_eq!(r.file_refs, vec!["CHANGELOG.md"]);
    }

    #[tokio::test]
    // Bash expansion is rejected when the option is disabled.
    async fn bash_expansion_disabled() {
        let parsed = parse_slash_command("/review").unwrap();
        let exec = FakeExecutor {
            files: Default::default(),
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let opts = SlashExpansionOptions {
            allow_bash_expansion: false,
            ..SlashExpansionOptions::default()
        };
        let err = expand_custom_command(&parsed, &cmd("!`ls`"), &opts, &exec)
            .await
            .unwrap_err();
        assert!(matches!(err, SlashCommandError::ExpansionDisabled { .. }));
    }

    #[tokio::test]
    // File expansion is rejected when the option is disabled.
    async fn file_expansion_disabled() {
        let parsed = parse_slash_command("/review").unwrap();
        let exec = FakeExecutor {
            files: Default::default(),
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let opts = SlashExpansionOptions {
            allow_file_references: false,
            ..SlashExpansionOptions::default()
        };
        let err = expand_custom_command(&parsed, &cmd("@README.md"), &opts, &exec)
            .await
            .unwrap_err();
        assert!(matches!(err, SlashCommandError::ExpansionDisabled { .. }));
    }

    #[tokio::test]
    // Output is capped if it exceeds the per-expansion budget.
    async fn output_truncation_marked() {
        let parsed = parse_slash_command("/review").unwrap();
        let mut bash = std::collections::HashMap::new();
        bash.insert("big".to_string(), "x".repeat(2000));
        let exec = FakeExecutor {
            files: Default::default(),
            bash,
            invocations: Mutex::new(Vec::new()),
        };
        let opts = SlashExpansionOptions {
            max_expansion_output_bytes: 100,
            ..SlashExpansionOptions::default()
        };
        let r = expand_custom_command(&parsed, &cmd("!`big`"), &opts, &exec)
            .await
            .unwrap();
        assert!(r.truncated);
        assert!(r.expanded_prompt.contains("(truncated)"));
    }

    #[tokio::test]
    // The whole expanded prompt has a hard size cap.
    async fn prompt_size_cap_enforced() {
        let parsed = parse_slash_command("/review").unwrap();
        let exec = FakeExecutor {
            files: Default::default(),
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let body = "x".repeat(2_000);
        let opts = SlashExpansionOptions {
            max_expanded_prompt_bytes: 100,
            ..SlashExpansionOptions::default()
        };
        let err = expand_custom_command(&parsed, &cmd(&body), &opts, &exec)
            .await
            .unwrap_err();
        assert!(matches!(err, SlashCommandError::PromptTooLarge { .. }));
    }

    #[tokio::test]
    // Positional arguments containing shell metacharacters cannot escape a
    // bash block: the substituted text is single-quoted, so an injection
    // attempt is sent to bash as a literal argument.
    async fn bash_substitution_quotes_positionals() {
        let parsed = parse_slash_command("/review").unwrap();
        // The user supplied an argument that, naively interpolated, would
        // be `git diff main; rm -rf ~`. After quoting it becomes
        // `git diff 'main; rm -rf ~'`, which bash treats as a single arg.
        let parsed = ParsedSlashCommand {
            arguments: vec!["main; rm -rf ~".to_string()],
            raw_arguments: "main; rm -rf ~".to_string(),
            ..parsed
        };
        let mut bash = std::collections::HashMap::new();
        // The fake executor matches on the literal command we expect bash
        // to receive after quoting, so it returns content only when
        // quoting is correctly applied.
        bash.insert(
            "git diff 'main; rm -rf ~'".to_string(),
            "diff body".to_string(),
        );
        let exec = FakeExecutor {
            files: Default::default(),
            bash,
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("!`git diff $1`"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        assert_eq!(r.bash_commands, vec!["git diff 'main; rm -rf ~'"]);
        assert!(r.expanded_prompt.contains("diff body"));
    }

    #[tokio::test]
    // Substitutions outside a bash block remain raw (so prose like
    // "review changes since v0.4.0" reads naturally) — only inside
    // bash do we escape.
    async fn substitution_is_raw_outside_bash() {
        let parsed = parse_slash_command("/note").unwrap();
        let parsed = ParsedSlashCommand {
            arguments: vec!["v0.4.0".to_string()],
            raw_arguments: "v0.4.0".to_string(),
            ..parsed
        };
        let exec = FakeExecutor {
            files: Default::default(),
            bash: Default::default(),
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("Tag the release as $1"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        assert!(r.expanded_prompt.contains("Tag the release as v0.4.0"));
    }

    #[test]
    // The single-quote escape handles single-quote-containing inputs
    // without breaking the quoted span.
    fn shell_quote_handles_inner_quote() {
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_single_quote(""), "''");
        assert_eq!(shell_single_quote("plain"), "'plain'");
    }

    // Regression for #167: bash output is NOT re-scanned for
    // @-tokens. A bash command whose stdout contains `@secret.txt`
    // must NOT trigger an implicit file read of `secret.txt`. Pre-fix
    // `expand_file_refs(&after_bash, ...)` did exactly that, turning
    // any bash printout into an arbitrary-file-read primitive.
    #[tokio::test]
    async fn bash_output_at_tokens_are_not_expanded() {
        let parsed = parse_slash_command("/review").unwrap();
        let mut bash = std::collections::HashMap::new();
        // Bash command emits a line that LOOKS like a file ref.
        bash.insert("git status".to_string(), "see also @secret.txt".to_string());
        let mut files = std::collections::HashMap::new();
        // If file expansion ever fires the secret content would be
        // inlined; we'd see it in the expanded prompt.
        files.insert("secret.txt".to_string(), "PASSWORD=hunter2".to_string());
        let exec = FakeExecutor {
            files,
            bash,
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("Status:\n!`git status`"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        // Bash output is present verbatim.
        assert!(r.expanded_prompt.contains("see also @secret.txt"));
        // The secret file content is NOT inlined.
        assert!(
            !r.expanded_prompt.contains("PASSWORD=hunter2"),
            "bash output must not trigger implicit @-token expansion (#167)"
        );
        // No file-ref invocation was recorded.
        assert!(
            !exec
                .invocations
                .lock()
                .unwrap()
                .iter()
                .any(|i| i.starts_with("read:")),
            "no file should be read from bash output: {:?}",
            exec.invocations.lock().unwrap()
        );
    }

    // Original @-tokens in the command body still expand normally.
    // The fix narrows what gets rescanned; it does not disable
    // file-ref expansion in the user's prompt.
    #[tokio::test]
    async fn original_at_tokens_still_expand_when_bash_present() {
        let parsed = parse_slash_command("/review").unwrap();
        let mut bash = std::collections::HashMap::new();
        bash.insert("date".to_string(), "Mon Jan 1 2026".to_string());
        let mut files = std::collections::HashMap::new();
        files.insert("README.md".to_string(), "# project\n".to_string());
        let exec = FakeExecutor {
            files,
            bash,
            invocations: Mutex::new(Vec::new()),
        };
        let r = expand_custom_command(
            &parsed,
            &cmd("date=!`date`, readme=@README.md"),
            &SlashExpansionOptions::default(),
            &exec,
        )
        .await
        .unwrap();
        assert!(r.expanded_prompt.contains("Mon Jan 1 2026"));
        assert!(r.expanded_prompt.contains("# project"));
    }
}
