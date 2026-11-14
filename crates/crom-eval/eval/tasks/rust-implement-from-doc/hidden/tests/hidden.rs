use rle::encode;

/// Runs are counted per maximal run, so a character can reappear later.
#[test]
fn repeated_runs_of_same_char() {
    assert_eq!(encode("aabbaa"), "a2b2a2");
    assert_eq!(encode("abca"), "abca");
}

/// Counts of ten or more are written in full decimal.
#[test]
fn multi_digit_counts() {
    assert_eq!(encode(&"x".repeat(12)), "x12");
    assert_eq!(encode(&"q".repeat(100)), "q100");
}

/// Multi-byte characters and whitespace count as single characters.
#[test]
fn unicode_and_whitespace() {
    assert_eq!(encode("ééé"), "é3");
    assert_eq!(encode("日日本"), "日2本");
    assert_eq!(encode("a  b"), "a 2b");
}

/// Empty and single-character inputs encode trivially.
#[test]
fn trivial_inputs() {
    assert_eq!(encode(""), "");
    assert_eq!(encode("z"), "z");
}
