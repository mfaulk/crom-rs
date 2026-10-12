//! External benchmark MVP adapters (M15I).
//!
//! These adapters convert *local* benchmark-style fixtures into the harness's
//! common [`EvalTask`] form. They are MVP-quality: they prove the adapter
//! boundary and run small local slices through the same pipeline. They do not
//! clone arbitrary GitHub repos or implement leaderboard-grade execution.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::schema::{command_oracle, HiddenSpec};
use crate::task::{EvalTask, FixtureKind, FixtureSpec, OracleKind, OracleSpec};

/// Read the first existing instruction/prompt file in `dir`.
fn read_instruction(dir: &Path) -> Result<String> {
    for name in [
        "instruction.txt",
        "prompt.txt",
        "PROMPT.md",
        "instructions.md",
    ] {
        let p = dir.join(name);
        if p.exists() {
            return std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()));
        }
    }
    Err(anyhow!(
        "no instruction.txt/prompt.txt found in {}",
        dir.display()
    ))
}

fn dir_id(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "task".to_string())
}

/// Load a generic instruction-driven task from a bare directory. The whole
/// directory becomes the fixture; `test.sh` (if present) is the public oracle.
pub fn load_instruction_task(dir: &Path, adapter: &str) -> Result<EvalTask> {
    let prompt = read_instruction(dir)?;
    let oracle = if dir.join("test.sh").exists() {
        command_oracle(vec!["sh test.sh".to_string()])
    } else {
        OracleSpec::default_none()
    };
    let mut task = EvalTask::minimal(dir_id(dir), dir_id(dir), prompt, oracle, dir);
    task.fixture = FixtureSpec {
        kind: FixtureKind::CopyDir,
        path: Some(".".to_string()),
    };
    task.tags = vec![adapter.to_string(), "external".to_string()];
    Ok(task)
}

/// SWE-bench-style instance metadata (`instance.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct SweBenchInstance {
    pub instance_id: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub base_commit: Option<String>,
    pub problem_statement: String,
    /// Relative path (within the instance dir) to the hidden test patch.
    #[serde(default)]
    pub test_patch: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// Command that verifies the fix (run as hidden verification).
    #[serde(default)]
    pub test_command: Option<String>,
    /// Fixture subdir holding the repo (default `repo`).
    #[serde(default)]
    pub fixture: Option<String>,
}

/// SWE-bench-style adapter (MVP, local fixtures only).
pub struct SweBenchAdapter;

impl SweBenchAdapter {
    pub fn load_one(dir: &Path) -> Result<EvalTask> {
        let meta_path = dir.join("instance.json");
        let text = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("reading {}", meta_path.display()))?;
        let inst: SweBenchInstance = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", meta_path.display()))?;

        let fixture_sub = inst.fixture.clone().unwrap_or_else(|| "repo".to_string());
        let test_cmd = inst
            .test_command
            .clone()
            .unwrap_or_else(|| "cargo test --all-targets".to_string());

        // Hidden verification: apply the test patch (if any), then run the test
        // command. The patch is applied from an absolute path so it never lives
        // in the workspace during the agent run.
        let mut hidden_cmds = Vec::new();
        if let Some(rel) = &inst.test_patch {
            let abs = std::fs::canonicalize(dir.join(rel))
                .map_err(|_| anyhow!("test_patch not found: {}", dir.join(rel).display()))?;
            // Quote the path: the command runs through `sh -c`, so a path with
            // spaces would otherwise be split into multiple arguments.
            hidden_cmds.push(format!("git apply '{}'", abs.display()));
        }
        hidden_cmds.push(test_cmd);

        let mut task = EvalTask::minimal(
            inst.instance_id.clone(),
            format!(
                "SWE-bench: {}",
                inst.repo
                    .clone()
                    .unwrap_or_else(|| inst.instance_id.clone())
            ),
            inst.problem_statement.clone(),
            OracleSpec::default_none(),
            dir,
        );
        task.fixture = FixtureSpec {
            kind: FixtureKind::CopyDir,
            path: Some(fixture_sub),
        };
        task.hidden = Some(HiddenSpec {
            fixture: None,
            oracle: command_oracle(hidden_cmds),
        });
        task.category = Some("bugfix".to_string());
        task.tags = vec!["swebench".to_string(), "external".to_string()];
        if let Some(v) = inst.version {
            task.tags.push(format!("version:{v}"));
        }
        task.capabilities.requires_test_feedback = true;
        task.capabilities.requires_repo_search = true;
        Ok(task)
    }
}

impl crate::adapter::EvalAdapter for SweBenchAdapter {
    fn id(&self) -> &'static str {
        "swebench"
    }
    fn describe(&self) -> &'static str {
        "SWE-bench-style local instance (instance.json + repo/ + test patch), MVP"
    }
    fn discover_tasks(&self, source: &Path) -> Result<Vec<EvalTask>> {
        if source.join("instance.json").exists() {
            return Ok(vec![Self::load_one(source)?]);
        }
        let mut tasks = Vec::new();
        for entry in
            std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
        {
            let p = entry?.path();
            if p.is_dir() && p.join("instance.json").exists() {
                tasks.push(Self::load_one(&p)?);
            }
        }
        if tasks.is_empty() {
            return Err(anyhow!("no instance.json under {}", source.display()));
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }
}

/// Aider/Exercism-style Rust exercise adapter (MVP).
pub struct AiderAdapter;

