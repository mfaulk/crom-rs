/// Format an amount in cents as dollars, e.g. `1234` -> `"$12.34"` and
/// `-50` -> `"-$0.50"`.
pub fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}${}.{:02}", abs / 100, abs % 100)
}

/// Multiply `cents` by `basis_points / 10_000`, rounding half away from zero.
pub fn apply_basis_points(cents: i64, basis_points: i64) -> i64 {
    let scaled = cents * basis_points;
    let (q, r) = (scaled / 10_000, scaled % 10_000);
    if r.abs() * 2 >= 10_000 {
        q + scaled.signum()
    } else {
        q
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn formats_positive_and_negative() {
        assert_eq!(format_cents(0), "$0.00");
        assert_eq!(format_cents(1234), "$12.34");
        assert_eq!(format_cents(-50), "-$0.50");
    }

    #[test]
    fn basis_points_round_half_up() {
        assert_eq!(apply_basis_points(1000, 800), 80);
        assert_eq!(apply_basis_points(1006, 800), 80); // 80.48
        assert_eq!(apply_basis_points(1007, 800), 81); // 80.56
        assert_eq!(apply_basis_points(25, 2000), 5);
    }
}
