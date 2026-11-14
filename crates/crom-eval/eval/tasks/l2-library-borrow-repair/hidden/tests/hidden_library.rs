use library::{checkout, most_borrowed, rename_author, return_book, Book, LoanError, Store};

fn store() -> Store {
    let mut s = Store::new();
    s.add(Book::new(1, "Dune", "Herbert"));
    s.add(Book::new(2, "Emma", "Austen"));
    s.add(Book::new(3, "Persuasion", "Austen"));
    s
}

#[test]
fn checkout_records_history_and_log() {
    let mut s = store();
    checkout(&mut s, 1, "ana").unwrap();
    return_book(&mut s, 1).unwrap();
    checkout(&mut s, 1, "bo").unwrap();
    let book = s.get(1).unwrap();
    assert_eq!(book.history, ["ana", "bo"]);
    assert_eq!(book.borrower.as_deref(), Some("bo"));
    assert_eq!(
        s.entries(),
        [
            "ana borrowed 'Dune'",
            "ana returned 'Dune'",
            "bo borrowed 'Dune'"
        ]
    );
}

#[test]
fn errors_leave_state_untouched() {
    let mut s = store();
    assert_eq!(checkout(&mut s, 9, "ana"), Err(LoanError::NotFound(9)));
    assert_eq!(return_book(&mut s, 2), Err(LoanError::NotBorrowed(2)));
    assert_eq!(return_book(&mut s, 9), Err(LoanError::NotFound(9)));
    checkout(&mut s, 2, "ana").unwrap();
    assert!(checkout(&mut s, 2, "bo").is_err());
    assert_eq!(s.get(2).unwrap().history, ["ana"]);
    assert_eq!(s.entries(), ["ana borrowed 'Emma'"]);
}

#[test]
fn most_borrowed_returns_a_reference_and_prefers_earliest() {
    let mut s = store();
    assert!(std::ptr::eq(most_borrowed(&s).unwrap(), s.get(1).unwrap()));
    checkout(&mut s, 3, "ana").unwrap();
    return_book(&mut s, 3).unwrap();
    checkout(&mut s, 3, "bo").unwrap();
    checkout(&mut s, 2, "cy").unwrap();
    let best: &Book = most_borrowed(&s).unwrap();
    assert_eq!(best.title, "Persuasion");
    assert!(most_borrowed(&Store::new()).is_none());
}

#[test]
fn rename_author_updates_and_logs() {
    let mut s = store();
    assert_eq!(rename_author(&mut s, "Austen", "J. Austen"), 2);
    assert_eq!(s.get(2).unwrap().author, "J. Austen");
    assert_eq!(s.get(3).unwrap().author, "J. Austen");
    assert_eq!(s.get(1).unwrap().author, "Herbert");
    assert_eq!(
        s.entries(),
        [
            "author of 'Emma' changed to J. Austen",
            "author of 'Persuasion' changed to J. Austen"
        ]
    );
    assert_eq!(rename_author(&mut s, "Nobody", "X"), 0);
}
