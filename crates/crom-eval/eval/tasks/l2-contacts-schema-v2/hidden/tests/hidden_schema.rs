use contacts::{decode, encode, migrate, AddressBook, CodecError, Contact};

#[test]
fn encode_writes_v2_with_header() {
    let ann = Contact::new("Ann", 31).with_email("ann@example.com");
    let bo = Contact::new("Bo", 4);
    assert_eq!(bo.email, None);
    assert_eq!(
        encode(&[ann, bo]),
        "#contacts v2\nAnn|ann@example.com|31\nBo||4\n"
    );
    assert_eq!(encode(&[]), "#contacts v2\n");
}

#[test]
fn v2_escapes_separators() {
    let odd = Contact::new("A|B\\C", 1).with_email("x|y");
    let text = encode(std::slice::from_ref(&odd));
    assert_eq!(text, "#contacts v2\nA\\|B\\\\C|x\\|y|1\n");
    assert_eq!(decode(&text).unwrap(), vec![odd]);
}

#[test]
fn decode_reads_both_versions() {
    let v1 = decode("name=Ann;age=31\n").unwrap();
    assert_eq!(v1, vec![Contact::new("Ann", 31)]);
    let v2 = decode("#contacts v2\nAnn|a@b.c|31\n\nBo||4\n").unwrap();
    assert_eq!(
        v2,
        vec![
            Contact::new("Ann", 31).with_email("a@b.c"),
            Contact::new("Bo", 4)
        ]
    );
    assert_eq!(decode("").unwrap(), vec![]);
    assert_eq!(decode("#contacts v2\n").unwrap(), vec![]);
}

#[test]
fn v2_errors_count_the_header_line() {
    assert_eq!(
        decode("#contacts v2\nAnn|31\n"),
        Err(CodecError {
            line: 2,
            msg: "expected 3 fields".to_string()
        })
    );
    assert_eq!(
        decode("#contacts v2\nAnn||31\nBo||old\n"),
        Err(CodecError {
            line: 3,
            msg: "invalid age".to_string()
        })
    );
}

#[test]
fn migrate_upgrades_v1_and_is_idempotent() {
    let v2 = migrate("name=Zed;age=9\nname=Ann;age=31\n").unwrap();
    assert_eq!(v2, "#contacts v2\nZed||9\nAnn||31\n");
    assert_eq!(migrate(&v2).unwrap(), v2);
    assert!(migrate("name=Ann;age=?\n").is_err());
}

#[test]
fn address_book_persists_emails() {
    let mut book = AddressBook::from_text("name=Bo;age=4\n").unwrap();
    book.insert(Contact::new("Ann", 31).with_email("ann@example.com"));
    let text = book.to_text();
    assert_eq!(text, "#contacts v2\nAnn|ann@example.com|31\nBo||4\n");
    let again = AddressBook::from_text(&text).unwrap();
    assert_eq!(
        again.find("Ann").unwrap().email.as_deref(),
        Some("ann@example.com")
    );
}
