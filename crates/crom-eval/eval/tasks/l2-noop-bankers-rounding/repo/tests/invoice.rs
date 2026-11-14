use ledger::{split, Invoice};

#[test]
fn invoice_total_sums_rounded_lines() {
    let mut inv = Invoice::default();
    inv.add("sticker", 25, 1)
        .add("pen", 1999, 2)
        .add("refund", -15, 1);
    // 2 + 400 + (-2)
    assert_eq!(inv.total_cents(), 400);
}

#[test]
fn split_sums_to_total() {
    for (total, n) in [(1000, 3), (7, 7), (0, 2), (99, 4)] {
        let shares = split(total, n);
        assert_eq!(shares.len(), n);
        assert_eq!(shares.iter().sum::<i64>(), total);
    }
}
