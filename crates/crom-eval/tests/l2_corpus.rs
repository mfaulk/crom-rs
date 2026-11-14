//! Structural checks on the L2 capability corpus (#288). Solvability and
//! base-failure are proven by `crom eval validate-suite l2` in CI; these tests
//! pin the corpus shape without running cargo inside fixtures.

use std::collections::BTreeSet;

use crom_eval::fixture::default_tasks_dir;
use crom_eval::schema::{Level, ReferenceKind, TaskKind};
use crom_eval::task::{discover_tasks, EvalTask};

fn l2_tasks() -> Vec<EvalTask> {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .filter(|t| t.in_suite("l2"))
        .collect()
}

/// Files touched by a unified diff, from its `+++ b/<path>` headers.
fn patched_files(patch: &str) -> BTreeSet<String> {
    patch
        .lines()
        .filter_map(|l| l.strip_prefix("+++ b/"))
        .map(str::to_string)
        .collect()
}

/// The l2 suite holds at least ten distinct L2 capability tasks.
#[test]
fn l2_suite_has_ten_capability_tasks() {
    let tasks = l2_tasks();
    let ids: BTreeSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.len() >= 10, "expected >= 10 l2 tasks, found {ids:?}");
    for t in &tasks {
        assert_eq!(t.kind, Some(TaskKind::Capability), "{}", t.id);
        assert_eq!(t.level, Some(Level::L2), "{}", t.id);
        assert!(!t.requires_network, "{} must run offline", t.id);
    }
}

/// Every l2 task has hidden verification overlaid from outside the public fixture.
#[test]
fn l2_tasks_have_hidden_verification() {
    for t in l2_tasks() {
        let hidden = t.hidden.as_ref().unwrap_or_else(|| panic!("{}", t.id));
        assert!(hidden.has_checks(), "{} hidden oracle is empty", t.id);
        let fixture = hidden.fixture.as_deref().expect("hidden fixture dir");
        assert!(t.dir.join(fixture).is_dir(), "{} hidden dir missing", t.id);
        assert!(!t.dir.join("repo").join(fixture).exists(), "{}", t.id);
    }
}

/// Change tasks ship an oracle patch touching at least two files; at least one task is no_change.
#[test]
fn l2_references_are_multi_file_or_no_change() {
    let mut no_change = 0;
    for t in l2_tasks() {
        if t.expects_no_change() {
            no_change += 1;
            assert_eq!(
                t.reference,
                Some(ReferenceKind::ValidatedOracle),
                "{}",
                t.id
            );
            continue;
        }
        assert_eq!(t.reference, Some(ReferenceKind::OraclePatch), "{}", t.id);
        let rel = t.oracle_patch.as_deref().expect("oracle_patch path");
        let patch = std::fs::read_to_string(t.dir.join(rel)).expect("read oracle patch");
        let files = patched_files(&patch);
        assert!(files.len() >= 2, "{} reference touches {files:?}", t.id);
    }
    assert!(no_change >= 1, "l2 needs at least one no_change task");
}
