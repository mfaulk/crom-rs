use ledger::{AccountKind, Ledger};
use money::{Currency, Money};

#[test]
fn three_way_split_posts_and_balances() {
    let mut l = Ledger::new(Currency::Usd);
    for n in ["ann", "ben", "cat"] {
        l.open(n, AccountKind::Asset).unwrap();
    }
    l.split_expense(
        "dinner",
        "ann",
        Money::from_minor(10_000, Currency::Usd),
        &[("ann", 1), ("ben", 1), ("cat", 1)],
    )
    .unwrap();
    assert_eq!(l.balance("ann").unwrap().minor(), 3334 - 10_000);
    assert_eq!(l.balance("ben").unwrap().minor(), 3333);
    assert_eq!(l.balance("cat").unwrap().minor(), 3333);
    let total: i64 = l.balances().unwrap().iter().map(|(_, m)| m.minor()).sum();
    assert_eq!(total, 0);
}
