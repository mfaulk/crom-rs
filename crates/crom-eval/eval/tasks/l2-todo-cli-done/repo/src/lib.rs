//! A file-backed to-do list CLI.
//!
//! ```text
//! todo [--file <path>] add <text...>
//! todo [--file <path>] list
//! ```
//!
//! Exit codes: 0 success, 1 runtime error, 2 usage error. Errors are printed
//! to stderr prefixed with `error: `.

pub mod cli;
pub mod commands;
pub mod store;
