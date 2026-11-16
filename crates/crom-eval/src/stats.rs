//! Interval estimates and pass@k for eval reporting (M17.4). Formulas follow
//! `docs/milestones/MILESTONE_17.md` §8.1/§8.3.

use serde::{Deserialize, Serialize};

/// Two-sided 95% normal quantile.
pub const Z95: f64 = 1.959_963_984_540_054;

/// A closed interval `[low, high]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    pub low: f64,
    pub high: f64,
}

/// Wilson score 95% interval for `successes / n`; `None` when `n == 0`.
pub fn wilson_interval(successes: usize, n: usize) -> Option<Interval> {
    if n == 0 || successes > n {
        return None;
    }
    let n_f = n as f64;
    let p = successes as f64 / n_f;
    let z2 = Z95 * Z95;
    let denom = 1.0 + z2 / n_f;
    let center = (p + z2 / (2.0 * n_f)) / denom;
    let half = Z95 * (p * (1.0 - p) / n_f + z2 / (4.0 * n_f * n_f)).sqrt() / denom;
    Some(Interval {
        low: if successes == 0 {
            0.0
        } else {
            (center - half).max(0.0)
        },
        high: if successes == n {
            1.0
        } else {
            (center + half).min(1.0)
        },
    })
}

/// Unbiased pass@k estimator `1 - C(n-c, k) / C(n, k)` (Chen et al. 2021)
/// for one task with `n` samples of which `c` passed. `None` when `k == 0`,
/// `k > n`, or `c > n`.
pub fn pass_at_k(n: usize, c: usize, k: usize) -> Option<f64> {
    if k == 0 || k > n || c > n {
        return None;
    }
    if n - c < k {
        return Some(1.0);
    }
    // C(n-c, k) / C(n, k) = prod_{i = n-c+1}^{n} (1 - k / i), numerically stable.
    let fail_all = ((n - c + 1)..=n).fold(1.0, |acc, i| acc * (1.0 - k as f64 / i as f64));
    Some(1.0 - fail_all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 5e-5
    }

    /// Wilson 95% bounds for 7/10 match the textbook values [0.3968, 0.8922].
    #[test]
    fn wilson_matches_hand_computed() {
        let i = wilson_interval(7, 10).unwrap();
        assert!(close(i.low, 0.3968) && close(i.high, 0.8922), "{i:?}");
    }

    /// Wilson bounds stay inside [0, 1] at the extremes and are undefined for n = 0.
    #[test]
    fn wilson_extremes() {
        let zero = wilson_interval(0, 10).unwrap();
        assert_eq!(zero.low, 0.0);
        assert!(close(zero.high, 0.2775), "{zero:?}");
        let all = wilson_interval(10, 10).unwrap();
        assert!(close(all.low, 0.7225) && all.high == 1.0, "{all:?}");
        assert_eq!(wilson_interval(0, 0), None);
    }

    /// pass@k equals the hand-computed combinatorial value, including the n - c < k shortcut.
    #[test]
    fn pass_at_k_matches_combinatorics() {
        assert!(close(pass_at_k(5, 2, 1).unwrap(), 0.4));
        // 1 - C(3,3)/C(5,3) = 1 - 1/10.
        assert!(close(pass_at_k(5, 2, 3).unwrap(), 0.9));
        // 1 - C(8,2)/C(10,2) = 1 - 28/45.
        assert!(close(pass_at_k(10, 2, 2).unwrap(), 17.0 / 45.0));
        assert_eq!(pass_at_k(5, 3, 3), Some(1.0));
        assert_eq!(pass_at_k(5, 0, 3), Some(0.0));
        assert_eq!(pass_at_k(2, 1, 3), None);
        assert_eq!(pass_at_k(2, 1, 0), None);
    }
}
