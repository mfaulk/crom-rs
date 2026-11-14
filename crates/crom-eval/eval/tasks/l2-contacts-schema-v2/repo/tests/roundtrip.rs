use contacts::{decode, encode, AddressBook, CodecError, Contact};

#[test]
fn decodes_v1() {
    let got = decode("name=Ann;age=31\n\nname=Bo;age=4\n").unwrap();
    assert_eq!(got, vec![Contact::new("Ann", 31), Contact::new("Bo", 4)]);
}

#[test]
fn encodes_v1() {
    let text = encode(&[Contact::new("Ann", 31)]);
    assert_eq!(text, "name=Ann;age=31\n");
}

#[test]
fn decode_errors_have_line_numbers() {
    assert_eq!(
        decode("name=Ann;age=31\nname=Bo;age=x\n"),
        Err(CodecError {
            line: 2,
            msg: "invalid age".to_string()
        })
    );
}

#[test]
fn book_round_trip_sorts_by_name() {
    let book = AddressBook::from_text("name=Zed;age=9\nname=Ann;age=31\n").unwrap();
    let names: Vec<&str> = book.contacts().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Ann", "Zed"]);
    let again = AddressBook::from_text(&book.to_text()).unwrap();
    assert_eq!(again, book);
}
