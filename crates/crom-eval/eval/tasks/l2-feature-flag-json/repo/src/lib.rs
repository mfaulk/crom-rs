//! Timing metrics: record events, summarise them, render them.
//!
//! The optional `json` feature adds a dependency-free JSON exporter.

pub mod model;
pub mod summary;
pub mod text;

#[cfg(feature = "json")]
pub mod export;

pub use model::Event;
pub use summary::Summary;
pub use text::render_text;
