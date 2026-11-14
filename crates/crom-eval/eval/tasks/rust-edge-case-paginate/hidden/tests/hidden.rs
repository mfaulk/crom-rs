use pager::paginate;

fn items() -> Vec<u32> {
    (1..=7).collect()
}

/// Valid pages, including a short last page, are unchanged.
#[test]
fn valid_pages_unchanged() {
    assert_eq!(paginate(&items(), 1, 3), vec![1, 2, 3]);
    assert_eq!(paginate(&items(), 2, 3), vec![4, 5, 6]);
    assert_eq!(paginate(&items(), 3, 3), vec![7]);
    assert_eq!(paginate(&items(), 1, 100), items());
}

/// Page 0 and per_page 0 return an empty Vec instead of panicking.
#[test]
fn zero_page_or_size_is_empty() {
    assert!(paginate(&items(), 0, 3).is_empty());
    assert!(paginate(&items(), 1, 0).is_empty());
    assert!(paginate(&items(), 0, 0).is_empty());
}

/// Pages past the end and empty input return an empty Vec.
#[test]
fn out_of_range_is_empty() {
    assert!(paginate(&items(), 4, 3).is_empty());
    assert!(paginate(&items(), 50, 3).is_empty());
    assert!(paginate::<u32>(&[], 1, 3).is_empty());
}

/// Arithmetic overflow in page * per_page never panics.
#[test]
fn huge_values_do_not_overflow() {
    assert!(paginate(&items(), usize::MAX, 2).is_empty());
    assert!(paginate(&items(), 2, usize::MAX).is_empty());
    assert_eq!(paginate(&items(), 1, usize::MAX), items());
}
