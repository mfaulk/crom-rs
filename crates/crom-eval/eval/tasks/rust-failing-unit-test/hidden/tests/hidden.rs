use stats::max_value;

/// The maximum is found whether it sits at the start, middle, or end.
#[test]
fn max_at_any_position() {
    assert_eq!(max_value(&[9, 1, 2]), Some(9));
    assert_eq!(max_value(&[1, 9, 2]), Some(9));
    assert_eq!(max_value(&[1, 2, 9]), Some(9));
}

/// Single elements, duplicates, extremes, and the empty slice are handled.
#[test]
fn edge_values() {
    assert_eq!(max_value(&[42]), Some(42));
    assert_eq!(max_value(&[3, 3, 3]), Some(3));
    assert_eq!(max_value(&[i64::MIN, i64::MAX, 0]), Some(i64::MAX));
    assert_eq!(max_value(&[i64::MIN]), Some(i64::MIN));
    assert_eq!(max_value(&[]), None);
}
