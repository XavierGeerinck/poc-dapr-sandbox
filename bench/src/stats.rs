//! Percentiles.

/// Nearest-rank percentile of an ascending slice; `p` is in `0.0..=100.0`.
///
/// # Panics
/// Panics if `sorted` is empty.
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    assert!(!sorted.is_empty(), "percentile of an empty sample");
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}
