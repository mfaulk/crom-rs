use logstore::{Store, StoreError};

#[test]
fn put_get_delete() {
    let mut s = Store::new();
    s.put(b"user:1", b"ada");
    s.put(b"user:2", b"grace");
    assert_eq!(s.get(b"user:1"), Some(&b"ada"[..]));
    assert!(s.delete(b"user:1"));
    assert!(!s.delete(b"user:1"));
    assert_eq!(s.get(b"user:1"), None);
    assert_eq!(s.len(), 1);
}

#[test]
fn reopen_replays_log() {
    let mut s = Store::new();
    s.put(b"a", b"1");
    s.put(b"b", b"2");
    s.put(b"a", b"3");
    s.delete(b"b");
    let reopened = Store::open(s.into_log()).unwrap();
    assert_eq!(reopened.get(b"a"), Some(&b"3"[..]));
    assert_eq!(reopened.get(b"b"), None);
    assert_eq!(reopened.record_count(), 4);
}

#[test]
fn compact_drops_dead_records() {
    let mut s = Store::new();
    for i in 0..10u8 {
        s.put(b"k", &[i]);
    }
    s.put(b"x", b"y");
    s.delete(b"x");
    s.compact();
    assert_eq!(s.record_count(), 1);
    let reopened = Store::open(s.into_log()).unwrap();
    assert_eq!(reopened.get(b"k"), Some(&[9u8][..]));
}

#[test]
fn corrupt_log_reports_offset() {
    let mut s = Store::new();
    s.put(b"a", b"1");
    let first = s.log().len();
    s.put(b"b", b"2");
    let mut log = s.into_log();
    let n = log.len();
    log[n - 1] ^= 0x01;
    match Store::open(log) {
        Err(StoreError::Corrupt { offset, .. }) => assert_eq!(offset, first),
        other => panic!("expected corruption, got {other:?}"),
    }
}
