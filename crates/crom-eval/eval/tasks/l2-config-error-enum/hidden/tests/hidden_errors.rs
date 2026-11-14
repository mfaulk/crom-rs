use iniconf::{parse, Config, ConfigError, ServerSettings};

fn is_std_error<E: std::error::Error + Send + Sync + 'static>(_: &E) {}

fn boxed(text: &str) -> Result<u16, Box<dyn std::error::Error>> {
    let cfg = parse(text)?;
    Ok(cfg.get_u16("port")?)
}

#[test]
fn syntax_errors_carry_line_numbers() {
    let err = parse("a = 1\n\n  no equals sign\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::Syntax {
            line: 3,
            msg: "expected 'key = value'".to_string()
        }
    );
    assert_eq!(err.to_string(), "line 3: expected 'key = value'");
    is_std_error(&err);

    assert_eq!(
        parse(" = 4").unwrap_err(),
        ConfigError::Syntax {
            line: 1,
            msg: "empty key".to_string()
        }
    );
}

#[test]
fn missing_and_invalid_are_distinct_variants() {
    let cfg: Config = parse("port = abc\nflag = yes\n").unwrap();
    assert_eq!(
        cfg.require("nope"),
        Err(ConfigError::Missing {
            key: "nope".to_string()
        })
    );
    let err = cfg.get_u16("port").unwrap_err();
    assert_eq!(
        err,
        ConfigError::Invalid {
            key: "port".to_string(),
            value: "abc".to_string(),
            expected: "u16"
        }
    );
    assert_eq!(
        err.to_string(),
        "invalid value 'abc' for 'port': expected u16"
    );
    assert_eq!(
        cfg.get_bool("flag").unwrap_err().to_string(),
        "invalid value 'yes' for 'flag': expected bool"
    );
    assert_eq!(cfg.get_u16_or("absent", 7), Ok(7));
    assert!(matches!(
        cfg.get_u16_or("port", 7),
        Err(ConfigError::Invalid { .. })
    ));
}

#[test]
fn settings_propagate_typed_errors() {
    let err = ServerSettings::load("[server]\nhost = h\nport = 1\n").unwrap_err();
    assert_eq!(
        err,
        ConfigError::Missing {
            key: "server.tls".to_string()
        }
    );
    let err = ServerSettings::load("[server\n").unwrap_err();
    assert!(matches!(err, ConfigError::Syntax { line: 1, .. }));
}

#[test]
fn converts_into_boxed_error() {
    assert_eq!(boxed("port = 80").unwrap(), 80);
    let err = boxed("port = -1").unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid value '-1' for 'port': expected u16"
    );
}
