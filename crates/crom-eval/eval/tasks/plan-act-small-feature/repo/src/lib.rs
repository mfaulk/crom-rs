/// Reverse a string by Unicode scalar values.
pub fn reverse(input: &str) -> String {
    input.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses() {
        assert_eq!(reverse("abc"), "cba");
    }
}
