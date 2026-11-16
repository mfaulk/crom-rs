use cleanup::{is_affirmative, total};

/// Affirmative detection is unchanged: case-insensitive first word, empty and whitespace-only input rejected.
#[test]
fn affirmative_behavior_preserved() {
    assert!(is_affirmative("YES"));
    assert!(is_affirmative("  Yes  sir"));
    assert!(!is_affirmative("   "));
    assert!(!is_affirmative("yesterday"));
    assert!(!is_affirmative("no yes"));
}

/// Totals still sum correctly for empty, negative, and Vec-backed inputs.
#[test]
fn total_behavior_preserved() {
    let empty: Vec<i64> = Vec::new();
    let mixed: Vec<i64> = [-5, 5, 10].into();
    let near_max: Vec<i64> = [i64::MAX - 1, 1].into();
    assert_eq!(total(&empty), 0);
    assert_eq!(total(&mixed), 10);
    assert_eq!(total(&near_max), i64::MAX);
}
