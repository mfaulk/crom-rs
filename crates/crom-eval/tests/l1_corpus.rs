//! Structural invariants of the `l1` capability suite (M17.2, #287). Fixture
//! behavior (reference passes, unmodified fixture fails) is checked by
//! `crom eval validate-suite l1` in CI, which builds every fixture.

use std::collections::HashSet;

use crom_eval::fixture::default_tasks_dir;
use crom_eval::schema::{Level, NetworkPolicy, ReferenceKind, TaskKind};
use crom_eval::task::{discover_tasks, EvalTask};

fn l1_tasks() -> Vec<EvalTask> {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .filter(|t| t.in_suite("l1"))
        .collect()
}

/// The suite holds at least ten distinct tasks, including two or more `no_change` tasks.
#[test]
fn l1_suite_size_and_negatives() {
    let tasks = l1_tasks();
    assert!(tasks.len() >= 10, "l1 has {} tasks", tasks.len());
    let ids: HashSet<_> = tasks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids.len(), tasks.len(), "duplicate ids in l1");
    let negatives = tasks.iter().filter(|t| t.expects_no_change()).count();
    assert!(negatives >= 2, "l1 has {negatives} no_change tasks");
}

/// Every l1 task is an offline L1 capability task with hidden checks and a reference.
#[test]
fn l1_tasks_are_offline_capability_tasks_with_hidden_and_reference() {
    for t in l1_tasks() {
        assert_eq!(t.kind, Some(TaskKind::Capability), "{}", t.id);
        assert_eq!(t.level, Some(Level::L1), "{}", t.id);
        assert_eq!(t.network, Some(NetworkPolicy::None), "{}", t.id);
        assert!(!t.requires_network, "{}", t.id);
        assert!(
            t.hidden.as_ref().is_some_and(|h| h.has_checks()),
            "{} lacks hidden checks",
            t.id
        );
        match t.reference {
            Some(ReferenceKind::OraclePatch) => {
                let patch = t.oracle_patch.as_deref().expect("oracle_patch path");
                assert!(t.dir.join(patch).is_file(), "{} oracle patch missing", t.id);
            }
            Some(ReferenceKind::ValidatedOracle) => {
                assert!(t.expects_no_change(), "{}", t.id);
            }
            None => panic!("{} declares no reference", t.id),
        }
    }
}

/// Hidden fixtures and oracle patches live outside the agent-visible `repo/` fixture.
#[test]
fn l1_grader_material_is_outside_the_fixture() {
    for t in l1_tasks() {
        let repo = t.dir.join(t.fixture.path.as_deref().unwrap_or("repo"));
        if let Some(rel) = t.hidden.as_ref().and_then(|h| h.fixture.as_deref()) {
            assert!(
                !t.dir.join(rel).starts_with(&repo),
                "{} hidden in repo",
                t.id
            );
        }
        if let Some(rel) = t.oracle_patch.as_deref() {
            assert!(
                !t.dir.join(rel).starts_with(&repo),
                "{} patch in repo",
                t.id
            );
        }
    }
}
