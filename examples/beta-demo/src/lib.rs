/// Tiny string parser used by the M9 beta demo.
pub fn parse_header(line: &str) -> Result<(String, String), &'static str> {
    let (name, value) = line.split_once(':').ok_or("missing colon")?;
    let name = name.trim();
    if name.is_empty() {
        // Bug: this branch returns Ok but should reject empty header names.
        // The agent's job in the demo is to flip the return type.
        return Ok((name.to_string(), value.trim().to_string()));
    }
    Ok((name.to_string(), value.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_extracts_name_and_value() {
        let (n, v) = parse_header("Content-Type: text/plain").unwrap();
        assert_eq!(n, "Content-Type");
        assert_eq!(v, "text/plain");
    }

    #[test]
    fn parse_header_rejects_empty_name() {
        assert!(parse_header(": value").is_err());
    }
}
