//! JSON export, compiled only with the `json` feature. Hand-written so the
//! crate stays dependency-free.

use crate::model::Event;
use crate::summary::Summary;

fn escape(s: &str) -> String {
    s.replace('"', "\\\"")
}

/// `{"name":"...","value":N}`
pub fn event_to_json(e: &Event) -> String {
    format!("{{\"name\":\"{}\",\"value\":{}}}", escape(&e.name), e.value)
}

/// A JSON array of [`event_to_json`] objects.
pub fn events_to_json(events: &[Event]) -> String {
    let parts: Vec<String> = events.iter().map(event_to_json).collect();
    format!("[{}]", parts.join(","))
}

// TODO: summary export.
