/// Return true if the input is non-empty and the first word is "yes".
///
/// This compiles and the tests pass, but several clippy lints fire under
/// `-D warnings` (needless_return, len_zero, comparison to a bool literal).
pub fn is_affirmative(input: &str) -> bool {
    if input.len() == 0 {
        return false;
    }
    let first = input.split_whitespace().next().unwrap_or("");
    return first.eq_ignore_ascii_case("yes");
}

/// Sum a slice with an explicit, un-idiomatic loop that clippy flags.
pub fn total(values: &Vec<i64>) -> i64 {
    let mut sum = 0;
    for i in 0..values.len() {
        sum += values[i];
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affirmative() {
        assert!(is_affirmative("yes please"));
        assert!(!is_affirmative(""));
        assert!(!is_affirmative("no"));
    }

    #[test]
    fn totals() {
        assert_eq!(total(&vec![1, 2, 3]), 6);
    }
}
