//! A fixed-size worker pool that maps a function over string inputs.

pub mod job;
pub mod pipeline;
pub mod worker;

pub use pipeline::{run, run_with_stats, Stats};
