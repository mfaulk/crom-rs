use library::{checkout, most_borrowed, return_book, Book, LoanError, Store};

fn store() -> Store {
    let mut s = Store::new();
    s.add(Book::new(1, "Dune", "Herbert"));
    s.add(Book::new(2, "Emma", "Austen"));
    s
}

#[test]
fn checkout_and_return_round_trip() {
    let mut s = store();
    checkout(&mut s, 1, "ana").unwrap();
    assert_eq!(s.get(1).unwrap().borrower.as_deref(), Some("ana"));
    assert_eq!(return_book(&mut s, 1), Ok("ana".to_string()));
    assert_eq!(s.get(1).unwrap().borrower, None);
}

#[test]
fn double_checkout_is_rejected() {
    let mut s = store();
    checkout(&mut s, 2, "ana").unwrap();
    assert_eq!(
        checkout(&mut s, 2, "bo"),
        Err(LoanError::AlreadyBorrowed {
            id: 2,
            by: "ana".to_string()
        })
    );
}

#[test]
fn most_borrowed_by_history() {
    let mut s = store();
    checkout(&mut s, 2, "ana").unwrap();
    assert_eq!(most_borrowed(&s).map(|b| b.id), Some(2));
}
