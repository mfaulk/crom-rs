#![cfg(feature = "json")]

use metrics::{events_to_json, to_json, Event, Summary};

#[test]
fn event_objects_include_duration_and_tags() {
    let events = vec![
        Event::new("db.query", 12).with_tag("db"),
        Event::new("http.get", 30),
    ];
    assert_eq!(
        events_to_json(&events),
        r#"[{"name":"db.query","duration_ms":12,"tags":["db"]},{"name":"http.get","duration_ms":30,"tags":[]}]"#
    );
    assert_eq!(events_to_json(&[]), "[]");
}

#[test]
fn strings_are_escaped() {
    let e = Event::new("say \"hi\"\\now\n", 1).with_tag("tab\there");
    assert_eq!(
        events_to_json(&[e]),
        r#"[{"name":"say \"hi\"\\now\n","duration_ms":1,"tags":["tab\there"]}]"#
    );
    let ctl = Event::new("bell\u{7}", 2);
    assert_eq!(
        events_to_json(&[ctl]),
        r#"[{"name":"bell\u0007","duration_ms":2,"tags":[]}]"#
    );
}

#[test]
fn summary_object() {
    let s = Summary::from_events(&[Event::new("a", 10), Event::new("b", 25)]);
    assert_eq!(
        to_json(&s),
        r#"{"count":2,"total_ms":35,"max_ms":25,"mean_ms":17}"#
    );
    assert_eq!(
        to_json(&Summary::default()),
        r#"{"count":0,"total_ms":0,"max_ms":0,"mean_ms":0}"#
    );
}
