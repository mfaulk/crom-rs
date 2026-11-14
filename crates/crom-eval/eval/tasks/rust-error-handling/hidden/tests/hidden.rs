use kvparse::{parse_entry, ParseError};

/// Well-formed lines parse to a trimmed key and signed integer.
#[test]
fn valid_lines_parse() {
    assert_eq!(parse_entry("retries = 3"), Ok(("retries".to_string(), 3)));
    assert_eq!(parse_entry("  delta=-7  "), Ok(("delta".to_string(), -7)));
}

/// A line without `=` reports `MissingSeparator`.
#[test]
fn missing_separator() {
    assert_eq!(parse_entry("retries 3"), Err(ParseError::MissingSeparator));
    assert_eq!(parse_entry(""), Err(ParseError::MissingSeparator));
}

/// An empty or whitespace-only key reports `EmptyKey`, before the value is checked.
#[test]
fn empty_key() {
    assert_eq!(parse_entry("=5"), Err(ParseError::EmptyKey));
    assert_eq!(parse_entry("   = abc"), Err(ParseError::EmptyKey));
}

/// A non-integer value reports `InvalidNumber` carrying the trimmed value text.
#[test]
fn invalid_number() {
    assert_eq!(
        parse_entry("x = abc "),
        Err(ParseError::InvalidNumber("abc".to_string()))
    );
    assert_eq!(
        parse_entry("x ="),
        Err(ParseError::InvalidNumber(String::new()))
    );
    assert_eq!(
        parse_entry("x = 1 = 2"),
        Err(ParseError::InvalidNumber("1 = 2".to_string()))
    );
    assert_eq!(
        parse_entry("x = 99999999999999999999"),
        Err(ParseError::InvalidNumber(
            "99999999999999999999".to_string()
        ))
    );
}

/// `ParseError` is a standard error with a non-empty message, usable with `?`.
#[test]
fn parse_error_is_std_error() {
    fn load(line: &str) -> Result<i64, Box<dyn std::error::Error>> {
        Ok(parse_entry(line)?.1)
    }
    assert_eq!(load("n = 4").unwrap(), 4);
    let err = load("n = four").unwrap_err();
    assert!(!err.to_string().is_empty());
}
