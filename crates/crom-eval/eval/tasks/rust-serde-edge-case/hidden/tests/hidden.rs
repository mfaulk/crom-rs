use config::{parse_config, Config};

/// A missing `retries` defaults to 3 regardless of field order or extra whitespace.
#[test]
fn missing_retries_defaults_to_three() {
    let c = parse_config("{ \"host\" : \"db.local\" }").unwrap();
    assert_eq!(
        c,
        Config {
            host: "db.local".into(),
            retries: 3
        }
    );
}

/// An explicit `retries`, including zero, is preserved rather than replaced by the default.
#[test]
fn explicit_retries_preserved() {
    assert_eq!(
        parse_config(r#"{"retries":0,"host":"a"}"#).unwrap().retries,
        0
    );
    assert_eq!(
        parse_config(r#"{"host":"a","retries":9}"#).unwrap().retries,
        9
    );
}

/// `host` stays required and a wrongly typed `retries` is still rejected.
#[test]
fn invalid_input_still_errors() {
    assert!(parse_config(r#"{"retries":1}"#).is_err());
    assert!(parse_config(r#"{"host":"a","retries":"x"}"#).is_err());
}
