use crate::store::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoanError {
    NotFound(u32),
    AlreadyBorrowed { id: u32, by: String },
    NotBorrowed(u32),
}

/// Lend book `id` to `who`, record it in the book's history, and log
/// `<who> borrowed '<title>'`.
pub fn checkout(store: &mut Store, id: u32, who: &str) -> Result<(), LoanError> {
    let mut book = store.get(id).ok_or(LoanError::NotFound(id))?;
    if let Some(current) = &book.borrower {
        return Err(LoanError::AlreadyBorrowed {
            id,
            by: current.clone(),
        });
    }
    book.borrower = Some(who.to_string());
    book.history.push(who.to_string());
    store.put(book);
    store.log(format!("{who} borrowed '{}'", book.title));
    Ok(())
}

/// Take book `id` back, log `<who> returned '<title>'`, and return who had it.
pub fn return_book(store: &mut Store, id: u32) -> Result<String, LoanError> {
    let mut book = store.get(id).ok_or(LoanError::NotFound(id))?;
    let who = book.borrower.take().ok_or(LoanError::NotBorrowed(id))?;
    store.put(book);
    store.log(format!("{who} returned '{}'", book.title));
    Ok(who)
}
