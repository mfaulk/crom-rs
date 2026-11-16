#[test]
fn base_2() {
    assert_eq!(ron::from_str("-0b101010i32"), Ok(-0b101010i32));
    assert_eq!(ron::from_str("0b101010u32"), Ok(0b101010u32));
}

#[test]
fn base_8() {
    assert_eq!(ron::from_str("-0o52i32"), Ok(-0o52i32));
    assert_eq!(ron::from_str("0o52u32"), Ok(0o52u32));
}

#[test]
fn base_10() {
    assert_eq!(ron::from_str("-42i32"), Ok(-42i32));
    assert_eq!(ron::from_str("42u32"), Ok(42u32));
}

#[test]
fn base_16() {
    assert_eq!(ron::from_str("-0x2Ai32"), Ok(-0x2Ai32));
    assert_eq!(ron::from_str("0x2Au32"), Ok(0x2Au32));
}
