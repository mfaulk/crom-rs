use crate::model::Book;
use crate::store::Store;

/// The book with the longest loan history (earliest-added wins ties), or
/// `None` for an empty store.
pub fn most_borrowed(store: &Store) -> Option<Book> {
    let mut best: Option<Book> = None;
    for id in store.ids() {
        let book = store.get(id)?;
        if best
            .as_ref()
            .is_none_or(|b| book.history.len() > b.history.len())
        {
            best = Some(book);
        }
    }
    best
}

/// Change the author of every book by `from` to `to`, logging
/// `author of '<title>' changed to <to>` for each. Returns how many changed.
pub fn rename_author(store: &mut Store, from: &str, to: &str) -> usize {
    let mut changed = 0;
    for id in store.ids() {
        let book = store.get(id).expect("id from ids()");
        if book.author != from {
            continue;
        }
        store.get_mut(id).expect("id from ids()").author = to.to_string();
        store.log(format!("author of '{}' changed to {to}", book.title));
        changed += 1;
    }
    changed
}
