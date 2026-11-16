use crate::model::Book;

/// In-memory catalogue plus an activity log.
///
/// API change (v0.2): `get` now returns `Option<&Book>` instead of a cloned
/// `Option<Book>`, `get_mut` was added for in-place updates, and `put` (which
/// wrote a modified copy back) was removed.
#[derive(Debug, Default)]
pub struct Store {
    books: Vec<Book>,
    log: Vec<String>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a book. Panics if the id is already taken.
    pub fn add(&mut self, book: Book) {
        assert!(self.get(book.id).is_none(), "duplicate id {}", book.id);
        self.books.push(book);
    }

    pub fn get(&self, id: u32) -> Option<&Book> {
        self.books.iter().find(|b| b.id == id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Book> {
        self.books.iter_mut().find(|b| b.id == id)
    }

    /// Ids in insertion order.
    pub fn ids(&self) -> Vec<u32> {
        self.books.iter().map(|b| b.id).collect()
    }

    pub fn log(&mut self, entry: String) {
        self.log.push(entry);
    }

    pub fn entries(&self) -> &[String] {
        &self.log
    }
}
