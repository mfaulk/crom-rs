use geometry::{area_of_circle, circumference};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * b.abs().max(1.0)
}

/// Area matches PI * r^2 for zero, fractional, and large radii.
#[test]
fn area_unchanged() {
    assert_eq!(area_of_circle(0.0), 0.0);
    assert!(close(area_of_circle(0.5), 0.785_398_163_397_448_3));
    assert!(close(area_of_circle(1e6), 3.141_592_653_589_793e12));
}

/// Circumference matches 2 * PI * r for zero, fractional, and large radii.
#[test]
fn circumference_unchanged() {
    assert_eq!(circumference(0.0), 0.0);
    assert!(close(circumference(0.5), 3.141_592_653_589_793));
    assert!(close(circumference(1e6), 6.283_185_307_179_586e6));
}

/// Every negative radius still yields exactly 0.0 from both functions.
#[test]
fn negative_radius_is_zero() {
    for r in [-0.5, -1.0, -1e9, f64::MIN] {
        assert_eq!(area_of_circle(r), 0.0);
        assert_eq!(circumference(r), 0.0);
    }
}
