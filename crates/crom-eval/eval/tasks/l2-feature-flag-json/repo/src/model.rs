/// One timed operation.
///
/// `duration_ms` was called `value` before v0.1; it was renamed to make the
/// unit explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub duration_ms: u64,
    pub tags: Vec<String>,
}

impl Event {
    pub fn new(name: &str, duration_ms: u64) -> Self {
        Self {
            name: name.to_string(),
            duration_ms,
            tags: Vec::new(),
        }
    }

    pub fn with_tag(mut self, tag: &str) -> Self {
        self.tags.push(tag.to_string());
        self
    }
}
