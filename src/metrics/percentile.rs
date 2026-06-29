//! Percentile helpers aligned with numpy's default linear interpolation.

/// λ-th percentile with linear interpolation (`np.percentile(a, p * 100)` default).
///
/// For sorted input `a` of length `n` and `p ∈ [0, 1]`, uses virtual index
/// `(n - 1) · p` and linearly interpolates between adjacent samples.
#[must_use]
pub(crate) fn percentile_linear(values: &[f64], p: f64) -> f64 {
    assert!(!values.is_empty());

    let n = values.len();
    if n == 1 {
        return values[0];
    }

    let p = p.clamp(0.0, 1.0);
    let virtual_index = (n - 1) as f64 * p;
    let lower = virtual_index.floor() as usize;
    let upper = virtual_index.ceil() as usize;

    if lower == upper {
        return values[lower];
    }

    let fraction = virtual_index - lower as f64;
    values[lower] + fraction * (values[upper] - values[lower])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_percentile_matches_numpy_style() {
        let mut values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());

        // virtual index (n-1)*0.9 = 8.1 → 9 + 0.1*(10-9) = 9.1
        let p90 = percentile_linear(&values, 0.9);
        assert!((p90 - 9.1).abs() < 1e-9);

        // virtual index 4.5 → 5.5
        let p50 = percentile_linear(&values, 0.5);
        assert!((p50 - 5.5).abs() < 1e-9);
    }

    #[test]
    fn single_element_percentile() {
        assert_eq!(percentile_linear(&[42.0], 0.9), 42.0);
    }
}
