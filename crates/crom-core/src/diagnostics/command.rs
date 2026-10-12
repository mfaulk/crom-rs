//! Shell command intent classification (M16F) and focused-validation
//! suggestions (M16G).
//!
//! This is a conservative, lexical classifier — it does not replace the
//! permission policy, it augments it. It answers "is this command a validation
//! command whose output we should parse for diagnostics?" and "is this a
//! mutating command that should set the validation-needed flag?".

use serde::{Deserialize, Serialize};

/// Coarse intent of a shell command. A command may carry several labels;
/// [`classify_command`] returns the full set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandIntent {
    Validation,
    FocusedValidation,
    Formatting,
    Inspection,
    Build,
    Test,
    Benchmark,
    Mutation,
    Destructive,
    Network,
    Unknown,
}

/// The classification of a single command: its set of intent labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandClass {
    pub intents: Vec<CommandIntent>,
}

impl CommandClass {
    pub fn has(&self, intent: CommandIntent) -> bool {
        self.intents.contains(&intent)
    }

    /// Whether this command's output is worth parsing for diagnostics.
    pub fn is_validation(&self) -> bool {
        self.has(CommandIntent::Validation) || self.has(CommandIntent::FocusedValidation)
    }

    /// Whether this command mutates the workspace (and should set the
    /// validation-needed flag).
    pub fn is_mutation(&self) -> bool {
        self.has(CommandIntent::Mutation) || self.has(CommandIntent::Destructive)
    }

    fn push(&mut self, intent: CommandIntent) {
        if !self.intents.contains(&intent) {
            self.intents.push(intent);
        }
    }
}

