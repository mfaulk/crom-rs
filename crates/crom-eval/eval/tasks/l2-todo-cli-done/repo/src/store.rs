//! On-disk format: one item per line, `<id>\t<0|1>\t<text>`, where the middle
//! column is 1 when the item is done. A missing file is an empty list.

use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: u32,
    pub done: bool,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct List {
    pub items: Vec<Item>,
}

impl List {
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let mut items = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let mut parts = line.splitn(3, '\t');
            let (Some(id), Some(done), Some(text)) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(corrupt(n));
            };
            items.push(Item {
                id: id.parse().map_err(|_| corrupt(n))?,
                done: done == "1",
                text: text.to_string(),
            });
        }
        Ok(Self { items })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut out = String::new();
        for i in &self.items {
            out.push_str(&format!("{}\t{}\t{}\n", i.id, u8::from(i.done), i.text));
        }
        fs::write(path, out)
    }

    /// Append a new item with the next free id and return that id.
    pub fn add(&mut self, text: &str) -> u32 {
        let id = self.items.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        self.items.push(Item {
            id,
            done: false,
            text: text.to_string(),
        });
        id
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }
}

fn corrupt(line: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupt line {}", line + 1),
    )
}
