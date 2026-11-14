use ledger::{AccountKind, Ledger, LedgerError, Posting};
use money::{Currency, Money};

fn usd(s: &str) -> Money {
    Money::parse(s, Currency::Usd).unwrap()
}

fn household() -> Ledger {
    let mut l = Ledger::new(Currency::Usd);
    for name in ["alice", "bob", "carol"] {
        l.open(name, AccountKind::Asset).unwrap();
    }
    l
}

#[test]
fn rejects_unbalanced_entry() {
    let mut l = household();
    let err = l
        .post(
            "oops",
            vec![
                Posting {
                    account: "alice".into(),
                    amount: usd("1.00"),
                },
                Posting {
                    account: "bob".into(),
                    amount: usd("-0.99"),
                },
            ],
        )
        .unwrap_err();
    assert_eq!(
        err,
        LedgerError::Unbalanced {
            memo: "oops".into(),
            residual: 1
        }
    );
}

#[test]
fn even_split_between_two() {
    let mut l = household();
    l.split_expense(
        "groceries",
        "alice",
        usd("40.00"),
        &[("alice", 1), ("bob", 1)],
    )
    .unwrap();
    assert_eq!(l.balance("alice").unwrap(), usd("-20.00"));
    assert_eq!(l.balance("bob").unwrap(), usd("20.00"));
}

#[test]
fn weighted_split() {
    let mut l = household();
    l.split_expense("rent", "carol", usd("900.00"), &[("alice", 2), ("bob", 1)])
        .unwrap();
    assert_eq!(l.balance("alice").unwrap(), usd("600.00"));
    assert_eq!(l.balance("bob").unwrap(), usd("300.00"));
    assert_eq!(l.balance("carol").unwrap(), usd("-900.00"));
}

#[test]
fn unknown_account_rejected() {
    let mut l = household();
    let err = l
        .split_expense("x", "alice", usd("1.00"), &[("dave", 1)])
        .unwrap_err();
    assert_eq!(err, LedgerError::UnknownAccount("dave".into()));
}
