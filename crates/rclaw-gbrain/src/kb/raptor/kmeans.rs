//! K-Means++ clustering

use rand::Rng;

pub struct KMeans {
    k: usize,
    max_iter: usize,
    tolerance: f64,
}

impl KMeans {
    pub fn new(k: usize, max_iter: usize, tolerance: f64) -> Self {
        Self {
            k,
            max_iter,
            tolerance,
        }
    }

    pub fn cluster(&self, vectors: &[Vec<f32>]) -> Vec<usize> {
        if vectors.is_empty() {
            return Vec::new();
        }

        let k = self.k.min(vectors.len());
        if k <= 1 {
            return vec![0; vectors.len()];
        }

        let mut rng = rand::thread_rng();
        let centroids = self.init_centroids_kmeans_plus_plus(vectors, k, &mut rng);

        let mut assignments = vec![0usize; vectors.len()];
        let mut centroids = centroids;

        for _ in 0..self.max_iter {
            let mut changed = false;
            for (i, vector) in vectors.iter().enumerate() {
                let nearest = nearest_centroid(vector, &centroids);
                if assignments[i] != nearest {
                    assignments[i] = nearest;
                    changed = true;
                }
            }

            if !changed {
                break;
            }

            let new_centroids = update_centroids(vectors, &assignments, k);

            let max_move = new_centroids
                .iter()
                .zip(centroids.iter())
                .map(|(new, old)| euclidean_distance(new, old))
                .fold(0.0_f64, f64::max);

            centroids = new_centroids;

            if max_move < self.tolerance {
                break;
            }
        }

        assignments
    }

    fn init_centroids_kmeans_plus_plus(
        &self,
        vectors: &[Vec<f32>],
        k: usize,
        rng: &mut impl Rng,
    ) -> Vec<Vec<f32>> {
        let n = vectors.len();
        let mut centroids = Vec::with_capacity(k);

        let first_idx = rng.gen_range(0..n);
        centroids.push(vectors[first_idx].clone());

        for _ in 1..k {
            let distances: Vec<f64> = vectors
                .iter()
                .map(|v| {
                    centroids
                        .iter()
                        .map(|c| euclidean_distance(v, c).powi(2))
                        .fold(f64::INFINITY, f64::min)
                })
                .collect();

            let total: f64 = distances.iter().sum();
            if total.abs() < 1e-10 {
                let idx = rng.gen_range(0..n);
                centroids.push(vectors[idx].clone());
                continue;
            }

            let r: f64 = rng.gen_range(0.0..total);
            let mut cumsum = 0.0;
            let mut chosen = 0;
            for (i, &d) in distances.iter().enumerate() {
                cumsum += d;
                if cumsum >= r {
                    chosen = i;
                    break;
                }
            }
            centroids.push(vectors[chosen].clone());
        }

        centroids
    }
}

fn euclidean_distance(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn nearest_centroid(vector: &[f32], centroids: &[Vec<f32>]) -> usize {
    centroids
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            euclidean_distance(vector, a)
                .partial_cmp(&euclidean_distance(vector, b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn update_centroids(vectors: &[Vec<f32>], assignments: &[usize], k: usize) -> Vec<Vec<f32>> {
    let dims = vectors.first().map(|v| v.len()).unwrap_or(0);
    let mut centroids = vec![vec![0.0_f32; dims]; k];
    let mut counts = vec![0usize; k];

    for (vector, &cluster) in vectors.iter().zip(assignments.iter()) {
        counts[cluster] += 1;
        for (j, val) in vector.iter().enumerate() {
            centroids[cluster][j] += val;
        }
    }

    for (i, centroid) in centroids.iter_mut().enumerate() {
        if counts[i] > 0 {
            for val in centroid.iter_mut() {
                *val /= counts[i] as f32;
            }
        }
    }

    centroids
}

pub fn get_clusters<T: Clone>(items: &[T], assignments: &[usize], k: usize) -> Vec<Vec<T>> {
    let mut clusters = vec![Vec::new(); k];
    for (item, &cluster) in items.iter().zip(assignments.iter()) {
        if cluster < k {
            clusters[cluster].push(item.clone());
        }
    }
    clusters
}