impl AiderAdapter {
    pub fn load_one(dir: &Path) -> Result<EvalTask> {
        let prompt = read_instruction(dir)?;
        // Exercise crates are verified with cargo test.
        let oracle = if dir.join("Cargo.toml").exists() || dir.join("repo/Cargo.toml").exists() {
            command_oracle(vec!["cargo test --all-targets".to_string()])
        } else if dir.join("test.sh").exists() {
            command_oracle(vec!["sh test.sh".to_string()])
        } else {
            OracleSpec::default_none()
        };
        let fixture_path = if dir.join("repo").exists() {
            "repo"
        } else {
            "."
        };
        let mut task = EvalTask::minimal(dir_id(dir), dir_id(dir), prompt, oracle, dir);
        task.fixture = FixtureSpec {
            kind: FixtureKind::CopyDir,
            path: Some(fixture_path.to_string()),
        };
        task.category = Some("feature".to_string());
        task.tags = vec![
            "aider".to_string(),
            "exercism".to_string(),
            "external".to_string(),
        ];
        task.capabilities.requires_test_feedback = true;
        Ok(task)
    }
}

impl crate::adapter::EvalAdapter for AiderAdapter {
    fn id(&self) -> &'static str {
        "aider"
    }
    fn describe(&self) -> &'static str {
        "Aider/Exercism-style Rust exercise (instruction + cargo test), MVP; supports --attempts"
    }
    fn discover_tasks(&self, source: &Path) -> Result<Vec<EvalTask>> {
        if read_instruction(source).is_ok() {
            return Ok(vec![Self::load_one(source)?]);
        }
        let mut tasks = Vec::new();
        for entry in
            std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
        {
            let p = entry?.path();
            if p.is_dir() && read_instruction(&p).is_ok() {
                tasks.push(Self::load_one(&p)?);
            }
        }
        if tasks.is_empty() {
            return Err(anyhow!("no exercises under {}", source.display()));
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }
}

/// Terminal-Bench-style task wrapper (MVP, local execution).
pub struct TerminalAdapter;

impl TerminalAdapter {
    pub fn load_one(dir: &Path) -> Result<EvalTask> {
        let prompt = read_instruction(dir)?;
        if !dir.join("test.sh").exists() {
            return Err(anyhow!(
                "terminal task {} is missing test.sh",
                dir.display()
            ));
        }
        let mut task = EvalTask::minimal(
            dir_id(dir),
            format!("terminal: {}", dir_id(dir)),
            prompt,
            OracleSpec {
                kind: OracleKind::Command,
                commands: vec!["sh test.sh".to_string()],
                ..Default::default()
            },
            dir,
        );
        task.fixture = FixtureSpec {
            kind: FixtureKind::CopyDir,
            path: Some(".".to_string()),
        };
        task.category = Some("cli".to_string());
        // image.txt / Dockerfile presence is recorded but local mode is used.
        let mode = if dir.join("Dockerfile").exists() || dir.join("image.txt").exists() {
            "terminal-local(has-image)"
        } else {
            "terminal-local"
        };
        task.tags = vec![
            "terminal".to_string(),
            "external".to_string(),
            mode.to_string(),
        ];
        task.capabilities.requires_cli_execution = true;
        if dir.join("oracle.patch").exists() {
            task.oracle_patch = Some("oracle.patch".to_string());
        }
        Ok(task)
    }
}

impl crate::adapter::EvalAdapter for TerminalAdapter {
    fn id(&self) -> &'static str {
        "terminal"
    }
    fn describe(&self) -> &'static str {
        "Terminal-Bench-style task dir (instruction.txt + test.sh, optional image), MVP local mode"
    }
    fn discover_tasks(&self, source: &Path) -> Result<Vec<EvalTask>> {
        if source.join("test.sh").exists() {
            return Ok(vec![Self::load_one(source)?]);
        }
        let mut tasks = Vec::new();
        for entry in
            std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
        {
            let p = entry?.path();
            if p.is_dir() && p.join("test.sh").exists() {
                tasks.push(Self::load_one(&p)?);
            }
        }
        if tasks.is_empty() {
            return Err(anyhow!("no terminal tasks under {}", source.display()));
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::EvalAdapter;

    #[test]
    fn swebench_local_task_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inst1");
        std::fs::create_dir_all(dir.join("repo/src")).unwrap();
        std::fs::write(dir.join("repo/src/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(
            dir.join("instance.json"),
            r#"{
  "instance_id": "example__repo-1",
  "repo": "example/repo",
  "problem_statement": "Fix the bug in f().",
  "test_command": "test -f src/lib.rs"
}"#,
        )
        .unwrap();
        let task = SweBenchAdapter::load_one(&dir).unwrap();
        assert_eq!(task.id, "example__repo-1");
        assert!(task.hidden.is_some());
        assert!(task.prompt.contains("Fix the bug"));
        let cmds = task.hidden.unwrap().oracle.primary_commands();
        assert!(cmds.iter().any(|c| c.contains("test -f")));
    }

    #[test]
    fn aider_exercise_loads_with_cargo_test() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ex1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("instruction.txt"), "Implement foo.").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let task = AiderAdapter::load_one(&dir).unwrap();
        assert!(task
            .oracle
            .primary_commands()
            .iter()
            .any(|c| c.contains("cargo test")));
    }

    #[test]
    fn terminal_task_requires_test_sh() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("t1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("instruction.txt"), "echo hi to out.txt").unwrap();
        assert!(TerminalAdapter::load_one(&dir).is_err());
        std::fs::write(dir.join("test.sh"), "exit 0").unwrap();
        let task = TerminalAdapter::load_one(&dir).unwrap();
        assert!(task
            .oracle
            .primary_commands()
            .iter()
            .any(|c| c.contains("test.sh")));
    }

    #[test]
    fn adapter_discover_single_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ex1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("instruction.txt"), "do it").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let tasks = AiderAdapter.discover_tasks(&dir).unwrap();
        assert_eq!(tasks.len(), 1);
    }
}
