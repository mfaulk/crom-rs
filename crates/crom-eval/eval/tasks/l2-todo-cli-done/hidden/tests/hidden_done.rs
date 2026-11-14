use std::path::PathBuf;
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("todo-hidden-{}-{name}.txt", std::process::id()));
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

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn done_marks_item_and_persists() {
    let f = scratch("done_marks");
    todo(&f, &["add", "buy milk"]);
    todo(&f, &["add", "call mom"]);
    let out = todo(&f, &["done", "2"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "completed 2: call mom\n");
    assert_eq!(text(&out.stderr), "");
    assert_eq!(
        text(&todo(&f, &["list"]).stdout),
        "[ ] 1 buy milk\n[x] 2 call mom\n"
    );
    let again = todo(&f, &["done", "2"]);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(text(&again.stdout), "already completed 2: call mom\n");
    let _ = std::fs::remove_file(&f);
}

#[test]
fn done_unknown_id_is_runtime_error() {
    let f = scratch("done_unknown");
    todo(&f, &["add", "x"]);
    let out = todo(&f, &["done", "7"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(text(&out.stderr), "error: no item with id 7\n");
    let _ = std::fs::remove_file(&f);
}

#[test]
fn done_bad_arguments_are_usage_errors() {
    let f = scratch("done_usage");
    for args in [&["done"][..], &["done", "abc"], &["done", "1", "2"]] {
        let out = todo(&f, args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}");
        assert_eq!(text(&out.stderr), "error: usage: todo done <id>\n");
    }
}

#[test]
fn existing_commands_still_work() {
    let f = scratch("existing");
    assert_eq!(text(&todo(&f, &["add", "a", "b"]).stdout), "added 1\n");
    assert_eq!(text(&todo(&f, &["list"]).stdout), "[ ] 1 a b\n");
    let _ = std::fs::remove_file(&f);
}
