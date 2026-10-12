//! Offline CLI tests for the M7 `skills` and `commands` subcommands.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crom"))
}

fn write_skill(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("SKILL.md"), body).unwrap();
}

#[test]
// `skills list` prints the discovered skills.
fn skills_list_works() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join(".agent").join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(
        &skills,
        "release-pr",
        "---\ndescription: Make a release PR\n---\nbody\n",
    );
    let out = Command::new(bin())
        .args(["skills", "list", "--cwd"])
        .arg(tmp.path())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("release-pr"), "stdout={stdout}");
    assert!(stdout.contains("Make a release PR"), "stdout={stdout}");
}

#[test]
// `skills show` renders the body preview.
fn skills_show_works() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join(".agent").join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(
        &skills,
        "release-pr",
        "---\ndescription: d\n---\nThis is the procedure body.\n",
    );
    let out = Command::new(bin())
        .args(["skills", "show", "release-pr", "--cwd"])
        .arg(tmp.path())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("procedure body"), "stdout={stdout}");
}

#[test]
// `skills validate` reports successes and surfaces failures with a non-zero exit.
fn skills_validate_works() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join(".agent").join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "good", "---\ndescription: d\n---\nbody\n");
    let out = Command::new(bin())
        .args(["skills", "validate", "--cwd"])
        .arg(tmp.path())
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ok    good"), "stdout={stdout}");
}

#[test]
// `skills init` creates a skeleton SKILL.md.
fn skills_init_works() {
    let tmp = TempDir::new().unwrap();
    let out = Command::new(bin())
        .args(["skills", "init", "my-skill", "--cwd"])
        .arg(tmp.path())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = tmp
        .path()
        .join(".agent")
        .join("skills")
        .join("my-skill")
        .join("SKILL.md");
    assert!(path.exists(), "skeleton not created at {}", path.display());
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("name: my-skill"));
}

#[test]
// `commands list` surfaces filesystem skill parse errors on stderr instead
// of silently dropping them.
fn commands_list_surfaces_skill_parse_errors() {
    let tmp = TempDir::new().unwrap();
    let skills = tmp.path().join(".agent").join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    // No frontmatter — the loader emits a MissingFrontmatter error.
    write_skill(&skills, "broken", "no frontmatter\n");
    let out = Command::new(bin())
        .args(["commands", "list", "--cwd"])
        .arg(tmp.path())
        .output()
        .expect("spawn");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[skills]"),
        "expected '[skills]' diagnostic on stderr, got: {stderr}"
    );
}

#[test]
// `commands list` includes built-ins and any custom command files.
fn commands_list_works() {
    let tmp = TempDir::new().unwrap();
    let cmds = tmp.path().join(".agent").join("commands");
    std::fs::create_dir_all(&cmds).unwrap();
    std::fs::write(
        cmds.join("review-changes.md"),
        "---\ndescription: review\n---\nBody\n",
    )
    .unwrap();
    let out = Command::new(bin())
        .args(["commands", "list", "--cwd"])
        .arg(tmp.path())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("review-changes"));
    assert!(stdout.contains("/help") || stdout.contains("help"));
}
