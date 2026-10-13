use serde::{Deserialize, Serialize};

/// Policy for how Bash interacts with checkpointing. None of these block
/// "safe" commands like `cargo test` from running — they govern whether
/// the agent must surface the limitation that Bash filesystem changes are
/// not captured by checkpointing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BashCheckpointPolicy {
    /// Always allow Bash, but emit a one-shot warning to the approval
    /// stream and the session log.
    AllowWithWarning,
    /// Default. Bash must be explicitly approved when checkpointing is
    /// enabled, even if a broad allow rule would otherwise allow it,
    /// unless the exact command is approved by a specific rule.
    #[default]
    RequireExplicitApproval,
    /// Deny Bash invocations whose command is classified as likely to
    /// mutate the workspace (sed -i, mv, rm, cargo fmt, prettier --write,
    /// shell redirects, etc.).
    DenyLikelyMutatingCommands,
}

impl BashCheckpointPolicy {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "allow_with_warning" | "allow-with-warning" => Some(Self::AllowWithWarning),
            "require_explicit_approval" | "require-explicit-approval" => {
                Some(Self::RequireExplicitApproval)
            }
            "deny_likely_mutating_commands"
            | "deny-likely-mutating-commands"
            | "deny_likely_mutating" => Some(Self::DenyLikelyMutatingCommands),
            _ => None,
        }
    }
}

/// Coarse classification of a Bash command for checkpoint-aware UX.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BashCommandClass {
    /// Command is unlikely to modify workspace files (e.g. `cargo test`).
    Inert,
    /// Command is highly likely to modify workspace files.
    LikelyMutating,
    /// We can't say with confidence; treat as potentially mutating.
    Unknown,
}

/// Heuristic classifier. The matching is intentionally conservative — we
/// favor false positives over false negatives, because the contract is
/// "Bash is not checkpointed; warn or refuse."
pub fn classify_bash_command(cmd: &str) -> BashCommandClass {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return BashCommandClass::Inert;
    }

    let lc = trimmed.to_ascii_lowercase();

    // Output redirection: `cmd > file`, `cmd >> file`, `cat <<EOF > file`.
    // We catch `>` even when followed by digits (file descriptor redirect)
    // because the more permissive match also covers `> file` writes; the
    // false-positive cost (warning on a stderr-only redirect) is tiny.
    // Skip plain `2>&1` style: those are FD-to-FD, not file writes.
    if has_filesystem_redirect(trimmed) {
        return BashCommandClass::LikelyMutating;
    }

    // Pipe-to-tee writes a file.
    if lc.contains("| tee ") || lc.contains("| tee\t") {
        return BashCommandClass::LikelyMutating;
    }

    let mutating_starts: &[&str] = &[
        "sed -i ",
        "perl -pi",
        "perl -i",
        "rm ",
        "rm\t",
        "rmdir ",
        "rmdir\t",
        "mv ",
        "mv\t",
        "cp ",
        "cp\t",
        "touch ",
        "touch\t",
        "mkdir ",
        "mkdir\t",
        "tee ",
        "tee\t",
        "cargo fmt",
        "cargo new",
        "cargo init",
        "cargo install",
        "cargo update",
        "cargo add",
        "cargo remove",
        "cargo clean",
        "git add",
        "git rm",
        "git mv",
        "git checkout",
        "git switch",
        "git reset",
        "git restore",
        "git stash",
        "git apply",
        "git commit",
        "git push",
        "git pull",
        "git merge",
        "git rebase",
        "npm install",
        "npm i ",
        "npm i\t",
        "npm i\n",
        "npm uninstall",
        "npm run format",
        "npm run lint:fix",
        "pnpm install",
        "pnpm add",
        "pnpm remove",
        "pnpm run format",
        "yarn install",
        "yarn add",
        "yarn remove",
        "yarn format",
        "prettier --write",
        "ruff format",
        "black ",
        "isort ",
        "gofmt -w",
        "go fmt",
        "make install",
        "make clean",
    ];
    for prefix in mutating_starts {
        if lc.starts_with(prefix) || lc == prefix.trim_end() {
            return BashCommandClass::LikelyMutating;
        }
    }

    // Inert read-only commands.
    let inert_starts: &[&str] = &[
        "ls",
        "pwd",
        "cat ",
        "head ",
        "tail ",
        "echo ",
        "printf ",
        "cargo test",
        "cargo check",
        "cargo build",
        "cargo clippy",
        "cargo run",
        "cargo doc",
        "cargo bench",
        "npm test",
        "pnpm test",
        "yarn test",
        "pytest",
        "go test",
        "git status",
        "git log",
        "git diff",
        "git show",
        "git branch",
        "grep ",
        "rg ",
        "find ",
        "wc ",
        "which ",
        "true",
        "false",
        "type ",
    ];
    for prefix in inert_starts {
        if lc.starts_with(prefix) || lc == prefix.trim_end() {
            return BashCommandClass::Inert;
        }
    }

    BashCommandClass::Unknown
}

/// Returns true iff `cmd` contains a `>` or `>>` redirection that targets
/// a file (not a pure FD-to-FD redirect like `2>&1`).
fn has_filesystem_redirect(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'>' {
            // Skip `>>`.
            let after = if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                i + 2
            } else {
                i + 1
            };
            // Skip whitespace.
            let mut j = after;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            // FD-to-FD: `>&1`. Treat as not-a-file.
            if j < bytes.len() && bytes[j] == b'&' {
                i = j + 1;
                continue;
            }
            // If something non-whitespace follows the operator, treat as a
            // file redirect. The empty case (operator at end of line) is a
            // syntax error in bash; we bail conservatively as mutating.
            if j < bytes.len() {
                return true;
            }
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // The default policy is RequireExplicitApproval, matching M3 Part 8.
    fn default_policy_is_require_explicit() {
        let p: BashCheckpointPolicy = Default::default();
        assert_eq!(p, BashCheckpointPolicy::RequireExplicitApproval);
    }

    #[test]
    // Common mutating commands are flagged LikelyMutating.
    fn flags_mutating_commands() {
        for cmd in [
            "sed -i 's/foo/bar/' src/x.rs",
            "rm -f bad.txt",
            "mv a b",
            "cp from to",
            "touch new.txt",
            "echo hi > out.txt",
            "echo hi >> out.txt",
            "cat <<EOF > x\nbody\nEOF",
            "cargo fmt --all",
            "prettier --write src/",
            "command | tee log.txt",
        ] {
            assert_eq!(
                classify_bash_command(cmd),
                BashCommandClass::LikelyMutating,
                "expected {cmd:?} to be likely mutating"
            );
        }
    }

    #[test]
    // Common read-only commands are inert.
    fn classifies_inert_commands() {
        for cmd in [
            "cargo test",
            "cargo check --all-targets",
            "cargo clippy --workspace",
            "ls -la",
            "git status",
            "grep foo src",
        ] {
            assert_eq!(
                classify_bash_command(cmd),
                BashCommandClass::Inert,
                "expected {cmd:?} to be inert"
            );
        }
    }

    #[test]
    // FD-to-FD redirection (`2>&1`) is not a filesystem write.
    fn fd_to_fd_redirect_is_not_mutating() {
        assert!(!has_filesystem_redirect("cargo test 2>&1"));
        assert_eq!(
            classify_bash_command("cargo test 2>&1"),
            BashCommandClass::Inert
        );
    }
}
