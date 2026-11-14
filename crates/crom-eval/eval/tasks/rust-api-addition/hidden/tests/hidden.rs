use stack::Stack;

/// `peek` returns the top item by reference without removing it.
#[test]
fn peek_does_not_remove() {
    let mut s = Stack::new();
    assert_eq!(s.peek(), None);
    s.push("a".to_string());
    s.push("b".to_string());
    assert_eq!(s.peek().map(String::as_str), Some("b"));
    assert_eq!(s.len(), 2);
    s.pop();
    assert_eq!(s.peek().map(String::as_str), Some("a"));
}

/// `is_empty` tracks pushes and pops.
#[test]
fn is_empty_tracks_contents() {
    let mut s = Stack::new();
    assert!(s.is_empty());
    s.push(1);
    assert!(!s.is_empty());
    s.pop();
    assert!(s.is_empty());
}

/// `Default` produces the same empty stack as `new`.
#[test]
fn default_is_empty() {
    let s: Stack<u8> = Stack::default();
    assert!(s.is_empty());
    assert_eq!(s, Stack::new());
}

/// Collecting pushes items in order, so the last yielded item is on top.
#[test]
fn collect_pushes_in_order() {
    let mut s: Stack<i32> = vec![1, 2, 3].into_iter().collect();
    assert_eq!(s.len(), 3);
    assert_eq!(s.peek(), Some(&3));
    assert_eq!(s.pop(), Some(3));
    assert_eq!(s.pop(), Some(2));
    assert_eq!(s.pop(), Some(1));
    let empty: Stack<i32> = std::iter::empty().collect();
    assert!(empty.is_empty());
}
