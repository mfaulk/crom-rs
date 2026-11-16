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

/// One task's `(baseline (resolved, scored), candidate (resolved, scored))`.
pub type PairCounts = ((usize, usize), (usize, usize));

/// Exact two-sided McNemar p-value for paired binary outcomes, where `b`
/// pairs went pass -> fail and `c` went fail -> pass: `min(1, 2 * P(X <=
/// min(b, c)))` with `X ~ Binomial(b + c, 1/2)`. `1.0` when `b + c == 0`.
pub fn mcnemar_exact(b: usize, c: usize) -> f64 {
    let n = b + c;
    if n == 0 {
        return 1.0;
    }
    // Sum C(n, i) / 2^n for i in 0..=min(b, c), in log space to stay finite.
    let ln_half_n = n as f64 * 0.5f64.ln();
    let mut ln_choose = 0.0; // ln C(n, 0)
    let mut tail = 0.0;
    for i in 0..=b.min(c) {
        if i > 0 {
            ln_choose += ((n - i + 1) as f64).ln() - (i as f64).ln();
        }
        tail += (ln_choose + ln_half_n).exp();
    }
    (2.0 * tail).min(1.0)
}

/// Deterministic SplitMix64 generator, so bootstrap intervals are
/// reproducible without an RNG dependency.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform index in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Percentile bootstrap 95% interval for the paired resolved-rate delta.
/// Each pair is one task's `(baseline (resolved, scored), candidate
/// (resolved, scored))`; a resample draws tasks with replacement and computes
/// pooled candidate rate minus pooled baseline rate. `None` with no pairs.
pub fn bootstrap_delta_ci(pairs: &[PairCounts], resamples: usize, seed: u64) -> Option<Interval> {
    if pairs.is_empty() || resamples == 0 {
        return None;
    }
    let mut rng = SplitMix64::new(seed);
    let mut deltas: Vec<f64> = Vec::with_capacity(resamples);
    let mut sample = Vec::with_capacity(pairs.len());
    for _ in 0..resamples {
        sample.clear();
        sample.extend((0..pairs.len()).map(|_| pairs[rng.below(pairs.len())]));
        if let Some(d) = pooled_delta(&sample) {
            deltas.push(d);
        }
    }
    if deltas.is_empty() {
        return None;
    }
    deltas.sort_by(f64::total_cmp);
    let at = |q: f64| deltas[((deltas.len() - 1) as f64 * q).round() as usize];
    Some(Interval {
        low: at(0.025),
        high: at(0.975),
    })
}

/// Pooled `candidate - baseline` resolved-rate delta over `pairs`; `None`
/// when either side has no scored attempts.
pub fn pooled_delta(pairs: &[PairCounts]) -> Option<f64> {
    let (mut bc, mut bn, mut cc, mut cn) = (0, 0, 0, 0);
    for &((b_res, b_n), (c_res, c_n)) in pairs {
        bc += b_res;
        bn += b_n;
        cc += c_res;
        cn += c_n;
    }
    (bn > 0 && cn > 0).then(|| cc as f64 / cn as f64 - bc as f64 / bn as f64)
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

    /// Exact McNemar p-values match hand-computed binomial tails.
    #[test]
    fn mcnemar_matches_hand_computed() {
        // 2 * (C(7,0) + C(7,1)) / 2^7 = 16/128.
        assert!(close(mcnemar_exact(1, 6), 0.125));
        assert!(close(mcnemar_exact(6, 1), 0.125));
        // 2 / 2^10.
        assert!(close(mcnemar_exact(0, 10), 0.001_953_125));
        // 2 * (1 + 6 + 15 + 20) / 64 > 1, capped.
        assert_eq!(mcnemar_exact(3, 3), 1.0);
        assert_eq!(mcnemar_exact(0, 0), 1.0);
        // 2 * (1 + 12 + 66) / 4096 for b = 2, c = 10.
        assert!(close(mcnemar_exact(2, 10), 158.0 / 4096.0));
    }

    /// The bootstrap is seeded and deterministic, collapses to the point delta when every task moves identically, and brackets the observed delta.
    #[test]
    fn bootstrap_delta_ci_properties() {
        let same = vec![((0, 1), (1, 1)); 8];
        let ci = bootstrap_delta_ci(&same, 500, 7).unwrap();
        assert_eq!((ci.low, ci.high), (1.0, 1.0));
        let mixed: Vec<_> = (0..20)
            .map(|i| {
                if i % 4 == 0 {
                    ((1, 1), (0, 1))
                } else {
                    ((0, 1), (1, 1))
                }
            })
            .collect();
        let delta = pooled_delta(&mixed).unwrap();
        assert!(close(delta, (15.0 - 5.0) / 20.0));
        let a = bootstrap_delta_ci(&mixed, 2000, 42).unwrap();
        assert_eq!(a, bootstrap_delta_ci(&mixed, 2000, 42).unwrap());
        assert!(a.low < delta && delta < a.high, "{a:?}");
        assert!(a.low > 0.0, "a 15-vs-5 split is clearly positive: {a:?}");
        assert_eq!(bootstrap_delta_ci(&[], 100, 1), None);
        assert_eq!(pooled_delta(&[((0, 0), (1, 1))]), None);
    }
}
