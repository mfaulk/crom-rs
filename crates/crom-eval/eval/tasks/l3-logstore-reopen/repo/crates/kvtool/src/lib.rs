//! A line-oriented command interpreter: `put <k> <v>`, `get <k>`,
//! `del <k>`, `keys`, `compact`.

use logstore::Store;

pub fn run_line(store: &mut Store, line: &str) -> String {
    let mut parts = line.trim().splitn(3, ' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("put"), Some(k), Some(v)) => {
            store.put(k.as_bytes(), v.as_bytes());
            "OK".to_string()
        }
        (Some("get"), Some(k), None) => match store.get(k.as_bytes()) {
            Some(v) => String::from_utf8_lossy(v).into_owned(),
            None => "(nil)".to_string(),
        },
        (Some("del"), Some(k), None) => {
            if store.delete(k.as_bytes()) {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        (Some("keys"), None, None) => store
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        (Some("compact"), None, None) => {
            store.compact();
            "OK".to_string()
        }
        _ => format!("ERR unknown command: {}", line.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session() {
        let mut s = Store::new();
        assert_eq!(run_line(&mut s, "put name alan turing"), "OK");
        assert_eq!(run_line(&mut s, "get name"), "alan turing");
        assert_eq!(run_line(&mut s, "put lang rust"), "OK");
        assert_eq!(run_line(&mut s, "keys"), "lang name");
        assert_eq!(run_line(&mut s, "del lang"), "1");
        assert_eq!(run_line(&mut s, "get lang"), "(nil)");
        assert!(run_line(&mut s, "frobnicate").starts_with("ERR"));
    }
}
