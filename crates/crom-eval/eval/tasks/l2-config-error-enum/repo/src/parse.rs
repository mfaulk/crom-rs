use std::collections::BTreeMap;

/// Parsed configuration: a flat map of `section.key` (or bare `key`) to raw
/// string values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    values: BTreeMap<String, String>,
}

impl Config {
    pub fn raw(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

/// Parse INI-style text. Errors are reported as `line N: <message>` with
/// 1-based line numbers.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut values = BTreeMap::new();
    let mut section: Option<String> = None;
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let Some(name) = rest.strip_suffix(']') else {
                return Err(format!("line {line_no}: unterminated section header"));
            };
            section = Some(name.trim().to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {line_no}: expected 'key = value'"));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("line {line_no}: empty key"));
        }
        let full = match &section {
            Some(s) => format!("{s}.{key}"),
            None => key.to_string(),
        };
        values.insert(full, value.trim().to_string());
    }
    Ok(Config { values })
}
