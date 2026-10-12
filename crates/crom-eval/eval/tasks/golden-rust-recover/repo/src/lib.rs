/// Add two numbers.
///
/// There is a bug here: it subtracts instead of adding.
pub fn add(a: i64, b: i64) -> i64 {
    a - b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(add(10, 20), 30);
    }
}
