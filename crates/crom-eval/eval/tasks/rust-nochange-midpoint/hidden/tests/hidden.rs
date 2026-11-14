use mid::midpoint;

/// Odd sums round down and argument order does not matter.
#[test]
fn rounds_down_symmetrically() {
    assert_eq!(midpoint(3, 4), 3);
    assert_eq!(midpoint(4, 3), 3);
    assert_eq!(midpoint(0, 1), 0);
    assert_eq!(midpoint(0, 0), 0);
}

/// Values near `u32::MAX` do not overflow.
#[test]
fn no_overflow_at_extremes() {
    assert_eq!(midpoint(u32::MAX, u32::MAX), u32::MAX);
    assert_eq!(midpoint(u32::MAX, u32::MAX - 1), u32::MAX - 1);
    assert_eq!(midpoint(0, u32::MAX), u32::MAX / 2);
    assert_eq!(midpoint(u32::MAX - 2, u32::MAX), u32::MAX - 1);
}
