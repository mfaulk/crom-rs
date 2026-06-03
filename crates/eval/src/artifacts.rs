//! Artifact bundle layout and writers.
//!
//! Every eval run writes a self-contained directory tree so a failed task can
//! be debugged after the fact:
//!
//! ```text
//! target/code-agent-eval/runs/<run-id>/
//!   summary.json
//!   summary.md
//!   tasks/<task-id>/
//!     task.toml
//!     prompt.md
//!     transcript.jsonl
//!     events.jsonl
//!     final.patch
//!     final.diff
//!     final_summary.md
//!     command-output/001-cargo-test.stdout ...
//!     score.json
//!     report.md
//!     workspace/        (preserved on success and failure)
//! ```

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::oracle::CommandResult;

/// Handle to a single run's artifact directory.
#[derive(Debug, Clone)]
pub struct RunDir {
    pub root: PathBuf,
    pub run_id: String,
}

impl RunDir {
    /// Create `runs_root/<run_id>/tasks` and return a handle.
    pub fn create(runs_root: &Path, run_id: &str) -> io::Result<Self> {
        let root = runs_root.join(run_id);
        std::fs::create_dir_all(root.join("tasks"))?;
        Ok(Self {
            root,
            run_id: run_id.to_string(),
        })
    }

    /// Allocate (and create) the artifact directory for a task.
    pub fn task(&self, task_id: &str) -> io::Result<TaskArtifacts> {
        let dir = self.root.join("tasks").join(task_id);
        std::fs::create_dir_all(&dir)?;
        Ok(TaskArtifacts { dir })
    }

    pub fn summary_json_path(&self) -> PathBuf {
        self.root.join("summary.json")
    }

    pub fn summary_md_path(&self) -> PathBuf {
        self.root.join("summary.md")
    }
}

/// Handle to one task's artifact directory.
#[derive(Debug, Clone)]
pub struct TaskArtifacts {
    pub dir: PathBuf,
}

impl TaskArtifacts {
    pub fn workspace_dir(&self) -> PathBuf {
        self.dir.join("workspace")
    }

    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    pub fn transcript_path(&self) -> PathBuf {
        self.dir.join("transcript.jsonl")
    }

    pub fn report_path(&self) -> PathBuf {
        self.dir.join("report.md")
    }

    pub fn score_path(&self) -> PathBuf {
        self.dir.join("score.json")
    }

    pub fn final_patch_path(&self) -> PathBuf {
        self.dir.join("final.patch")
    }

    pub fn final_diff_path(&self) -> PathBuf {
        self.dir.join("final.diff")
    }

    /// Write a string artifact.
    pub fn write_string(&self, name: &str, content: &str) -> io::Result<()> {
        std::fs::write(self.dir.join(name), content)
    }

    /// Write a JSON artifact (pretty-printed).
    pub fn write_json<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
        std::fs::write(self.dir.join(name), text)
    }

    /// Write a JSONL artifact (one JSON value per line).
    pub fn write_jsonl<T: Serialize>(&self, name: &str, values: &[T]) -> io::Result<()> {
        let mut out = String::new();
        for v in values {
            let line = serde_json::to_string(v).map_err(io::Error::other)?;
            out.push_str(&line);
            out.push('\n');
        }
        std::fs::write(self.dir.join(name), out)
    }

    /// Write captured stdout/stderr for each oracle command under
    /// `command-output/NNN-<slug>.{stdout,stderr}`.
    pub fn write_command_outputs(&self, results: &[CommandResult]) -> io::Result<()> {
        if results.is_empty() {
            return Ok(());
        }
        let dir = self.dir.join("command-output");
        std::fs::create_dir_all(&dir)?;
        for (i, r) in results.iter().enumerate() {
            let slug = slugify(&r.command);
            let base = format!("{:03}-{}", i + 1, slug);
            std::fs::write(dir.join(format!("{base}.stdout")), &r.stdout)?;
            std::fs::write(dir.join(format!("{base}.stderr")), &r.stderr)?;
            // A tiny status sidecar makes exit codes greppable.
            std::fs::write(
                dir.join(format!("{base}.status")),
                format!(
                    "command: {}\nexit_code: {:?}\nsuccess: {}\ntimed_out: {}\nduration_ms: {}\n",
                    r.command, r.exit_code, r.success, r.timed_out, r.duration_ms
                ),
            )?;
        }
        Ok(())
    }
}

/// Turn a command into a short filesystem-safe slug.
pub fn slugify(command: &str) -> String {
    let mut slug: String = command
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let truncated: String = slug.chars().take(40).collect();
    if truncated.is_empty() {
        "command".to_string()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_is_filesystem_safe() {
        assert_eq!(
            slugify("cargo test --all-targets"),
            "cargo-test-all-targets"
        );
        assert_eq!(
            slugify("cargo clippy -- -D warnings"),
            "cargo-clippy-D-warnings"
        );
        assert_eq!(slugify("!!!"), "command");
    }

    #[test]
    fn writes_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let run = RunDir::create(tmp.path(), "run-1").unwrap();
        let task = run.task("t1").unwrap();
        task.write_string("prompt.md", "do it").unwrap();
        task.write_json("score.json", &serde_json::json!({"total": 1.0}))
            .unwrap();
        task.write_jsonl("events.jsonl", &[serde_json::json!({"a": 1})])
            .unwrap();
        let cmds = vec![CommandResult {
            command: "cargo test".into(),
            success: true,
            exit_code: Some(0),
            stdout: "ok".into(),
            stderr: String::new(),
            duration_ms: 5,
            timed_out: false,
        }];
        task.write_command_outputs(&cmds).unwrap();

        assert!(task.dir.join("prompt.md").exists());
        assert!(task.dir.join("score.json").exists());
        assert!(task.dir.join("events.jsonl").exists());
        assert!(task
            .dir
            .join("command-output/001-cargo-test.stdout")
            .exists());
        assert!(run.summary_json_path().starts_with(&run.root));
    }
}
