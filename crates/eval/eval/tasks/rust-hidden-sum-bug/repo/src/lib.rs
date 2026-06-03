//! A tiny library with a deliberately wrong `add`.

/// Add two integers. (Currently buggy: it subtracts.)
pub fn add(a: i64, b: i64) -> i64 {
    a - b
}
