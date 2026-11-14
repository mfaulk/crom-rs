use appconfig::AppConfig;

const NO_ENV: [(&str, &str); 0] = [];

#[test]
fn file_overrides_single_keys() {
    let c = AppConfig::load(Some("[server]\nport = 9000\n[db.pool]\nmax = 50\n"), NO_ENV).unwrap();
    assert_eq!(c.port, 9000);
    assert_eq!(c.host, "127.0.0.1");
    assert_eq!(c.read_timeout_secs, 30);
    assert_eq!(c.db_url, "postgres://localhost/app");
    assert_eq!((c.pool_min, c.pool_max), (1, 50));
}

#[test]
fn environment_beats_file_and_defaults() {
    let c = AppConfig::load(
        Some("[server]\nport = 9000\n"),
        [
            ("APP_SERVER__PORT", "9001"),
            ("APP_SERVER__READ_TIMEOUT", "7"),
            ("APP_FEATURES__METRICS", "false"),
            ("APP_DB__POOL__MIN", "2"),
        ],
    )
    .unwrap();
    assert_eq!(c.port, 9001);
    assert_eq!(c.read_timeout_secs, 7);
    assert!(!c.metrics);
    assert_eq!((c.pool_min, c.pool_max), (2, 10));
    assert_eq!(c.host, "127.0.0.1");
}
