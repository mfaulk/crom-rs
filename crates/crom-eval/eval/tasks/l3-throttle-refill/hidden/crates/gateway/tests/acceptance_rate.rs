use clock::ManualClock;
use gateway::{Decision, Gateway};

#[test]
fn retry_after_reflects_time_to_next_token() {
    let clock = ManualClock::new(10_000);
    let mut g = Gateway::new(clock.clone(), 1, 2_500);
    assert_eq!(g.check("10.0.0.1"), Decision::Allow { remaining: 0 });
    clock.advance(1_200);
    let d = g.check("10.0.0.1");
    assert_eq!(
        d,
        Decision::Limit {
            retry_after_ms: 1_300
        }
    );
    assert_eq!(d.retry_after_header(), Some("2".into()));
}

#[test]
fn polling_client_gets_configured_throughput() {
    let clock = ManualClock::new(0);
    let mut g = Gateway::new(clock.clone(), 2, 100);
    g.check("c");
    g.check("c");
    let mut allowed = 0;
    for _ in 0..50 {
        clock.advance(60);
        if matches!(g.check("c"), Decision::Allow { .. }) {
            allowed += 1;
        }
    }
    assert_eq!(allowed, 30);
}
