use calendar::{days_in_month, is_leap_year};

/// Century years are leap years only when divisible by 400.
#[test]
fn century_rule() {
    assert!(!is_leap_year(1900));
    assert!(!is_leap_year(2100));
    assert!(is_leap_year(2000));
    assert!(is_leap_year(1600));
}

/// Ordinary and negative (proleptic) years follow the divisible-by-4 rule.
#[test]
fn ordinary_and_negative_years() {
    assert!(is_leap_year(2024));
    assert!(!is_leap_year(2023));
    assert!(is_leap_year(0));
    assert!(is_leap_year(-4));
    assert!(!is_leap_year(-100));
}

/// February has 28 days in 1900 and 29 in 2000; invalid months are `None`.
#[test]
fn february_and_invalid_months() {
    assert_eq!(days_in_month(1900, 2), Some(28));
    assert_eq!(days_in_month(2000, 2), Some(29));
    assert_eq!(days_in_month(2023, 12), Some(31));
    assert_eq!(days_in_month(2023, 0), None);
    assert_eq!(days_in_month(2023, 13), None);
}
