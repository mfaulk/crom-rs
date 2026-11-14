use clock::ManualClock;
use gateway::{Decision, Gateway};

#[test]
fn clients_are_independent() {
    let clock = ManualClock::new(0);
    let mut g = Gateway::new(clock.clone(), 1, 1000);
    assert_eq!(g.check("a"), Decision::Allow { remaining: 0 });
    assert!(matches!(g.check("a"), Decision::Limit { .. }));
    assert_eq!(g.check("b"), Decision::Allow { remaining: 0 });
}

#[test]
fn recovers_after_interval() {
    let clock = ManualClock::new(0);
    let mut g = Gateway::new(clock.clone(), 1, 1000);
    g.check("a");
    clock.advance(1000);
    assert_eq!(g.check("a"), Decision::Allow { remaining: 0 });
}

#[test]
fn retry_after_header_rounds_up() {
    assert_eq!(
        Decision::Limit { retry_after_ms: 1 }.retry_after_header(),
        Some("1".into())
    );
    assert_eq!(
        Decision::Limit {
            retry_after_ms: 2001
        }
        .retry_after_header(),
        Some("3".into())
    );
    assert_eq!(Decision::Allow { remaining: 3 }.retry_after_header(), None);
}

#[test]
fn evicts_idle_clients() {
    let clock = ManualClock::new(0);
    let mut g = Gateway::new(clock.clone(), 5, 100);
    g.check("a");
    clock.advance(500);
    g.check("b");
    assert_eq!(g.evict_idle(300), 1);
    assert_eq!(g.tracked_clients(), 1);
}
