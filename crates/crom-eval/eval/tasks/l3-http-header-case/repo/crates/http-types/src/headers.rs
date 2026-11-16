//! An ordered multi-map of header fields.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// First value for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// All values for `name`, in insertion order.
    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Add a value, keeping existing values for the same name.
    pub fn append(&mut self, name: &str, value: &str) {
        self.entries.push((name.to_string(), value.to_string()));
    }

    /// Replace all values for `name` with a single value.
    pub fn insert(&mut self, name: &str, value: &str) {
        self.remove(name);
        self.append(name, value);
    }

    /// Remove every value for `name`; returns how many were removed.
    pub fn remove(&mut self, name: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|(n, _)| n != name);
        before - self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `(name, value)` pairs in insertion order, with names as given.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_replaces_append_accumulates() {
        let mut h = Headers::new();
        h.append("Vary", "Accept");
        h.append("Vary", "Origin");
        assert_eq!(h.get_all("Vary"), ["Accept", "Origin"]);
        h.insert("Vary", "*");
        assert_eq!(h.get_all("Vary"), ["*"]);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn remove_counts() {
        let mut h = Headers::new();
        h.append("X-A", "1");
        h.append("X-A", "2");
        h.append("X-B", "3");
        assert_eq!(h.remove("X-A"), 2);
        assert_eq!(h.iter().collect::<Vec<_>>(), [("X-B", "3")]);
    }
}
