//! Storage format (v1): one contact per line, `name=<name>;age=<age>`.
//! Blank lines are ignored. Names may not contain `;` or newlines.

use std::fmt;

use crate::record::Contact;

/// A decode failure at a 1-based line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for CodecError {}

fn err(line: usize, msg: &str) -> CodecError {
    CodecError {
        line,
        msg: msg.to_string(),
    }
}

fn decode_v1_line(line_no: usize, line: &str) -> Result<Contact, CodecError> {
    let mut name = None;
    let mut age = None;
    for field in line.split(';') {
        let Some((key, value)) = field.split_once('=') else {
            return Err(err(line_no, "expected key=value"));
        };
        match key {
            "name" => name = Some(value.to_string()),
            "age" => age = Some(value.parse().map_err(|_| err(line_no, "invalid age"))?),
            _ => return Err(err(line_no, "unknown field")),
        }
    }
    match (name, age) {
        (Some(name), Some(age)) => Ok(Contact { name, age }),
        _ => Err(err(line_no, "missing field")),
    }
}

pub fn decode(text: &str) -> Result<Vec<Contact>, CodecError> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| decode_v1_line(i + 1, l))
        .collect()
}

pub fn encode(contacts: &[Contact]) -> String {
    contacts
        .iter()
        .map(|c| format!("name={};age={}\n", c.name, c.age))
        .collect()
}
