use logstore::Store;

fn big(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn reopen_with_large_values() {
    let mut s = Store::new();
    let sizes = [0usize, 1, 127, 128, 200, 1000, 16_384, 70_000];
    for (i, n) in sizes.iter().enumerate() {
        s.put(format!("k{i}").as_bytes(), &big(*n, i as u8));
    }
    s.delete(b"k1");
    let reopened = Store::open(s.into_log()).expect("reopen");
    for (i, n) in sizes.iter().enumerate() {
        let key = format!("k{i}");
        if i == 1 {
            assert_eq!(reopened.get(key.as_bytes()), None);
        } else {
            assert_eq!(
                reopened.get(key.as_bytes()),
                Some(&big(*n, i as u8)[..]),
                "{key}"
            );
        }
    }
}

#[test]
fn compact_then_reopen_with_long_keys() {
    let mut s = Store::new();
    let long_key = vec![b'x'; 300];
    s.put(&long_key, b"v1");
    s.put(&long_key, &big(500, 7));
    s.put(b"short", b"s");
    s.compact();
    let reopened = Store::open(s.into_log()).expect("reopen after compact");
    assert_eq!(reopened.get(&long_key), Some(&big(500, 7)[..]));
    assert_eq!(reopened.get(b"short"), Some(&b"s"[..]));
    assert_eq!(reopened.record_count(), 2);
}
