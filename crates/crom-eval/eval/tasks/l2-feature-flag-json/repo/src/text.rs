use crate::model::Event;
use crate::summary::Summary;

/// Human-readable report: one line per event, then a summary line.
pub fn render_text(events: &[Event]) -> String {
    let mut out = String::new();
    for e in events {
        out.push_str(&format!("{:<20} {:>6} ms", e.name, e.duration_ms));
        if !e.tags.is_empty() {
            out.push_str(&format!(" [{}]", e.tags.join(", ")));
        }
        out.push('\n');
    }
    let s = Summary::from_events(events);
    out.push_str(&format!(
        "count={} total={}ms max={}ms mean={}ms\n",
        s.count,
        s.total_ms,
        s.max_ms,
        s.mean_ms()
    ));
    out
}
