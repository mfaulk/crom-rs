//! The service's typed configuration.

use std::fmt;

use confval::{ConfigError, Value};

pub const DEFAULTS: &str = "\
[server]
host = 127.0.0.1
port = 8080
read_timeout = 30

[db]
url = postgres://localhost/app

[db.pool]
min = 1
max = 10

[features]
metrics = true
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub read_timeout_secs: u32,
    pub db_url: String,
    pub pool_min: u32,
    pub pool_max: u32,
    pub metrics: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppConfigError {
    Config(ConfigError),
    Missing(&'static str),
    Invalid { key: &'static str, reason: String },
}

impl fmt::Display for AppConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppConfigError::Config(e) => write!(f, "{e}"),
            AppConfigError::Missing(k) => write!(f, "missing required key `{k}`"),
            AppConfigError::Invalid { key, reason } => write!(f, "invalid `{key}`: {reason}"),
        }
    }
}

impl std::error::Error for AppConfigError {}

impl From<ConfigError> for AppConfigError {
    fn from(e: ConfigError) -> Self {
        AppConfigError::Config(e)
    }
}

fn str_key(v: &Value, key: &'static str) -> Result<String, AppConfigError> {
    match v.get(key) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(other) => Err(AppConfigError::Invalid {
            key,
            reason: format!("expected a string, found {other:?}"),
        }),
        None => Err(AppConfigError::Missing(key)),
    }
}

fn int_key(v: &Value, key: &'static str, max: i64) -> Result<i64, AppConfigError> {
    match v.get(key) {
        Some(Value::Int(i)) if (0..=max).contains(i) => Ok(*i),
        Some(Value::Int(i)) => Err(AppConfigError::Invalid {
            key,
            reason: format!("{i} is out of range 0..={max}"),
        }),
        Some(other) => Err(AppConfigError::Invalid {
            key,
            reason: format!("expected an integer, found {other:?}"),
        }),
        None => Err(AppConfigError::Missing(key)),
    }
}

fn bool_key(v: &Value, key: &'static str) -> Result<bool, AppConfigError> {
    match v.get(key) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(AppConfigError::Invalid {
            key,
            reason: format!("expected a boolean, found {other:?}"),
        }),
        None => Err(AppConfigError::Missing(key)),
    }
}

impl AppConfig {
    pub fn from_value(v: &Value) -> Result<AppConfig, AppConfigError> {
        let cfg = AppConfig {
            host: str_key(v, "server.host")?,
            port: int_key(v, "server.port", u16::MAX as i64)? as u16,
            read_timeout_secs: int_key(v, "server.read_timeout", u32::MAX as i64)? as u32,
            db_url: str_key(v, "db.url")?,
            pool_min: int_key(v, "db.pool.min", u32::MAX as i64)? as u32,
            pool_max: int_key(v, "db.pool.max", u32::MAX as i64)? as u32,
            metrics: bool_key(v, "features.metrics")?,
        };
        if cfg.pool_min > cfg.pool_max {
            return Err(AppConfigError::Invalid {
                key: "db.pool.min",
                reason: format!("{} exceeds db.pool.max ({})", cfg.pool_min, cfg.pool_max),
            });
        }
        Ok(cfg)
    }

    /// Defaults, then `file` (if any), then `env`.
    pub fn load<I, K, V>(file: Option<&str>, env: I) -> Result<AppConfig, AppConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut stack = vec![confval::parse(DEFAULTS)?];
        if let Some(src) = file {
            stack.push(confval::parse(src)?);
        }
        stack.push(layers::env_layer(env)?);
        AppConfig::from_value(&layers::resolve(stack))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_ENV: [(&str, &str); 0] = [];

    #[test]
    fn defaults_only() {
        let c = AppConfig::load(None, NO_ENV).unwrap();
        assert_eq!(c.port, 8080);
        assert_eq!(c.pool_max, 10);
        assert!(c.metrics);
    }

    #[test]
    fn file_can_replace_whole_sections() {
        let file = "[server]\nhost = 0.0.0.0\nport = 9000\nread_timeout = 5\n";
        let c = AppConfig::load(Some(file), NO_ENV).unwrap();
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 9000);
        assert_eq!(c.db_url, "postgres://localhost/app");
    }

    #[test]
    fn rejects_bad_pool() {
        let file = "[db]\nurl = postgres://db/app\n[db.pool]\nmin = 20\nmax = 10\n";
        let err = AppConfig::load(Some(file), NO_ENV).unwrap_err();
        assert!(matches!(
            err,
            AppConfigError::Invalid {
                key: "db.pool.min",
                ..
            }
        ));
    }
}
