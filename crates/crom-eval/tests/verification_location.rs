use std::path::Path;
use std::time::Duration;

use crom_eval::artifacts::RunDir;
use crom_eval::providers::{final_text_turn, ProviderMode, ScriptedTurns};
use crom_eval::runner::{run_task, RunConfig};
use crom_eval::task::load_task;

const TASK_TOML: &str = r#"
schema_version = 3
task_version = "1.0.0"
kind = "harness"
level = "L0"
network = "none"
id = "verify-outside-workspace"
title = "Hidden checks run outside any enclosing Cargo workspace"
category = "harness"
difficulty = "easy"
tags = ["small"]
suites = ["test"]
prompt = "Do nothing."
max_turns = 2
timeout_secs = 60

[fixture]
kind = "copy_dir"
path = "repo"

[oracle]
kind = "command"
commands = ["cargo metadata --offline --no-deps --format-version 1"]

[hidden.oracle]
kind = "command"
commands = ["cargo metadata --offline --no-deps --format-version 1"]

[provenance]
author = "crom maintainers"
source = "original"
license = "Apache-2.0 OR MIT"
created = "2026-11-18"
"#;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Hidden checks pass for an upstream-style package (no `[workspace]`) when the run directory sits inside another Cargo workspace, as on CI.
#[tokio::test]
async fn hidden_checks_ignore_an_enclosing_cargo_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    // Stand-in for the crom-rs checkout that `crom eval` runs from.
    write(
        &tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = []\n",
    );
    let task_dir = tmp.path().join("task-src");
    write(&task_dir.join("task.toml"), TASK_TOML);
    write(
        &task_dir.join("repo/Cargo.toml"),
        "[package]\nname = \"upstream\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(&task_dir.join("repo/src/lib.rs"), "");
    let task = load_task(&task_dir.join("task.toml")).expect("load task");
    let run = RunDir::create(&tmp.path().join("target/crom-eval/runs"), "ws").unwrap();
    let cfg = RunConfig {
        provider: ProviderMode::Scripted(ScriptedTurns::new(vec![final_text_turn("done")])),
        timeout_grace: Duration::from_secs(20),
        workspace_root: Some(std::env::temp_dir().join("crom-eval-ws-test")),
        ..RunConfig::default()
    };

    let result = run_task(&task, &run, &cfg).await;

    assert!(result.public_passed, "public: {:?}", result.score.notes);
    assert_eq!(
        result.hidden_passed,
        Some(true),
        "hidden: {:?}",
        result.score.notes
    );
    assert!(
        result
            .workspace_path
            .starts_with(tmp.path().join("target/crom-eval/runs")),
        "verified workspace must end up in the artifact bundle: {}",
        result.workspace_path.display()
    );
}
