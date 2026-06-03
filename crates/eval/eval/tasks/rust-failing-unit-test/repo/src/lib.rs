/// Return the maximum value in a slice, or `None` if empty.
///
/// Bug: the comparison is inverted, so this returns the minimum.
pub fn max_value(values: &[i64]) -> Option<i64> {
    let mut best = *values.first()?;
    for &v in &values[1..] {
        if v < best {
            best = v;
        }
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_maximum() {
        assert_eq!(max_value(&[1, 7, 3, 2]), Some(7));
        assert_eq!(max_value(&[-5, -1, -9]), Some(-1));
        assert_eq!(max_value(&[]), None);
    }
}
