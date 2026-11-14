//! Structural checks for the L3 corpus (#289). Solvability (reference passes,
//! unmodified fixture fails) is proven by `crom eval validate-suite l3` in CI.

use std::path::Path;

use crom_eval::fixture::default_tasks_dir;
use crom_eval::schema::{Level, NetworkPolicy, ReferenceKind, TaskKind};
use crom_eval::task::{discover_tasks, EvalTask};

/// Per-task fixture budget (repo + hidden + patch), in bytes.
const MAX_TASK_BYTES: u64 = 96 * 1024;

fn l3_tasks() -> Vec<EvalTask> {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .filter(|t| t.in_suite("l3"))
        .collect()
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
        let path = entry.path();
        let meta = entry.metadata().expect("metadata");
        if meta.is_dir() {
            total += dir_bytes(&path);
        } else {
            total += meta.len();
        }
    }
    total
}

/// Workspace members declared in a fixture's root `Cargo.toml`.
fn workspace_members(repo: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(repo.join("Cargo.toml")).expect("fixture Cargo.toml");
    let value: toml::Table = toml::from_str(&manifest).expect("fixture Cargo.toml parses");
    value
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .map(|m| {
            m.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The l3 suite holds at least ten independent L3 capability tasks.
#[test]
fn l3_suite_has_ten_capability_tasks() {
    let tasks = l3_tasks();
    assert!(
        tasks.len() >= 10,
        "expected >= 10 l3 tasks, found {}",
        tasks.len()
    );
    for t in &tasks {
        assert_eq!(t.kind, Some(TaskKind::Capability), "{}", t.id);
        assert_eq!(t.level, Some(Level::L3), "{}", t.id);
        assert!(t.id.starts_with("l3-"), "{} should be prefixed l3-", t.id);
    }
}

/// Every L3 task runs offline, has hidden tests outside the fixture, and an oracle-patch reference.
#[test]
fn l3_tasks_are_offline_with_hidden_tests_and_reference() {
    for t in l3_tasks() {
        assert_eq!(t.network, Some(NetworkPolicy::None), "{}", t.id);
        assert!(!t.requires_network, "{}", t.id);
        assert_eq!(t.reference, Some(ReferenceKind::OraclePatch), "{}", t.id);
        let patch = t.oracle_patch.as_deref().expect("oracle_patch");
        assert!(t.dir.join(patch).is_file(), "{}: missing {patch}", t.id);

        let hidden = t.hidden.as_ref().expect("hidden section");
        let fixture = hidden.fixture.as_deref().expect("hidden fixture dir");
        assert!(!fixture.starts_with("repo"), "{}: hidden inside repo", t.id);
        assert!(t.dir.join(fixture).is_dir(), "{}: missing {fixture}/", t.id);

        let mut commands = t.oracle.primary_commands();
        commands.extend(hidden.oracle.primary_commands());
        assert!(!commands.is_empty(), "{}", t.id);
        for c in commands {
            assert!(c.contains("--offline"), "{}: `{c}` must run offline", t.id);
        }
    }
}

/// Every L3 fixture is a multi-crate Cargo workspace with no external dependencies.
#[test]
fn l3_fixtures_are_dependency_free_multi_crate_workspaces() {
    for t in l3_tasks() {
        let repo = t.dir.join("repo");
        let members = workspace_members(&repo);
        assert!(members.len() >= 3, "{}: {members:?}", t.id);
        for m in &members {
            let manifest = std::fs::read_to_string(repo.join(m).join("Cargo.toml"))
                .unwrap_or_else(|e| panic!("{}: {m}/Cargo.toml: {e}", t.id));
            let value: toml::Table = toml::from_str(&manifest).expect("member manifest parses");
            if let Some(deps) = value.get("dependencies").and_then(|d| d.as_table()) {
                for (name, spec) in deps {
                    assert!(
                        spec.get("path").is_some(),
                        "{}: {m} depends on non-path crate `{name}`",
                        t.id
                    );
                }
            }
        }
        assert!(
            !repo.join("Cargo.lock").exists(),
            "{}: commit no lockfile",
            t.id
        );
        assert!(!repo.join("target").exists(), "{}: stray target/", t.id);
    }
}

/// Every L3 task records provenance with an SPDX license and stays within the size budget.
#[test]
fn l3_tasks_record_provenance_and_fit_budget() {
    for t in l3_tasks() {
        let p = t.provenance.as_ref().expect("provenance");
        assert!(
            p.license.as_deref().is_some_and(|l| !l.is_empty()),
            "{}",
            t.id
        );
        assert!(p.source.is_some(), "{}", t.id);
        let bytes = dir_bytes(&t.dir);
        assert!(
            bytes <= MAX_TASK_BYTES,
            "{}: {bytes} bytes exceeds {MAX_TASK_BYTES}",
            t.id
        );
    }
}
