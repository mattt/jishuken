//! Self-calibration (DESIGN §8). Every Confirmed/Refuted outcome logs a
//! `(prior, grounded_outcome)` pair; the verification engine thereby produces
//! the training signal to grade its own LLM triage. If the priors run hot, fit
//! a 1-D Platt recalibration map and apply it to future ingests.

use serde::{Deserialize, Serialize};

/// A single training pair, produced only by a `GroundCheck` (DESIGN §8).
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
    pub fn apply(&self, prior: f64) -> f64 {
        // Center the prior so the default map is close to the identity on [0,1].
        sigmoid(self.a * (prior - 0.5) * 4.0 + self.b).clamp(0.0, 1.0)
    }
}

/// Fit a Platt map by gradient descent on log-loss. Needs a handful of samples;
/// below that it returns the identity map (DESIGN §8: regenerable, refit freely).
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
