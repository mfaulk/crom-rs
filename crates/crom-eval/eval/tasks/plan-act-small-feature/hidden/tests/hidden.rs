use textutil::{is_palindrome, reverse};

/// Case and non-alphanumeric characters are ignored when testing for a palindrome.
#[test]
fn ignores_case_and_punctuation() {
    assert!(is_palindrome("Was it a car or a cat I saw?"));
    assert!(is_palindrome("No 'x' in Nixon"));
    assert!(is_palindrome("12321"));
    assert!(!is_palindrome("12345"));
    assert!(!is_palindrome("Hello, world"));
}

/// Empty and punctuation-only strings count as palindromes.
#[test]
fn degenerate_inputs() {
    assert!(is_palindrome(""));
    assert!(is_palindrome("!!"));
    assert!(is_palindrome("a"));
}

/// The existing `reverse` function is unchanged.
#[test]
fn reverse_still_works() {
    assert_eq!(reverse("abc"), "cba");
    assert_eq!(reverse(""), "");
}
