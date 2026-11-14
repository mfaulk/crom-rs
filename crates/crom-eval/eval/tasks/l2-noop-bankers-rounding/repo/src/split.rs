/// Split `total_cents` into `n` shares that sum exactly to the total. Shares
/// differ by at most one cent and leftover cents go to the first payers.
/// Panics if `n == 0` or `total_cents` is negative.
pub fn split(total_cents: i64, n: usize) -> Vec<i64> {
    assert!(n > 0, "cannot split between zero payers");
    assert!(total_cents >= 0, "cannot split a negative total");
    let n_i = n as i64;
    let base = total_cents / n_i;
    let extra = (total_cents % n_i) as usize;
    (0..n)
        .map(|i| if i < extra { base + 1 } else { base })
        .collect()
}
