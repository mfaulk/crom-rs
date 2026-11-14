use ledger::{mills_to_cents, split, Invoice};

#[test]
fn rounding_is_half_to_even() {
    let cases = [
        (5, 0),
        (15, 2),
        (25, 2),
        (35, 4),
        (45, 4),
        (-5, 0),
        (-15, -2),
        (-25, -2),
        (24, 2),
        (26, 3),
    ];
    for (mills, cents) in cases {
        assert_eq!(mills_to_cents(mills), cents, "{mills} mills");
    }
}

#[test]
fn split_gives_leftover_to_first_payers() {
    assert_eq!(split(1000, 3), [334, 333, 333]);
    assert_eq!(split(10, 4), [3, 3, 2, 2]);
    assert_eq!(split(5, 1), [5]);
    assert_eq!(split(2, 5), [1, 1, 0, 0, 0]);
}

#[test]
fn invoice_rounds_each_line_once() {
    let mut inv = Invoice::default();
    inv.add("a", 25, 1).add("b", 5, 3).add("c", 333, 3);
    // 25 -> 2, 15 -> 2, 999 -> 100
    let totals: Vec<i64> = inv.lines.iter().map(|l| l.total_cents()).collect();
    assert_eq!(totals, [2, 2, 100]);
    assert_eq!(inv.total_cents(), 104);
}