/// Split a command line into tokens, conservatively. We only look at the first
/// segment before a shell operator so that `cargo test && echo done` is
/// classified by its leading command.
fn lead_tokens(command: &str) -> Vec<String> {
    let trimmed = command.trim();
    // Cut at the first shell control operator.
    let mut end = trimmed.len();
    for op in ["&&", "||", ";", "|", ">", "<"] {
        if let Some(idx) = trimmed.find(op) {
            end = end.min(idx);
        }
    }
    trimmed[..end]
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

/// Classify a shell command into its set of intents.
pub fn classify_command(command: &str) -> CommandClass {
    let mut class = CommandClass::default();
    let tokens = lead_tokens(command);
    if tokens.is_empty() {
        class.push(CommandIntent::Unknown);
        return class;
    }
    let prog = tokens[0].as_str();
    // Args excluding flags, for subcommand detection.
    let args: Vec<&str> = tokens[1..].iter().map(|s| s.as_str()).collect();
    let non_flag: Vec<&str> = args
        .iter()
        .copied()
        .filter(|a| !a.starts_with('-'))
        .collect();

    match prog {
        "cargo" => classify_cargo(&non_flag, &args, &mut class),
        "rustc" => {
            class.push(CommandIntent::Validation);
            class.push(CommandIntent::Build);
        }
        "rm" => {
            class.push(CommandIntent::Destructive);
            class.push(CommandIntent::Mutation);
        }
        "mv" | "cp" | "mkdir" | "touch" | "chmod" | "sed" | "tee" => {
            class.push(CommandIntent::Mutation);
        }
        "ls" | "find" | "grep" | "rg" | "cat" | "head" | "tail" | "wc" | "tree" | "pwd"
        | "which" | "echo" | "stat" | "file" => {
            class.push(CommandIntent::Inspection);
        }
        "curl" | "wget" | "nc" | "ssh" | "scp" => {
            class.push(CommandIntent::Network);
        }
        "git" => classify_git(&non_flag, &args, &mut class),
        "make" => {
            class.push(CommandIntent::Build);
            class.push(CommandIntent::Validation);
        }
        _ => class.push(CommandIntent::Unknown),
    }

    if class.intents.is_empty() {
        class.push(CommandIntent::Unknown);
    }
    class
}

fn classify_cargo(non_flag: &[&str], args: &[&str], class: &mut CommandClass) {
    let sub = non_flag.first().copied().unwrap_or("");
    match sub {
        "test" | "nextest" => {
            class.push(CommandIntent::Test);
            class.push(CommandIntent::Validation);
            // A positional test filter (not a flag, not the subcommand) implies
            // a focused run, e.g. `cargo test parser::foo`.
            if non_flag.len() > 1 || args.iter().any(|a| a.contains("--test")) {
                class.push(CommandIntent::FocusedValidation);
            }
        }
        "check" | "build" | "b" | "c" => {
            class.push(CommandIntent::Build);
            class.push(CommandIntent::Validation);
        }
        "clippy" => {
            class.push(CommandIntent::Validation);
        }
        "fmt" => {
            // `cargo fmt --check` validates; bare `cargo fmt` mutates.
            if args.iter().any(|a| *a == "--check" || *a == "--check=true") {
                class.push(CommandIntent::Validation);
                class.push(CommandIntent::Formatting);
            } else {
                class.push(CommandIntent::Formatting);
                class.push(CommandIntent::Mutation);
            }
        }
        "bench" => {
            class.push(CommandIntent::Benchmark);
            class.push(CommandIntent::Validation);
        }
        "doc" => {
            class.push(CommandIntent::Build);
        }
        "run" | "r" => {
            // Ambiguous: could be exercising behavior (test-like) or just
            // running. Treat conservatively as Unknown + Build.
            class.push(CommandIntent::Build);
            class.push(CommandIntent::Unknown);
        }
        "add" | "rm" | "update" => {
            class.push(CommandIntent::Mutation);
        }
        _ => class.push(CommandIntent::Unknown),
    }
}

fn classify_git(non_flag: &[&str], args: &[&str], class: &mut CommandClass) {
    let sub = non_flag.first().copied().unwrap_or("");
    match sub {
        "diff" | "status" | "log" | "show" | "blame" | "branch" => {
            class.push(CommandIntent::Inspection);
        }
        "checkout" | "reset" | "clean" | "restore" => {
            class.push(CommandIntent::Mutation);
            if args
                .iter()
                .any(|a| *a == "--hard" || *a == "-fd" || *a == "-f")
            {
                class.push(CommandIntent::Destructive);
            }
        }
        "commit" | "add" | "stash" | "merge" | "rebase" | "apply" => {
            class.push(CommandIntent::Mutation);
        }
        "push" | "pull" | "fetch" | "clone" => {
            class.push(CommandIntent::Network);
        }
        _ => class.push(CommandIntent::Inspection),
    }
}

// --- Focused validation (M16G) --------------------------------------------

use super::model::{CommandDiagnostic, DiagnosticKind};

/// A suggested validation plan: the broad command that failed plus narrower
/// commands that target the specific failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationPlan {
    pub broad_command: String,
    pub focused_commands: Vec<String>,
    pub reason: String,
}

/// Derive a focused-validation plan from a failed validation command's
/// diagnostic. Best-effort: package inference is shallow and may be omitted.
pub fn focused_validation_plan(diag: &CommandDiagnostic) -> Option<ValidationPlan> {
    let broad = diag.command.clone();
    let mut focused = Vec::new();
    let mut reason = String::new();

    // Failing unit tests → `cargo test <name>` for each (capped).
    if !diag.summary.failing_tests.is_empty() {
        for ft in diag.summary.failing_tests.iter().take(3) {
            // Integration test files surface as `tests/<file>.rs`; prefer
            // `--test <file>` when the test path looks file-scoped.
            if let Some(file) = &ft.file {
                if let Some(test_name) = integration_test_name(file) {
                    let cmd = format!("cargo test --test {test_name}");
                    if !focused.contains(&cmd) {
                        focused.push(cmd);
                    }
                    continue;
                }
            }
            focused.push(format!("cargo test {}", ft.name));
        }
        reason = "Re-run only the failing test(s) to iterate faster.".to_string();
    }

    match diag.summary.primary_kind {
        DiagnosticKind::Formatting => {
            focused.push("cargo fmt --check".to_string());
            if reason.is_empty() {
                reason = "Formatting failure — check formatting only.".to_string();
            }
        }
        DiagnosticKind::ClippyLint => {
            focused.push("cargo clippy -- -D warnings".to_string());
            if reason.is_empty() {
                reason = "Lint failure — re-run clippy.".to_string();
            }
        }
        // Compile error: a focused `cargo check` is still narrower than a full
        // test run.
        _ if focused.is_empty() && diag.command.contains("test") => {
            focused.push("cargo check".to_string());
            reason = "Compile error — check compilation before re-running tests.".to_string();
        }
        _ => {}
    }

    focused.dedup();
    if focused.is_empty() {
        return None;
    }
    Some(ValidationPlan {
        broad_command: broad,
        focused_commands: focused,
        reason,
    })
}

