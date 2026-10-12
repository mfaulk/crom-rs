use serde::Deserialize;

/// Service configuration parsed from JSON.
///
/// Bug: `retries` is required, so JSON that omits it fails to deserialize. It
/// should default to 3 when absent.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub retries: u32,
}

/// Parse a [`Config`] from a JSON string.
pub fn parse_config(json: &str) -> Result<Config, String> {
    serde_json::from_str(json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_retries() {
        let c = parse_config(r#"{"host":"a","retries":5}"#).unwrap();
        assert_eq!(c, Config { host: "a".into(), retries: 5 });
    }

    #[test]
    fn defaults_retries_when_missing() {
        let c = parse_config(r#"{"host":"a"}"#).unwrap();
        assert_eq!(c.retries, 3);
    }
}
