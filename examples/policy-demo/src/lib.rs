/// Parse `KEY=VALUE` from a single line, trimming whitespace around both
/// sides and dropping blank lines or comments.
pub fn parse_kv(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let k = k.trim();
    let v = v.trim();
    if k.is_empty() {
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

/// Parse a multi-line config blob into a Vec of (key, value) pairs.
pub fn parse_config(text: &str) -> Vec<(String, String)> {
    text.lines().filter_map(parse_kv).collect()
}
