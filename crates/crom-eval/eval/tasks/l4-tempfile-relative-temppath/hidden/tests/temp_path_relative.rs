// Owner-written acceptance tests for Stebalien/tempfile#395, taken from the
// issue's reproduction. They call only `TempPath::from_path`, which exists at
// the base commit, so any fix that meets the issue's "Expected" behavior passes.
#![allow(deprecated)]

use std::env;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tempfile::{tempdir, TempPath};

static CWD_LOCK: Mutex<()> = Mutex::new(());

struct CwdGuard {
    old: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.old);
    }
}

fn cwd_guard() -> CwdGuard {
    let lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    CwdGuard {
        old: env::current_dir().unwrap(),
        _lock: lock,
    }
}

/// Dropping a `TempPath` built from a relative path removes the file it named
/// at construction time, not a same-named file in a later working directory.
#[test]
fn drop_removes_file_relative_to_construction_cwd() {
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    let _guard = cwd_guard();

    env::set_current_dir(a.path()).unwrap();
    File::create("victim").unwrap();
    let tp = TempPath::from_path("victim");

    env::set_current_dir(b.path()).unwrap();
    File::create("victim").unwrap();
    drop(tp);

    assert!(!a.path().join("victim").exists(), "a/victim was not removed");
    assert!(b.path().join("victim").exists(), "b/victim was removed");
}

/// `TempPath::close` on a relative path removes the file it named at
/// construction time, not a same-named file in a later working directory.
#[test]
fn close_removes_file_relative_to_construction_cwd() {
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    let _guard = cwd_guard();

    env::set_current_dir(a.path()).unwrap();
    File::create("victim").unwrap();
    let tp = TempPath::from_path("victim");

    env::set_current_dir(b.path()).unwrap();
    File::create("victim").unwrap();
    tp.close().unwrap();

    assert!(!a.path().join("victim").exists(), "a/victim was not removed");
    assert!(b.path().join("victim").exists(), "b/victim was removed");
}
