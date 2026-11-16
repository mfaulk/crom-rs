/// A catalogued book.
///
/// Deliberately not `Clone`: books carry their full loan history, and copying
/// them on every lookup was the main allocation hotspot.
#[derive(Debug, PartialEq, Eq)]
pub struct Book {
    pub id: u32,
    pub title: String,
    pub author: String,
    /// Who currently has the book, if anyone.
    pub borrower: Option<String>,
    /// Everyone who has ever borrowed the book, oldest first.
    pub history: Vec<String>,
}

impl Book {
    pub fn new(id: u32, title: &str, author: &str) -> Self {
        Self {
            id,
            title: title.to_string(),
            author: author.to_string(),
            borrower: None,
            history: Vec::new(),
        }
    }
}
