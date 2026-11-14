//! Typed accessors on [`Config`].

use crate::parse::Config;

impl Config {
    /// The raw value for `key`, or an error if it is absent.
    pub fn require(&self, key: &str) -> Result<&str, String> {
        self.raw(key).ok_or_else(|| format!("missing key '{key}'"))
    }

    pub fn get_u16(&self, key: &str) -> Result<u16, String> {
        let v = self.require(key)?;
        v.parse::<u16>()
            .map_err(|_| format!("invalid value '{v}' for '{key}': expected u16"))
    }

    /// Accepts `true`/`false` only.
    pub fn get_bool(&self, key: &str) -> Result<bool, String> {
        let v = self.require(key)?;
        match v {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!("invalid value '{v}' for '{key}': expected bool")),
        }
    }

    /// Like [`Config::get_u16`] but returns `default` when the key is absent.
    pub fn get_u16_or(&self, key: &str, default: u16) -> Result<u16, String> {
        if self.raw(key).is_none() {
            return Ok(default);
        }
        self.get_u16(key)
    }
}
