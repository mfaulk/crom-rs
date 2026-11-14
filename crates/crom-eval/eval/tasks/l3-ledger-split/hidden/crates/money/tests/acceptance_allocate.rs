use money::{Currency, Money};

fn minors(parts: &[Money]) -> Vec<i64> {
    parts.iter().map(Money::minor).collect()
}

#[test]
fn split_three_ways_hands_remainder_to_first_parts() {
    let parts = Money::from_minor(10_000, Currency::Usd).split(3).unwrap();
    assert_eq!(minors(&parts), vec![3334, 3333, 3333]);
}

#[test]
fn weighted_allocation_sums_to_total() {
    let parts = Money::from_minor(1001, Currency::Usd)
        .allocate(&[1, 2, 3])
        .unwrap();
    assert_eq!(minors(&parts), vec![167, 334, 500]);
    assert_eq!(parts.iter().map(Money::minor).sum::<i64>(), 1001);
}

#[test]
fn negative_amounts_balance() {
    let parts = Money::from_minor(-100, Currency::Eur).split(3).unwrap();
    assert_eq!(minors(&parts), vec![-34, -33, -33]);
}

#[test]
fn zero_weight_parts_get_nothing() {
    let parts = Money::from_minor(5, Currency::Jpy)
        .allocate(&[0, 1, 1])
        .unwrap();
    assert_eq!(minors(&parts), vec![0, 3, 2]);
}

#[test]
fn many_parts_always_sum() {
    for total in [1i64, 7, 99, 12_345, -12_345] {
        for n in 1..=9 {
            let parts = Money::from_minor(total, Currency::Usd).split(n).unwrap();
            assert_eq!(parts.len(), n);
            assert_eq!(
                parts.iter().map(Money::minor).sum::<i64>(),
                total,
                "{total} / {n}"
            );
        }
    }
}
