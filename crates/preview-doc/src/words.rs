//! Word and column measures for documents (more arrives with the word splitter).

/// The share of the table's width each column takes: its widest cell in characters, clamped to `[3, 40]`, normalized
/// to sum to 1.0.
pub fn column_fractions(widths: &[usize]) -> Vec<f32> {
    let clamped: Vec<usize> = widths.iter().map(|w| (*w).clamp(3, 40)).collect();
    let total: usize = clamped.iter().sum();
    clamped.iter().map(|w| *w as f32 / total as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_fractions_clamp_and_normalize() {
        let f = column_fractions(&[1, 100, 10]);
        let want = [3.0 / 53.0, 40.0 / 53.0, 10.0 / 53.0];
        assert_eq!(f.len(), 3);
        for (a, b) in f.iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{f:?}");
        }
        assert!(column_fractions(&[]).is_empty());
    }
}
