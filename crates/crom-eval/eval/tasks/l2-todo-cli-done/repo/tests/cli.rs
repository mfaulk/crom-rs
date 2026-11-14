use std::path::PathBuf;
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("todo-cli-{}-{name}.txt", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn todo(file: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_todo"))
        .arg("--file")
        .arg(file)
        .args(args)
        .output()
        .expect("spawn todo")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn add_then_list() {
    let f = scratch("add_then_list");
    assert_eq!(stdout(&todo(&f, &["add", "buy", "milk"])), "added 1\n");
    assert_eq!(stdout(&todo(&f, &["add", "call mom"])), "added 2\n");
    let out = todo(&f, &["list"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "[ ] 1 buy milk\n[ ] 2 call mom\n");
    let _ = std::fs::remove_file(&f);
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let f = scratch("unknown_subcommand");
    let out = todo(&f, &["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "error: unknown subcommand 'frobnicate'\n"
    );
}
