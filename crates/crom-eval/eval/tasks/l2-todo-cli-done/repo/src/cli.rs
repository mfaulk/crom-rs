use std::io::Write;
use std::path::PathBuf;

use crate::commands::{self, Failure};

const DEFAULT_FILE: &str = "todo.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Add(String),
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub file: PathBuf,
    pub command: Command,
}

/// Parse arguments (without the program name). `--file <path>` may appear
/// before the subcommand.
pub fn parse(args: &[String]) -> Result<Invocation, Failure> {
    let mut file = PathBuf::from(DEFAULT_FILE);
    let mut rest = args;
    while let Some(first) = rest.first() {
        if first != "--file" {
            break;
        }
        let Some(path) = rest.get(1) else {
            return Err(Failure::usage("--file needs a path"));
        };
        file = PathBuf::from(path);
        rest = &rest[2..];
    }
    let Some((sub, tail)) = rest.split_first() else {
        return Err(Failure::usage("missing subcommand"));
    };
    let command = match sub.as_str() {
        "add" => {
            if tail.is_empty() {
                return Err(Failure::usage("usage: todo add <text...>"));
            }
            Command::Add(tail.join(" "))
        }
        "list" => Command::List,
        other => return Err(Failure::usage(format!("unknown subcommand '{other}'"))),
    };
    Ok(Invocation { file, command })
}

/// Run the CLI and return the process exit code.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = parse(args).and_then(|inv| match inv.command {
        Command::Add(text) => commands::add(&inv.file, &text, out),
        Command::List => commands::list(&inv.file, out),
    });
    match result {
        Ok(()) => 0,
        Err(f) => {
            let _ = writeln!(err, "error: {}", f.message);
            f.code
        }
    }
}
