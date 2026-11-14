use metrics::{render_text, Event, Summary};

fn events() -> Vec<Event> {
    vec![
        Event::new("db.query", 12).with_tag("db"),
        Event::new("http.get", 30),
        Event::new("db.query", 18).with_tag("db").with_tag("slow"),
    ]
}

#[test]
fn summary_stats() {
    let s = Summary::from_events(&events());
    assert_eq!(
        (s.count, s.total_ms, s.max_ms, s.mean_ms()),
        (3, 60, 30, 20)
    );
    assert_eq!(Summary::from_events(&[]).mean_ms(), 0);
}

#[test]
fn text_report_ends_with_summary() {
    let text = render_text(&events());
    assert_eq!(text.lines().count(), 4);
    assert!(text.contains("[db, slow]"));
    assert!(text.ends_with("count=3 total=60ms max=30ms mean=20ms\n"));
}
