//! Self-calibration.
//! Every Confirmed/Refuted outcome logs a `(prior, grounded_outcome)` pair; the
//! verification engine thereby produces the training signal to grade its own
//! LLM triage.
//! If the priors run hot, fit a 1-D Platt recalibration map and apply it to
//! future ingests.

use serde::{Deserialize, Serialize};

/// A single training pair, produced only by a `GroundCheck`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CalibrationSample {
    pub prior: f64,
    pub grounded_outcome: bool,
}

/// Logistic (Platt) recalibration map: `sigmoid(a * prior + b)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Recalibrator {
    pub a: f64,
    pub b: f64,
}

impl Default for Recalibrator {
    /// Identity-ish: passes confidence through near-unchanged.
    fn default() -> Self {
        Recalibrator { a: 1.0, b: 0.0 }
    }
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

impl Recalibrator {
    /// True when no fit has moved the map off its default. The docs promise the
    /// map "is the identity until a handful of samples exist", so the unfitted
    /// map must pass priors through exactly, not approximately.
    pub fn is_identity(&self) -> bool {
        *self == Recalibrator::default()
    }

    pub fn apply(&self, prior: f64) -> f64 {
        if self.is_identity() {
            return prior.clamp(0.0, 1.0);
        }
        // Center the prior so the fitted map stays well-conditioned on [0,1].
        sigmoid(self.a * (prior - 0.5) * 4.0 + self.b).clamp(0.0, 1.0)
    }
}

/// Mean squared error between predicted priors and binary outcomes.
pub fn brier_score(samples: &[CalibrationSample]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|s| {
            let y = if s.grounded_outcome { 1.0 } else { 0.0 };
            (s.prior - y).powi(2)
        })
        .sum();
    sum / samples.len() as f64
}

/// Reliability diagram bins: equal-width intervals over [0, 1].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReliabilityBin {
    pub bin_center: f64,
    pub mean_predicted: f64,
    pub mean_observed: f64,
    pub count: usize,
}

/// Bucket samples into `num_bins` equal-width intervals; empty bins omitted.
pub fn reliability_bins(samples: &[CalibrationSample], num_bins: usize) -> Vec<ReliabilityBin> {
    if samples.is_empty() || num_bins == 0 {
        return vec![];
    }
    let mut bins: Vec<Vec<&CalibrationSample>> = vec![vec![]; num_bins];
    for s in samples {
        let slot = (s.prior.clamp(0.0, 1.0) * num_bins as f64).floor();
        #[expect(
            clippy::cast_sign_loss,
            reason = "slot is a clamped-to-[0,1] prior scaled and floored, never negative"
        )]
        let idx = (slot as usize).min(num_bins.saturating_sub(1));
        bins[idx].push(s);
    }
    bins.into_iter()
        .enumerate()
        .filter_map(|(i, bin)| {
            let count = bin.len();
            if count == 0 {
                return None;
            }
            let mean_predicted = bin.iter().map(|s| s.prior).sum::<f64>() / count as f64;
            let mean_observed =
                bin.iter().filter(|s| s.grounded_outcome).count() as f64 / count as f64;
            Some(ReliabilityBin {
                bin_center: (i as f64 + 0.5) / num_bins as f64,
                mean_predicted,
                mean_observed,
                count,
            })
        })
        .collect()
}

/// Fit a Platt map by gradient descent on log-loss.
/// Returns the identity map until there are at least four samples.
pub fn recalibrate(samples: &[CalibrationSample]) -> Recalibrator {
    if samples.len() < 4 {
        return Recalibrator::default();
    }
    let (mut a, mut b) = (1.0f64, 0.0f64);
    let lr = 0.1;
    let n = samples.len() as f64;
    for _ in 0..2000 {
        let (mut ga, mut gb) = (0.0, 0.0);
        for s in samples {
            let x = (s.prior - 0.5) * 4.0;
            let p = sigmoid(a * x + b);
            let y = if s.grounded_outcome { 1.0 } else { 0.0 };
            let err = p - y;
            ga += err * x;
            gb += err;
        }
        a -= lr * ga / n;
        b -= lr * gb / n;
    }
    Recalibrator { a, b }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_few_samples_is_identity() {
        let r = recalibrate(&[CalibrationSample {
            prior: 0.9,
            grounded_outcome: true,
        }]);
        assert_eq!(r, Recalibrator::default());
    }

    #[test]
    fn hot_priors_get_pulled_down() {
        // The LLM claimed ~0.9 every time but was right only ~30% of the time.
        let mut samples = Vec::new();
        for i in 0..100 {
            samples.push(CalibrationSample {
                prior: 0.9,
                grounded_outcome: i % 10 < 3,
            });
        }
        let r = recalibrate(&samples);
        assert!(r.apply(0.9) < 0.9, "hot prior should be discounted");
        assert!(r.apply(0.9) < 0.6);
    }

    #[test]
    fn brier_score_perfect_is_zero() {
        let samples = vec![
            CalibrationSample {
                prior: 1.0,
                grounded_outcome: true,
            },
            CalibrationSample {
                prior: 0.0,
                grounded_outcome: false,
            },
        ];
        assert!((brier_score(&samples) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn reliability_bins_cover_samples() {
        let samples = vec![
            CalibrationSample {
                prior: 0.1,
                grounded_outcome: false,
            },
            CalibrationSample {
                prior: 0.9,
                grounded_outcome: true,
            },
        ];
        let bins = reliability_bins(&samples, 10);
        assert_eq!(bins.len(), 2);
        assert_eq!(bins.iter().map(|b| b.count).sum::<usize>(), 2);
    }

    #[test]
    fn well_calibrated_priors_roughly_preserved() {
        let mut samples = Vec::new();
        for i in 0..100 {
            samples.push(CalibrationSample {
                prior: 0.8,
                grounded_outcome: i % 10 < 8,
            });
        }
        let r = recalibrate(&samples);
        assert!((r.apply(0.8) - 0.8).abs() < 0.15);
    }
}
