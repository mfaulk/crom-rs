//! Adapter abstraction (M15A).
//!
//! An [`EvalAdapter`] turns some *source* of tasks into the harness's common
//! [`EvalTask`] representation, so internal TOML tasks, one-off directories, and
//! external benchmark slices all flow through the same runner, oracle, scoring,
//! and artifact pipeline. The runner is adapter-agnostic: an adapter's only job
//! is discovery + normalization into `EvalTask` (including any hidden
//! verification), which the runner then executes uniformly.
//!
//! Adapters in this build:
//!
//! | id          | source                                   | maturity |
//! |-------------|------------------------------------------|----------|
//! | `internal`  | bundled `eval/tasks` TOML manifests      | full     |
//! | `directory` | a single local task directory            | full     |
//! | `swebench`  | a local SWE-bench-style instance dir     | MVP      |
//! | `aider`     | a local Aider/Exercism-style exercise    | MVP      |
//! | `terminal`  | a local Terminal-Bench-style task dir    | MVP      |
//!
//! External adapters are intentionally MVP-quality and operate on **local
//! fixtures** only — they prove the adapter boundary, not leaderboard-grade
//! benchmark execution.

use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::external;
use crate::task::{discover_tasks, load_task, EvalTask};

/// Converts a task source into runnable [`EvalTask`]s.
pub trait EvalAdapter: Send + Sync {
    /// Stable adapter id used on the CLI and recorded in artifacts/index.
    fn id(&self) -> &'static str;

    /// One-line human description (shown by `eval adapters`).
    fn describe(&self) -> &'static str;

    /// Discover and load all tasks reachable from `source`.
    fn discover_tasks(&self, source: &Path) -> Result<Vec<EvalTask>>;
}

/// The bundled internal-task adapter: a directory of `<task-id>/task.toml`.
pub struct InternalAdapter;

impl EvalAdapter for InternalAdapter {
    fn id(&self) -> &'static str {
        "internal"
    }

    fn describe(&self) -> &'static str {
        "Bundled code-agent TOML tasks under eval/tasks"
    }

    fn discover_tasks(&self, source: &Path) -> Result<Vec<EvalTask>> {
        discover_tasks(source).map_err(|e| anyhow!("internal adapter: {e}"))
    }
}

/// One-off local task directory adapter.
///
/// Accepts either a directory containing a `task.toml` (loaded directly), or a
/// bare repo directory containing an `instruction.txt`/`prompt.txt` plus an
/// optional `test.sh` verification script (a lightweight ad-hoc task).
pub struct DirectoryAdapter;

impl DirectoryAdapter {
    /// Load a single task from a directory.
    pub fn load_one(dir: &Path) -> Result<EvalTask> {
        let manifest = dir.join("task.toml");
        if manifest.exists() {
            return load_task(&manifest).map_err(|e| anyhow!("directory adapter: {e}"));
        }
        external::load_instruction_task(dir, "directory")
            .with_context(|| format!("directory adapter: loading {}", dir.display()))
    }
}

impl EvalAdapter for DirectoryAdapter {
    fn id(&self) -> &'static str {
        "directory"
    }

    fn describe(&self) -> &'static str {
        "A single local task directory (task.toml or instruction.txt + test.sh)"
    }

    fn discover_tasks(&self, source: &Path) -> Result<Vec<EvalTask>> {
        if !source.exists() {
            return Err(anyhow!(
                "directory adapter: path does not exist: {}",
                source.display()
            ));
        }
        // A directory that is itself a task.
        if source.join("task.toml").exists()
            || source.join("instruction.txt").exists()
            || source.join("prompt.txt").exists()
        {
            return Ok(vec![Self::load_one(source)?]);
        }
        // Otherwise treat it as a directory of task directories.
        let mut tasks = Vec::new();
        for entry in
            std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
        {
            let path = entry?.path();
            if path.is_dir()
                && (path.join("task.toml").exists()
                    || path.join("instruction.txt").exists()
                    || path.join("prompt.txt").exists())
            {
                tasks.push(Self::load_one(&path)?);
            }
        }
        if tasks.is_empty() {
            return Err(anyhow!(
                "directory adapter: no task.toml or instruction.txt under {}",
                source.display()
            ));
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }
}

/// Registry of available adapters.
pub struct AdapterRegistry {
    adapters: Vec<Box<dyn EvalAdapter>>,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self {
            adapters: vec![
                Box::new(InternalAdapter),
                Box::new(DirectoryAdapter),
                Box::new(external::SweBenchAdapter),
                Box::new(external::AiderAdapter),
                Box::new(external::TerminalAdapter),
            ],
        }
    }
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// List `(id, description)` for every registered adapter.
    pub fn list(&self) -> Vec<(&'static str, &'static str)> {
        self.adapters
            .iter()
            .map(|a| (a.id(), a.describe()))
            .collect()
    }

    /// Look up an adapter by id.
    pub fn get(&self, id: &str) -> Result<&dyn EvalAdapter> {
        self.adapters
            .iter()
            .find(|a| a.id() == id)
            .map(|b| b.as_ref())
            .ok_or_else(|| {
                let ids: Vec<&str> = self.adapters.iter().map(|a| a.id()).collect();
                anyhow!("unknown adapter '{id}'; available: {}", ids.join(", "))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::default_tasks_dir;

    #[test]
    fn registry_lists_adapters() {
        let reg = AdapterRegistry::new();
        let ids: Vec<&str> = reg.list().iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&"internal"));
        assert!(ids.contains(&"directory"));
        assert!(ids.contains(&"swebench"));
    }

    #[test]
    fn unknown_adapter_errors_clearly() {
        let reg = AdapterRegistry::new();
        let err = reg.get("nope").map(|_| ()).unwrap_err().to_string();
        assert!(err.contains("unknown adapter"));
        assert!(err.contains("internal"));
    }

    #[test]
    fn internal_adapter_discovers_tasks() {
        let reg = AdapterRegistry::new();
        let adapter = reg.get("internal").unwrap();
        let tasks = adapter.discover_tasks(&default_tasks_dir()).unwrap();
        assert!(tasks.len() >= 8);
    }

    #[test]
    fn directory_adapter_loads_task_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mytask");
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("repo/README.md"), "hello").unwrap();
        std::fs::write(
            dir.join("task.toml"),
            r#"
id = "adhoc-1"
title = "ad hoc"
prompt = "do the thing"
[oracle]
kind = "none"
"#,
        )
        .unwrap();
        let task = DirectoryAdapter::load_one(&dir).unwrap();
        assert_eq!(task.id, "adhoc-1");
    }

    #[test]
    fn directory_adapter_loads_instruction_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bare");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("instruction.txt"), "Fix the bug.").unwrap();
        std::fs::write(dir.join("test.sh"), "exit 0").unwrap();
        let task = DirectoryAdapter::load_one(&dir).unwrap();
        assert!(task.prompt.contains("Fix the bug"));
        assert!(!task.oracle.primary_commands().is_empty());
    }
}
