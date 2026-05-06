//! Tiny library used by the plugin demo so the demo plugin has something
//! plausible to review.

/// Greet a name. Returns the canonical greeting.
pub fn hello(name: &str) -> String {
    format!("hello, {name}!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // hello() returns the canonical greeting form.
    fn greets() {
        assert_eq!(hello("world"), "hello, world!");
    }
}
