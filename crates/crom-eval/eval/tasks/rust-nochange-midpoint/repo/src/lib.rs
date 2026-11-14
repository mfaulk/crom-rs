/// Average of `a` and `b`, rounded down, valid for the full `u32` range.
pub fn midpoint(a: u32, b: u32) -> u32 {
    (a & b) + ((a ^ b) >> 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values() {
        assert_eq!(midpoint(2, 4), 3);
        assert_eq!(midpoint(10, 10), 10);
    }
}
