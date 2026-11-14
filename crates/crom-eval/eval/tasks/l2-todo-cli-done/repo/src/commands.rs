use std::io::Write;
use std::path::Path;

use crate::store::List;

/// A command failure: the message (without the `error: ` prefix) and the
/// process exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: i32,
    pub message: String,
}

impl Failure {
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }
}

fn load(path: &Path) -> Result<List, Failure> {
    List::load(path).map_err(|e| Failure::runtime(format!("{}: {e}", path.display())))
}

fn save(list: &List, path: &Path) -> Result<(), Failure> {
    list.save(path)
        .map_err(|e| Failure::runtime(format!("{}: {e}", path.display())))
}

/// `add`: prints `added <id>`.
pub fn add(path: &Path, text: &str, out: &mut dyn Write) -> Result<(), Failure> {
    let mut list = load(path)?;
    let id = list.add(text);
    save(&list, path)?;
    writeln!(out, "added {id}").map_err(|e| Failure::runtime(e.to_string()))
}

/// `list`: prints `[ ] <id> <text>` or `[x] <id> <text>` per item.
pub fn list(path: &Path, out: &mut dyn Write) -> Result<(), Failure> {
    let list = load(path)?;
    for i in &list.items {
        let mark = if i.done { 'x' } else { ' ' };
        writeln!(out, "[{mark}] {} {}", i.id, i.text)
            .map_err(|e| Failure::runtime(e.to_string()))?;
    }
    Ok(())
}
