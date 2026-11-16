//! Structural checks for the L4 corpus (#296): real, closed upstream issues
//! frozen as tasks before any baseline run. Solvability (reference passes,
//! unmodified fixture fails) is proven by `crom eval validate-suite l4`; the
//! selection record is `docs/eval/real-world-tasks.md`.

use std::path::Path;

use crom_eval::fixture::{default_tasks_dir, is_full_sha};
use crom_eval::schema::{Level, NetworkPolicy, ReferenceKind, TaskKind};
use crom_eval::task::{discover_tasks, EvalTask, FixtureKind};

const FIXTURES_REPO: &str = "https://github.com/mfaulk/crom-eval-fixtures";

/// Per-task in-tree budget (task.toml + hidden + patch); the snapshot lives in the fixtures repo.
const MAX_TASK_BYTES: u64 = 256 * 1024;

const PERMISSIVE_LICENSES: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
];

/// Tags that mark a task as exercising tooling, security, or MCP behavior.
const TOOLING_SECURITY_TAGS: &[&str] = &["tooling", "security", "mcp"];

fn l4() -> Vec<EvalTask> {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .filter(|t| t.in_suite("l4"))
        .collect()
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
        let meta = entry.metadata().expect("metadata");
        total += if meta.is_dir() {
            dir_bytes(&entry.path())
        } else {
            meta.len()
        };
    }
    total
}

fn selection_doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/eval/real-world-tasks.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The l4 suite holds at least five offline L4 capability tasks with hidden tests and an oracle patch.
#[test]
fn l4_suite_has_five_offline_capability_tasks() {
    let tasks = l4();
    assert!(
        tasks.len() >= 5,
        "{:?}",
        tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    for t in &tasks {
        assert_eq!(t.kind, Some(TaskKind::Capability), "{}", t.id);
        assert_eq!(t.level, Some(Level::L4), "{}", t.id);
        assert!(t.id.starts_with("l4-"), "{} should be prefixed l4-", t.id);
        assert_eq!(t.network, Some(NetworkPolicy::None), "{}", t.id);
        assert!(!t.requires_network, "{}", t.id);
        assert_eq!(t.reference, Some(ReferenceKind::OraclePatch), "{}", t.id);
        let patch = t.oracle_patch.as_deref().expect("oracle_patch");
        assert!(t.dir.join(patch).is_file(), "{}: missing {patch}", t.id);

        let hidden = t.hidden.as_ref().expect("hidden section");
        let fixture = hidden.fixture.as_deref().expect("hidden fixture dir");
        assert!(t.dir.join(fixture).is_dir(), "{}: missing {fixture}/", t.id);

        let mut commands = t.oracle.primary_commands();
        commands.extend(hidden.oracle.primary_commands());
        for c in commands {
            assert!(c.contains("--offline"), "{}: `{c}` must run offline", t.id);
        }
    }
}

/// At least one l4 task exercises tooling, security, or MCP behavior.
#[test]
fn l4_suite_covers_tooling_or_security() {
    let covered = l4().into_iter().any(|t| {
        t.tags
            .iter()
            .any(|tag| TOOLING_SECURITY_TAGS.contains(&tag.as_str()))
    });
    assert!(covered, "no l4 task is tagged tooling, security, or mcp");
}

/// Every l4 fixture is pinned by full SHA to one fixtures-repo commit, and Cargo manifests and vendored sources are off-limits.
#[test]
fn l4_fixtures_are_pinned_in_the_fixtures_repo() {
    let mut revs: Vec<String> = Vec::new();
    for t in l4() {
        let f = &t.fixture;
        assert_eq!(f.kind, FixtureKind::Git, "{}", t.id);
        assert_eq!(f.repo.as_deref(), Some(FIXTURES_REPO), "{}", t.id);
        let rev = f.rev.as_deref().expect("rev");
        assert!(is_full_sha(rev), "{}: rev `{rev}` is not a full SHA", t.id);
        assert_eq!(f.path.as_deref(), Some(t.id.as_str()), "{}", t.id);
        revs.push(rev.to_string());
        for p in ["Cargo.toml", "Cargo.lock", "vendor/"] {
            assert!(
                t.forbidden_paths.iter().any(|f| f == p),
                "{}: forbidden_paths must include {p}",
                t.id
            );
        }
    }
    revs.sort();
    revs.dedup();
    assert_eq!(
        revs.len(),
        1,
        "l4 tasks should share one fixtures commit: {revs:?}"
    );
}

/// Every l4 task records its upstream issue, repo, base and fix commits, a permissive license, and fits the size budget.
#[test]
fn l4_tasks_record_issue_provenance() {
    for t in l4() {
        let p = t.provenance.as_ref().expect("provenance");
        let upstream = p.upstream.as_deref().expect("upstream");
        assert!(
            upstream.starts_with("https://github.com/"),
            "{}: {upstream}",
            t.id
        );
        let issue = p.source.as_deref().expect("source");
        assert!(
            issue.starts_with(&format!("{upstream}/issues/")),
            "{}: source `{issue}` must be an issue of {upstream}",
            t.id
        );
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
        assert!(p.created.is_some(), "{}: missing created date", t.id);
        let bytes = dir_bytes(&t.dir);
        assert!(bytes <= MAX_TASK_BYTES, "{}: {bytes} bytes", t.id);
    }
}

/// The selection record lists every l4 task and its issue, so the frozen set is documented.
#[test]
fn selection_doc_lists_every_l4_task() {
    let doc = selection_doc();
    for t in l4() {
        assert!(doc.contains(&t.id), "selection doc is missing {}", t.id);
        let issue = t
            .provenance
            .as_ref()
            .and_then(|p| p.source.clone())
            .unwrap();
        assert!(doc.contains(&issue), "selection doc is missing {issue}");
    }
}
