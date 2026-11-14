use iniconf::{parse, ServerSettings};

const SAMPLE: &str = "\
# demo
name = demo
[server]
host = 0.0.0.0
port = 8080
tls = true
";

#[test]
fn loads_server_settings() {
    let s = ServerSettings::load(SAMPLE).unwrap();
    assert_eq!(s.host, "0.0.0.0");
    assert_eq!(s.port, 8080);
    assert!(s.tls);
    assert_eq!(s.workers, 4);
}

#[test]
fn bare_and_sectioned_keys() {
    let cfg = parse(SAMPLE).unwrap();
    let keys: Vec<&str> = cfg.keys().collect();
    assert_eq!(keys, ["name", "server.host", "server.port", "server.tls"]);
}

#[test]
fn error_messages() {
    let err = ServerSettings::load("[server]\nhost = x\nport = 99999\ntls = true\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid value '99999' for 'server.port': expected u16"
    );
    let err = parse("a = 1\n[oops\n").unwrap_err();
    assert_eq!(err.to_string(), "line 2: unterminated section header");
    let err = ServerSettings::load("[server]\nport = 1\n").unwrap_err();
    assert_eq!(err.to_string(), "missing key 'server.host'");
}
