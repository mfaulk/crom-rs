use std::path::Path;
use std::time::Duration;

use crom_eval::artifacts::RunDir;
use crom_eval::fixture::default_tasks_dir;
use crom_eval::providers::ProviderMode;
use crom_eval::runner::{run_tasks_repeated, RunConfig};
use crom_eval::scoreboard::{load_attempts, Filters, Format, GroupBy, Scoreboard};
use crom_eval::task::discover_tasks;

fn assert_snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; rerun with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name} differs");
}

/// Two repetitions of the golden and security suites under the golden provider produce a stable scoreboard snapshot.
#[tokio::test]
async fn golden_runs_produce_stable_scoreboard() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks: Vec<_> = discover_tasks(&default_tasks_dir())
        .unwrap()
        .into_iter()
        .filter(|t| t.in_suite("golden") || t.in_suite("security"))
        .collect();
    let run = RunDir::create(tmp.path(), "golden-scoreboard").unwrap();
    let config = RunConfig {
        provider: ProviderMode::Golden,
        timeout_grace: Duration::from_secs(20),
        ..RunConfig::default()
    };
    run_tasks_repeated(&tasks, &run, &config, 2).await;

    let (mut attempts, warnings) = load_attempts(tmp.path(), &[]).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        attempts.len(),
        tasks.len() * 2,
        "one result per task per repetition"
    );
    // Wall time is the only run-to-run varying input the scoreboard reads.
    for a in &mut attempts {
        a.result.duration_ms = 0;
    }
    let board = Scoreboard::build(&attempts, &Filters::default(), GroupBy::Model);
    assert_snapshot("scoreboard-golden.txt", &board.render(Format::Text));
    assert_snapshot("scoreboard-golden.md", &board.render(Format::Markdown));
    assert_snapshot("scoreboard-golden.json", &board.render(Format::Json));
}

/// load_attempts restricted to named runs ignores other run directories.
#[test]
fn load_attempts_filters_runs_and_tolerates_missing_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let (none, _) = load_attempts(&tmp.path().join("absent"), &[]).unwrap();
    assert!(none.is_empty());
    std::fs::create_dir_all(tmp.path().join("other/tasks/x")).unwrap();
    std::fs::write(tmp.path().join("other/tasks/x/result.json"), "{}").unwrap();
    let (attempts, warnings) = load_attempts(tmp.path(), &["wanted".into()]).unwrap();
    assert!(attempts.is_empty() && warnings.is_empty());
    let (_, warnings) = load_attempts(tmp.path(), &[]).unwrap();
    assert_eq!(
        warnings.len(),
        1,
        "unparseable result.json is reported, not fatal"
    );
}
