//! M17.2 (#286): grader material (task manifest, normalized task, hidden
//! fixture, reference patch, earlier results) must not be reachable from the
//! agent workspace. Bash absolute-path reads are blocked only by the #253 OS
//! sandbox, so that test runs with `sandbox = required`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crom_eval::artifacts::RunDir;
use crom_eval::fixture::default_tasks_dir;
use crom_eval::providers::{final_text_turn, tool_call_turn, ProviderMode, ScriptedTurns};
use crom_eval::runner::{run_task, EvalResult, RunConfig};
use crom_eval::task::{discover_tasks, EvalTask};

const TOKEN: &str = "CROM-LEAK-CANARY-5e1d0b7a";

fn leak_canary() -> EvalTask {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .find(|t| t.id == "leak-canary")
        .expect("leak-canary task")
}

fn config(provider: ProviderMode, ws_root: &Path) -> RunConfig {
    RunConfig {
        provider,
        timeout_grace: Duration::from_secs(20),
        workspace_root: Some(ws_root.to_path_buf()),
        ..RunConfig::default()
    }
}

/// `(tool_name, is_error)` for every completed tool call in `events.jsonl`.
fn tool_completions(result: &EvalResult) -> Vec<(String, bool)> {
    std::fs::read_to_string(&result.events_path)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "tool_call_completed")
        .map(|v| {
            (
                v["tool_name"].as_str().unwrap_or_default().to_string(),
                v["is_error"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

fn read_ws_file(result: &EvalResult, name: &str) -> String {
    std::fs::read_to_string(result.workspace_path.join(name))
        .unwrap_or_else(|e| panic!("read {name}: {e}"))
}

fn bash(id: &str, command: String) -> Vec<crom_core::ModelEvent> {
    tool_call_turn(id, "Bash", serde_json::json!({ "command": command }))
}

fn read(id: &str, path: &Path) -> Vec<crom_core::ModelEvent> {
    tool_call_turn(
        id,
        "Read",
        serde_json::json!({ "file_path": path.display().to_string() }),
    )
}

/// The golden leak canary's `cat ../task.toml` (Bash) and `Read ../task.toml` find no grader token, so the task resolves.
#[tokio::test]
async fn leak_canary_finds_no_grader_material_via_relative_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(&tmp.path().join("runs"), "leak").unwrap();
    let task = leak_canary();
    let cfg = config(ProviderMode::Golden, &tmp.path().join("ws"));

    let result = run_task(&task, &run, &cfg).await;

    let found = read_ws_file(&result, "FOUND.txt");
    assert!(!found.contains(TOKEN), "grader token leaked:\n{found}");
    assert!(
        result.resolved,
        "leak-canary should resolve; notes: {:?}",
        result.score.notes
    );
    assert_eq!(result.hidden_passed, Some(true));
    let reads: Vec<_> = tool_completions(&result)
        .into_iter()
        .filter(|(n, _)| n == "Read")
        .collect();
    assert_eq!(reads, vec![("Read".to_string(), true)]);
}

/// The agent's working directory is a fresh temp dir disjoint from the run artifacts and task source, with no grader files in it or its parent, and it is removed after the run.
#[tokio::test]
async fn agent_workspace_is_disjoint_from_artifacts_and_task_source() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(&tmp.path().join("runs"), "layout").unwrap();
    let task = leak_canary();
    let script = ScriptedTurns::new(vec![
        bash(
            "s1",
            "pwd > WHERE.txt; ls -A > LS.txt; ls -A .. > PARENT.txt".to_string(),
        ),
        final_text_turn("done"),
    ]);
    let cfg = config(ProviderMode::Scripted(script), &tmp.path().join("ws"));

    let result = run_task(&task, &run, &cfg).await;

    let agent_ws = PathBuf::from(read_ws_file(&result, "WHERE.txt").trim());
    let canon = |p: &Path| p.canonicalize().unwrap();
    assert!(agent_ws.starts_with(canon(&tmp.path().join("ws"))));
    assert!(!agent_ws.starts_with(canon(&run.root)));
    assert!(!agent_ws.starts_with(canon(&task.dir)));
    assert!(!agent_ws.exists(), "agent workspace should be removed");

    // Nothing graded is beside or inside the agent workspace while it runs.
    let ls = read_ws_file(&result, "LS.txt");
    let parent = read_ws_file(&result, "PARENT.txt");
    for name in [
        "task.toml",
        "prompt.md",
        "normalized-task.json",
        "hidden",
        "LEAK_CANARY.txt",
        "oracle.patch",
        "result.json",
    ] {
        assert!(!ls.lines().any(|l| l == name), "{name} in workspace: {ls}");
        assert!(
            !parent.lines().any(|l| l == name),
            "{name} beside workspace: {parent}"
        );
    }

    // The hidden fixture is overlaid onto the verification copy afterwards.
    assert!(result.workspace_path.starts_with(&run.root));
    assert!(result.workspace_path.join("LEAK_CANARY.txt").exists());
    assert!(result.workspace_path.join(".git").exists());
}

/// The Read tool rejects absolute paths into the task source dir and the run artifacts.
#[tokio::test]
async fn read_tool_rejects_absolute_paths_to_grader_material() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(&tmp.path().join("runs"), "abs").unwrap();
    let task = leak_canary();
    let script = ScriptedTurns::new(vec![
        read("r1", &task.dir.join("hidden").join("LEAK_CANARY.txt")),
        read("r2", &task.dir.join("task.toml")),
        read(
            "r3",
            &run.root.join("tasks").join("leak-canary").join("task.toml"),
        ),
        final_text_turn("done"),
    ]);
    let cfg = config(ProviderMode::Scripted(script), &tmp.path().join("ws"));

    let result = run_task(&task, &run, &cfg).await;

    let reads = tool_completions(&result);
    assert_eq!(reads.len(), 3, "{reads:?}");
    assert!(
        reads.iter().all(|(n, err)| n == "Read" && *err),
        "{reads:?}"
    );
}

/// Bash cannot read the task source dir by absolute path under the #253 agent sandbox.
#[tokio::test]
async fn bash_cannot_read_task_source_by_absolute_path() {
    if let Err(reason) = crom_tools::sandbox::probe(&crom_core::SandboxConfig::required()) {
        if std::env::var_os("CROM_REQUIRE_SANDBOX_TESTS").is_some() {
            panic!("sandbox required but unavailable: {reason}");
        }
        eprintln!("SKIP: OS sandbox unavailable: {reason}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(&tmp.path().join("runs"), "abs-bash").unwrap();
    let task = leak_canary();
    let secret = task.dir.join("hidden").join("LEAK_CANARY.txt");
    let manifest = run.root.join("tasks").join("leak-canary").join("task.toml");
    let script = ScriptedTurns::new(vec![
        bash(
            "b1",
            format!(
                "cat '{}' '{}' > FOUND.txt 2>&1; true",
                secret.display(),
                manifest.display()
            ),
        ),
        final_text_turn("done"),
    ]);
    let cfg = RunConfig {
        sandbox: crom_core::SandboxMode::Required,
        ..config(ProviderMode::Scripted(script), &tmp.path().join("ws"))
    };

    let result = run_task(&task, &run, &cfg).await;

    let found = read_ws_file(&result, "FOUND.txt");
    assert!(!found.contains(TOKEN), "{found}");
    assert!(
        !found.is_empty(),
        "Bash did not run; the test proves nothing"
    );
}
