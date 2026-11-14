//! A small lending library.

pub mod loans;
pub mod model;
pub mod report;
pub mod store;

pub use loans::{checkout, return_book, LoanError};
pub use model::Book;
pub use report::{most_borrowed, rename_author};
pub use store::Store;
