use csvread::{parse, write, CsvError};

#[test]
fn quoted_commas_and_escaped_quotes() {
    let r = parse("name,quote\n\"Smith, John\",\"say \"\"hi\"\"\"\n").unwrap();
    assert_eq!(
        r,
        vec![vec!["name", "quote"], vec!["Smith, John", "say \"hi\""]]
    );
}

#[test]
fn embedded_line_breaks() {
    let r = parse("a,b\n\"line1\nline2\",z\n\"x\r\ny\",w\n").unwrap();
    assert_eq!(r[1], ["line1\nline2", "z"]);
    assert_eq!(r[2], ["x\r\ny", "w"]);
}

#[test]
fn crlf_records_and_unquoted_whitespace() {
    let r = parse("a,b\r\n 1 ,2\r\n\r\n3,\"\"\r\n").unwrap();
    assert_eq!(r, vec![vec!["a", "b"], vec![" 1 ", "2"], vec!["3", ""]]);
}

#[test]
fn last_record_without_newline() {
    let r = parse("a,\"b\"").unwrap();
    assert_eq!(r, vec![vec!["a", "b"]]);
}

#[test]
fn roundtrip_through_write() {
    let records = vec![
        vec!["plain".to_string(), "with, comma".to_string()],
        vec!["quote \"q\"".to_string(), "multi\nline".to_string()],
        vec!["".to_string(), "crlf\r\nfield".to_string()],
    ];
    assert_eq!(parse(&write(&records)).unwrap(), records);
}

#[test]
fn unterminated_quote_reports_start_line() {
    assert_eq!(
        parse("a,\"open\nmore"),
        Err(CsvError::UnterminatedQuote { line: 1 })
    );
    assert_eq!(
        parse("h\n\"x\ny\"\n\"bad\n"),
        Err(CsvError::UnterminatedQuote { line: 4 })
    );
}

#[test]
fn junk_after_closing_quote() {
    assert_eq!(
        parse("ok\n\"a\nb\"c,d\n"),
        Err(CsvError::UnexpectedChar { line: 3, ch: 'c' })
    );
}
