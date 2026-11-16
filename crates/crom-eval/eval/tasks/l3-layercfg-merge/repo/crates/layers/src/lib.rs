//! Layer sources and merging.

use confval::{ConfigError, Value};

pub const ENV_PREFIX: &str = "APP_";

/// Overlay `top` onto `base`: keys in `top` win.
pub fn merge(base: &mut Value, top: Value) {
    match (base, top) {
        (Value::Table(b), Value::Table(t)) => {
            for (k, v) in t {
                b.insert(k, v);
            }
        }
        (b, t) => *b = t,
    }
}

/// Build a layer from environment variables. Only variables starting with
/// [`ENV_PREFIX`] are used; `APP_SERVER__PORT` sets `server.port`.
pub fn env_layer<I, K, V>(vars: I) -> Result<Value, ConfigError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut root = Value::table();
    for (k, v) in vars {
        let Some(rest) = k.as_ref().strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let path = rest.to_lowercase().replace('_', ".");
        root.set(&path, Value::Str(v.as_ref().to_string()))?;
    }
    Ok(root)
}

/// Merge layers in order; later layers override earlier ones.
pub fn resolve(layers: Vec<Value>) -> Value {
    let mut out = Value::table();
    for layer in layers {
        merge(&mut out, layer);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_scalar_wins() {
        let a = confval::parse("name = a\nlevel = 1\n").unwrap();
        let b = confval::parse("name = b\n").unwrap();
        let v = resolve(vec![a, b]);
        assert_eq!(v.get("name").and_then(Value::as_str), Some("b"));
        assert_eq!(v.get("level").and_then(Value::as_int), Some(1));
    }

    #[test]
    fn env_prefix_filters() {
        let v = env_layer([("HOME", "/root"), ("APP_NAME", "svc")]).unwrap();
        assert_eq!(v.get("name").and_then(Value::as_str), Some("svc"));
        assert_eq!(v.get("home"), None);
    }
}
