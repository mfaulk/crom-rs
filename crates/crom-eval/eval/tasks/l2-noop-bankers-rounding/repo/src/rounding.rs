/// Convert mills (1/10 cent) to cents, rounding half to even (banker's
/// rounding) per the finance audit policy in README.md.
pub fn mills_to_cents(mills: i64) -> i64 {
    let q = mills.div_euclid(10);
    let r = mills.rem_euclid(10);
    match r {
        0..=4 => q,
        6..=9 => q + 1,
        // Exactly half: round to the even neighbour.
        _ => {
            if q % 2 == 0 {
                q
            } else {
                q + 1
            }
        }
    }
}

#[cfg(test)]
mod unit {
    use super::mills_to_cents;

    #[test]
    fn rounds_non_halves_to_nearest() {
        assert_eq!(mills_to_cents(0), 0);
        assert_eq!(mills_to_cents(14), 1);
        assert_eq!(mills_to_cents(16), 2);
        assert_eq!(mills_to_cents(-16), -2);
    }

    #[test]
    fn halves_go_to_even() {
        assert_eq!(mills_to_cents(25), 2);
        assert_eq!(mills_to_cents(35), 4);
        assert_eq!(mills_to_cents(-25), -2);
    }
}