/// If a path looks like an integration test, return the cargo `--test <name>`
/// target. A cargo integration test target is named by the entry directly under
/// `tests/`: `tests/api.rs` → `api`, and `tests/suite/main.rs` → `suite` (the
/// directory is the target, not its `main.rs`/`mod.rs`).
fn integration_test_name(path: &std::path::Path) -> Option<String> {
    use std::path::Component;
    let comps: Vec<Component> = path.components().collect();
    let tests_idx = comps.iter().position(|c| c.as_os_str() == "tests")?;
    let entry = comps.get(tests_idx + 1)?;
    // The target name is the file stem of a direct file child, or the directory
    // name when the test is `tests/<dir>/...`.
    let stem = std::path::Path::new(entry.as_os_str())
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())?;
    Some(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intents(cmd: &str) -> Vec<CommandIntent> {
        classify_command(cmd).intents
    }

    #[test]
    fn cargo_test_is_validation_test() {
        let i = intents("cargo test");
        assert!(i.contains(&CommandIntent::Validation));
        assert!(i.contains(&CommandIntent::Test));
        assert!(!i.contains(&CommandIntent::FocusedValidation));
    }

    #[test]
    fn cargo_test_filter_is_focused() {
        let i = intents("cargo test parser::foo");
        assert!(i.contains(&CommandIntent::FocusedValidation));
        assert!(i.contains(&CommandIntent::Test));
    }

    #[test]
    fn cargo_check_is_validation_build() {
        let i = intents("cargo check");
        assert!(i.contains(&CommandIntent::Validation));
        assert!(i.contains(&CommandIntent::Build));
    }

    #[test]
    fn cargo_fmt_check_vs_mutate() {
        assert!(intents("cargo fmt --check").contains(&CommandIntent::Validation));
        assert!(intents("cargo fmt --check").contains(&CommandIntent::Formatting));
        let m = intents("cargo fmt");
        assert!(m.contains(&CommandIntent::Mutation));
        assert!(m.contains(&CommandIntent::Formatting));
        assert!(!m.contains(&CommandIntent::Validation));
    }

    #[test]
    fn cargo_clippy_is_validation() {
        assert!(intents("cargo clippy -- -D warnings").contains(&CommandIntent::Validation));
    }

    #[test]
    fn git_diff_is_inspection() {
        assert_eq!(intents("git diff"), vec![CommandIntent::Inspection]);
    }

    #[test]
    fn rm_rf_is_destructive() {
        let i = intents("rm -rf target");
        assert!(i.contains(&CommandIntent::Destructive));
        assert!(i.contains(&CommandIntent::Mutation));
    }

    #[test]
    fn curl_is_network() {
        assert_eq!(
            intents("curl https://example.com"),
            vec![CommandIntent::Network]
        );
    }

    #[test]
    fn unknown_pipeline_is_conservative() {
        let i = intents("frobnicate --wibble | xargs zap");
        assert!(i.contains(&CommandIntent::Unknown));
    }

    #[test]
    fn git_checkout_hard_is_destructive() {
        let i = intents("git reset --hard HEAD");
        assert!(i.contains(&CommandIntent::Destructive));
    }

    #[test]
    fn integration_test_name_targets() {
        use std::path::Path;
        assert_eq!(
            integration_test_name(Path::new("tests/api.rs")).as_deref(),
            Some("api")
        );
        // The target is the directory under tests/, not its main.rs.
        assert_eq!(
            integration_test_name(Path::new("tests/suite/main.rs")).as_deref(),
            Some("suite")
        );
        assert_eq!(integration_test_name(Path::new("src/lib.rs")), None);
    }
}
