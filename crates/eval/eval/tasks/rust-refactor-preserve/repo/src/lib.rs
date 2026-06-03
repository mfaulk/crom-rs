const PI: f64 = 3.141_592_653_589_793;

/// Area of a circle. Returns 0.0 for a negative radius.
pub fn area_of_circle(radius: f64) -> f64 {
    if radius < 0.0 {
        return 0.0;
    }
    PI * radius * radius
}

/// Circumference of a circle. Returns 0.0 for a negative radius.
///
/// Note the duplicated PI constant and the duplicated radius validation — a
/// refactor should give these a single source of truth.
pub fn circumference(radius: f64) -> f64 {
    if radius < 0.0 {
        return 0.0;
    }
    2.0 * PI * radius
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas() {
        assert!((area_of_circle(2.0) - 12.566_370_614_359_172).abs() < 1e-9);
        assert_eq!(area_of_circle(-1.0), 0.0);
    }

    #[test]
    fn circumferences() {
        assert!((circumference(1.0) - 6.283_185_307_179_586).abs() < 1e-9);
        assert_eq!(circumference(-1.0), 0.0);
    }
}
