//! A tiny INI-style configuration loader.
//!
//! ```text
//! # comment
//! name = demo
//! [server]
//! port = 8080
//! tls = true
//! ```
//!
//! Keys inside a section are addressed as `section.key`.

pub mod parse;
pub mod settings;
pub mod typed;

pub use parse::{parse, Config};
pub use settings::ServerSettings;
