/// Return page `page` (1-based) of `items`, with `per_page` items per page.
///
/// The last page may be shorter than `per_page`.
pub fn paginate<T: Clone>(items: &[T], page: usize, per_page: usize) -> Vec<T> {
    let start = (page - 1) * per_page;
    let end = (start + per_page).min(items.len());
    items[start..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_pages() {
        let items: Vec<u32> = (1..=6).collect();
        assert_eq!(paginate(&items, 1, 2), vec![1, 2]);
        assert_eq!(paginate(&items, 3, 2), vec![5, 6]);
    }

    #[test]
    fn short_last_page() {
        let items: Vec<u32> = (1..=5).collect();
        assert_eq!(paginate(&items, 2, 3), vec![4, 5]);
    }
}
