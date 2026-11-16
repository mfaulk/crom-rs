//! Structural checks for the L3 corpora (#289). `l3` tasks are upstream
//! repositories pinned in the fixtures repo; `l3-mini` tasks are small in-tree
//! workspaces. Solvability (reference passes, unmodified fixture fails) is
//! proven by `crom eval validate-suite <suite>`.

use std::path::Path;

use crom_eval::fixture::{default_tasks_dir, is_full_sha};
use crom_eval::schema::{Level, NetworkPolicy, ReferenceKind, TaskKind};
use crom_eval::task::{discover_tasks, EvalTask, FixtureKind};

const FIXTURES_REPO: &str = "https://github.com/mfaulk/crom-eval-fixtures";

/// Per-task in-tree budget for `l3-mini` (repo + hidden + patch), in bytes.
const MAX_MINI_TASK_BYTES: u64 = 96 * 1024;

/// Per-task in-tree budget for `l3` (task.toml + hidden + patch); the snapshot lives in the fixtures repo.
const MAX_UPSTREAM_TASK_BYTES: u64 = 256 * 1024;

const PERMISSIVE_LICENSES: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
];

fn suite(name: &str) -> Vec<EvalTask> {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .filter(|t| t.in_suite(name))
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

fn assert_offline_with_hidden_tests_and_reference(t: &EvalTask) {
    assert_eq!(t.kind, Some(TaskKind::Capability), "{}", t.id);
    assert_eq!(t.level, Some(Level::L3), "{}", t.id);
    assert!(t.id.starts_with("l3-"), "{} should be prefixed l3-", t.id);
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

/// The l3 suite holds ten offline upstream-derived capability tasks with hidden tests and an oracle patch.
#[test]
fn l3_suite_has_ten_offline_upstream_tasks() {
    let tasks = suite("l3");
    assert_eq!(
        tasks.len(),
        10,
        "{:?}",
        tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    for t in &tasks {
        assert_offline_with_hidden_tests_and_reference(t);
    }
}

/// Every l3 fixture is pinned by full SHA to one commit of the fixtures repo, and Cargo manifests are off-limits.
#[test]
fn l3_fixtures_are_pinned_in_the_fixtures_repo() {
    let tasks = suite("l3");
    let mut revs: Vec<&str> = Vec::new();
    for t in &tasks {
        let f = &t.fixture;
        assert_eq!(f.kind, FixtureKind::Git, "{}", t.id);
        assert_eq!(f.repo.as_deref(), Some(FIXTURES_REPO), "{}", t.id);
        let rev = f.rev.as_deref().expect("rev");
        assert!(is_full_sha(rev), "{}: rev `{rev}` is not a full SHA", t.id);
        assert_eq!(f.path.as_deref(), Some(t.id.as_str()), "{}", t.id);
        revs.push(rev);
        for p in ["Cargo.toml", "Cargo.lock"] {
            assert!(
                t.forbidden_paths.iter().any(|f| f == p),
                "{}: forbidden_paths must include {p}",
                t.id
            );
        }
    }
    revs.dedup();
    assert_eq!(
        revs.len(),
        1,
        "l3 tasks should share one fixtures commit: {revs:?}"
    );
}

/// Every l3 task records its upstream repo, base and fix commits, and a permissive SPDX license.
#[test]
fn l3_tasks_record_upstream_provenance() {
    for t in suite("l3") {
        let p = t.provenance.as_ref().expect("provenance");
        assert_eq!(p.source.as_deref(), Some("upstream"), "{}", t.id);
        let upstream = p.upstream.as_deref().expect("upstream");
        assert!(upstream.starts_with("https://"), "{}: {upstream}", t.id);
        for (k, v) in [
            ("base_commit", &p.base_commit),
            ("fix_commit", &p.fix_commit),
        ] {
            let sha = v
                .as_deref()
                .unwrap_or_else(|| panic!("{}: missing {k}", t.id));
            assert!(is_full_sha(sha), "{}: {k} `{sha}`", t.id);
        }
        assert_ne!(p.base_commit, p.fix_commit, "{}", t.id);
        let license = p.license.as_deref().expect("license");
        assert!(
            license
                .split(" OR ")
                .any(|l| PERMISSIVE_LICENSES.contains(&l.trim())),
            "{}: license `{license}`",
            t.id
        );
        let bytes = dir_bytes(&t.dir);
        assert!(bytes <= MAX_UPSTREAM_TASK_BYTES, "{}: {bytes} bytes", t.id);
    }
}

/// The l3-mini suite holds ten offline capability tasks with hidden tests and an oracle patch.
#[test]
fn l3_mini_suite_has_ten_offline_tasks() {
    let tasks = suite("l3-mini");
    assert_eq!(tasks.len(), 10);
    for t in &tasks {
        assert_offline_with_hidden_tests_and_reference(t);
    }
}

/// Every l3-mini fixture is an in-tree multi-crate Cargo workspace with no external dependencies.
#[test]
fn l3_mini_fixtures_are_dependency_free_multi_crate_workspaces() {
    for t in suite("l3-mini") {
        assert_eq!(t.fixture.kind, FixtureKind::CopyDir, "{}", t.id);
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

/// Every l3-mini task records provenance with an SPDX license and stays within the size budget.
#[test]
fn l3_mini_tasks_record_provenance_and_fit_budget() {
    for t in suite("l3-mini") {
        let p = t.provenance.as_ref().expect("provenance");
        assert!(
            p.license.as_deref().is_some_and(|l| !l.is_empty()),
            "{}",
            t.id
        );
        assert!(p.source.is_some(), "{}", t.id);
        let bytes = dir_bytes(&t.dir);
        assert!(
            bytes <= MAX_MINI_TASK_BYTES,
            "{}: {bytes} bytes exceeds {MAX_MINI_TASK_BYTES}",
            t.id
        );
    }
}
