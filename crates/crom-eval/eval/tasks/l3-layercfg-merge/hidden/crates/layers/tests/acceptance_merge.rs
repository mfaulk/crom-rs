use confval::Value;
use layers::{env_layer, merge, resolve};

#[test]
fn nested_tables_merge_recursively() {
    let a = confval::parse("[a.b]\nx = 1\ny = 2\n[a]\nkeep = true\n").unwrap();
    let b = confval::parse("[a.b]\ny = 3\nz = 4\n").unwrap();
    let v = resolve(vec![a, b]);
    assert_eq!(v.get("a.b.x").and_then(Value::as_int), Some(1));
    assert_eq!(v.get("a.b.y").and_then(Value::as_int), Some(3));
    assert_eq!(v.get("a.b.z").and_then(Value::as_int), Some(4));
    assert_eq!(v.get("a.keep").and_then(Value::as_bool), Some(true));
}

#[test]
fn type_mismatch_later_wins() {
    let mut base = confval::parse("[a]\nx = 1\n").unwrap();
    merge(&mut base, confval::parse("a = flat\n").unwrap());
    assert_eq!(base.get("a").and_then(Value::as_str), Some("flat"));
    let mut base = confval::parse("a = flat\n").unwrap();
    merge(&mut base, confval::parse("[a]\nx = 1\n").unwrap());
    assert_eq!(base.get("a.x").and_then(Value::as_int), Some(1));
}

#[test]
fn env_names_map_to_nested_snake_case_keys() {
    let v = env_layer([
        ("APP_SERVER__PORT", "9001"),
        ("APP_SERVER__READ_TIMEOUT", "5"),
        ("APP_DB__POOL__MAX", "3"),
        ("APP_FEATURES__METRICS", "false"),
        ("APP_LOG_LEVEL", "debug"),
        ("PATH", "/bin"),
    ])
    .unwrap();
    assert_eq!(v.get("server.port"), Some(&Value::Int(9001)));
    assert_eq!(v.get("server.read_timeout"), Some(&Value::Int(5)));
    assert_eq!(v.get("db.pool.max"), Some(&Value::Int(3)));
    assert_eq!(v.get("features.metrics"), Some(&Value::Bool(false)));
    assert_eq!(v.get("log_level"), Some(&Value::Str("debug".into())));
    assert_eq!(v.get("path"), None);
}
