//! A compact, mergeable t-digest (the `DataSketches` / Circllhist idea). Used to
//! turn a generator's static `cost_estimate` into a measured quantile of its
//! observed run times. Mergeability is the point: a digest survives `jj op
//! restore` and pools across every fact that shares a generator, the same way a
//! sketch merges across shards.

use serde::{Deserialize, Serialize};

/// Compression: larger keeps more centroids (finer quantiles), smaller is
/// coarser and cheaper. 100 is the usual default and is ample for cost stats.
const COMPRESSION: f64 = 100.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Centroid {
    mean: f64,
    weight: f64,
}

/// A streaming quantile sketch over a stream of `f64` samples. Bounded in size
/// by [`COMPRESSION`], so it stays small no matter how many samples it sees.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TDigest {
    centroids: Vec<Centroid>,
}

impl TDigest {
    pub fn new() -> Self {
        TDigest::default()
    }

    pub fn is_empty(&self) -> bool {
        self.centroids.is_empty()
    }

    /// Total observed weight (sample count, since every insert weighs 1).
    pub fn count(&self) -> f64 {
        self.centroids.iter().map(|c| c.weight).sum()
    }

    /// Fold one sample in. Non-finite samples are ignored.
    pub fn insert(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.centroids.push(Centroid {
            mean: value,
            weight: 1.0,
        });
        self.compress();
    }

    /// Merge another digest into this one (the property a plain mean cannot
    /// offer): pool two independent sketches without the underlying samples.
    pub fn merge(&mut self, other: &TDigest) {
        self.centroids.extend(other.centroids.iter().cloned());
        self.compress();
    }

    /// The value at quantile `q` in `[0,1]` by linear interpolation between
    /// centroid centers, or `None` if the digest is empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let total = self.count();
        if self.centroids.is_empty() || total <= 0.0 {
            return None;
        }
        if self.centroids.len() == 1 {
            return Some(self.centroids[0].mean);
        }
        let target = q.clamp(0.0, 1.0) * total;
        let mut cum = 0.0;
        let mut prev_center = 0.0;
        let mut prev_mean = self.centroids[0].mean;
        for (i, c) in self.centroids.iter().enumerate() {
            let center = cum + c.weight / 2.0;
            if target <= center {
                if i == 0 {
                    return Some(c.mean);
                }
                let frac = (target - prev_center) / (center - prev_center);
                return Some(prev_mean + frac * (c.mean - prev_mean));
            }
            cum += c.weight;
            prev_center = center;
            prev_mean = c.mean;
        }
        self.centroids.last().map(|c| c.mean)
    }

    /// Greedily merge adjacent centroids while each stays under the t-digest
    /// size bound `4 * total * q * (1 - q) / COMPRESSION`, which keeps the tails
    /// fine and lets the middle coarsen.
    fn compress(&mut self) {
        if self.centroids.len() <= 1 {
            return;
        }
        self.centroids.sort_by(|a, b| a.mean.total_cmp(&b.mean));
        let total = self.count();
        let mut merged: Vec<Centroid> = Vec::with_capacity(self.centroids.len());
        let mut cum = 0.0;
        let mut acc = self.centroids[0].clone();
        for c in self.centroids.iter().skip(1) {
            let combined = acc.weight + c.weight;
            let q = (cum + combined / 2.0) / total;
            let bound = (4.0 * total * q * (1.0 - q) / COMPRESSION).max(1.0);
            if combined <= bound {
                acc.mean = (acc.mean * acc.weight + c.mean * c.weight) / combined;
                acc.weight = combined;
            } else {
                cum += acc.weight;
                merged.push(acc);
                acc = c.clone();
            }
        }
        merged.push(acc);
        self.centroids = merged;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_quantile() {
        assert_eq!(TDigest::new().quantile(0.5), None);
    }

    #[test]
    fn single_sample_is_its_own_median() {
        let mut d = TDigest::new();
        d.insert(4.2);
        assert_eq!(d.quantile(0.5), Some(4.2));
    }

    #[test]
    fn median_of_uniform_stream_is_near_middle() {
        let mut d = TDigest::new();
        for i in 0..=1000 {
            d.insert(f64::from(i) / 1000.0);
        }
        let p50 = d.quantile(0.5).unwrap();
        assert!((p50 - 0.5).abs() < 0.05, "p50 was {p50}");
        let p90 = d.quantile(0.9).unwrap();
        assert!((p90 - 0.9).abs() < 0.05, "p90 was {p90}");
    }

    #[test]
    fn stays_compact() {
        let mut d = TDigest::new();
        for i in 0..100_000 {
            d.insert(f64::from(i) * 0.001);
        }
        // Bounded by compression, not by sample count: orders of magnitude
        // smaller than the 100k samples folded in.
        assert!(d.centroids.len() < 2_000, "grew to {}", d.centroids.len());
    }

    #[test]
    fn merge_matches_combined_stream() {
        let mut a = TDigest::new();
        let mut b = TDigest::new();
        let mut both = TDigest::new();
        for i in 0..1000 {
            let x = f64::from(i) / 1000.0;
            if i % 2 == 0 {
                a.insert(x);
            } else {
                b.insert(x);
            }
            both.insert(x);
        }
        a.merge(&b);
        let merged = a.quantile(0.5).unwrap();
        let direct = both.quantile(0.5).unwrap();
        assert!((merged - direct).abs() < 0.05, "{merged} vs {direct}");
    }
}
