//! Vector search utilities
//! Mirrors gbrain's src/core/search/vector.ts

/// Compute cosine similarity between two vectors
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    if a.len() != b.len() {
        tracing::warn!(
            a_len = a.len(),
            b_len = b.len(),
            "cosine_similarity: dimension mismatch, returning 0.0"
        );
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    let result = dot / (norm_a * norm_b);
    result.clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        // Opposite vectors should return -1.0
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-6, "expected -1.0, got {}", sim);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        // Zero vector paired with a non-zero vector should return 0.0
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);

        // Both zero vectors should also return 0.0
        let c = vec![0.0, 0.0];
        let d = vec![0.0, 0.0];
        assert_eq!(cosine_similarity(&c, &d), 0.0);
    }

    #[test]
    fn test_cosine_similarity_empty_vectors() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_nan_input() {
        // NaN in input should not panic; result may be NaN but must not crash
        let a = vec![f32::NAN, 1.0];
        let b = vec![1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        // NaN propagation is acceptable; just ensure no panic
        assert!(sim.is_nan() || sim.is_finite());
    }

    #[test]
    fn test_cosine_similarity_inf_input() {
        // Inf in input should not panic
        let a = vec![f32::INFINITY, 0.0];
        let b = vec![1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        // Result may be NaN due to inf/sqrt(inf) interaction; just ensure no panic
        assert!(sim.is_nan() || sim.is_finite() || sim.is_infinite());
    }
}
