use bucket::{Take, TokenBucket};

#[test]
fn partial_progress_is_kept() {
    let mut b = TokenBucket::new(2, 100, 0);
    b.try_take(0);
    b.try_take(0);
    assert_eq!(b.try_take(150), Take::Granted { remaining: 0 });
    assert_eq!(b.try_take(200), Take::Granted { remaining: 0 });
    assert_eq!(b.try_take(250), Take::Denied { retry_after_ms: 50 });
    assert_eq!(b.try_take(299), Take::Denied { retry_after_ms: 1 });
    assert_eq!(b.try_take(300), Take::Granted { remaining: 0 });
}

#[test]
fn steady_polling_matches_configured_rate() {
    let mut b = TokenBucket::new(2, 100, 0);
    b.try_take(0);
    b.try_take(0);
    let mut granted = 0;
    let mut t = 0;
    while t < 3000 {
        t += 60;
        if matches!(b.try_take(t), Take::Granted { .. }) {
            granted += 1;
        }
    }
    assert_eq!(granted, 30);
}

#[test]
fn full_bucket_does_not_bank_time() {
    let mut b = TokenBucket::new(1, 100, 0);
    assert_eq!(b.try_take(1000), Take::Granted { remaining: 0 });
    assert_eq!(b.try_take(1050), Take::Denied { retry_after_ms: 50 });
    assert_eq!(b.available(1099), 0);
    assert_eq!(b.available(1100), 1);
}

#[test]
fn capacity_is_a_ceiling() {
    let mut b = TokenBucket::new(3, 10, 0);
    for _ in 0..3 {
        b.try_take(0);
    }
    assert_eq!(b.available(25), 2);
    assert_eq!(b.available(1_000), 3);
    b.try_take(1_000);
    assert_eq!(b.try_take(1_000), Take::Granted { remaining: 1 });
}
