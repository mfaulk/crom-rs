//! A contact list persisted in a line-oriented text format.

pub mod book;
pub mod codec;
pub mod record;

pub use book::AddressBook;
pub use codec::{decode, encode, CodecError};
pub use record::Contact;
