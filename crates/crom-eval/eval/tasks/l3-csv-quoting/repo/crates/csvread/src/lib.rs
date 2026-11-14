//! A small CSV reader.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvError {
    /// A quoted field was still open at end of input. `line` is the 1-based
    /// line on which the field started.
    UnterminatedQuote { line: usize },
    /// Text after a closing quote that is not a delimiter or record end.
    UnexpectedChar { line: usize, ch: char },
}

impl fmt::Display for CsvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CsvError::UnterminatedQuote { line } => {
                write!(f, "line {line}: quoted field is never closed")
            }
            CsvError::UnexpectedChar { line, ch } => {
                write!(f, "line {line}: unexpected `{ch}` after closing quote")
            }
        }
    }
}

impl std::error::Error for CsvError {}

pub type Record = Vec<String>;

/// Parse CSV text into records. Blank lines are skipped.
pub fn parse(input: &str) -> Result<Vec<Record>, CsvError> {
    let mut records = Vec::new();
    for line in input.split('\n') {
        if line.is_empty() {
            continue;
        }
        records.push(
            line.split(',')
                .map(|f| f.trim_matches('"').to_string())
                .collect(),
        );
    }
    Ok(records)
}

/// Quote a field for output if it needs it.
pub fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Serialize records with `\n` line endings.
pub fn write(records: &[Record]) -> String {
    let mut out = String::new();
    for r in records {
        let fields: Vec<String> = r.iter().map(|f| escape(f)).collect();
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_rows() {
        let r = parse("a,b,c\n1,2,3\n").unwrap();
        assert_eq!(r, vec![vec!["a", "b", "c"], vec!["1", "2", "3"]]);
    }

    #[test]
    fn empty_fields_kept() {
        let r = parse("a,,c\n,,\n").unwrap();
        assert_eq!(r[0], ["a", "", "c"]);
        assert_eq!(r[1], ["", "", ""]);
    }

    #[test]
    fn simple_quotes_stripped() {
        let r = parse("\"x\",y\n").unwrap();
        assert_eq!(r[0], ["x", "y"]);
    }

    #[test]
    fn escape_quotes_when_needed() {
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("a,b"), "\"a,b\"");
        assert_eq!(escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }
}
