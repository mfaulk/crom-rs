use crate::codec::{decode, encode, CodecError};
use crate::record::Contact;

/// An in-memory list of contacts kept sorted by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressBook {
    contacts: Vec<Contact>,
}

impl AddressBook {
    pub fn from_text(text: &str) -> Result<Self, CodecError> {
        let mut book = Self::default();
        for c in decode(text)? {
            book.insert(c);
        }
        Ok(book)
    }

    pub fn to_text(&self) -> String {
        encode(&self.contacts)
    }

    /// Insert or replace (by exact name), keeping the list sorted.
    pub fn insert(&mut self, contact: Contact) {
        match self
            .contacts
            .binary_search_by(|c| c.name.as_str().cmp(&contact.name))
        {
            Ok(i) => self.contacts[i] = contact,
            Err(i) => self.contacts.insert(i, contact),
        }
    }

    pub fn find(&self, name: &str) -> Option<&Contact> {
        self.contacts.iter().find(|c| c.name == name)
    }

    pub fn contacts(&self) -> &[Contact] {
        &self.contacts
    }
}
