//! Staleness as covariance and the Kalman update.
//!
//! Confidence is the scalar state estimate; variance is its covariance.
//! Between verifications variance accrues as process noise `Q` per unit time
//! (the predict step), so confidence relaxes toward maximal ignorance (0.5).
//! On a Confirmed/Refuted outcome we do a confidence-weighted Bayesian update,
//! with a network verifier's gain discounted because its channel is
//! attacker-influenceable.

use crate::schema::{Fact, Timestamp};

/// Maximal-ignorance prior; confidence relaxes toward this as a fact goes stale.
pub const IGNORANCE: f64 = 0.5;

/// Base measurement noise for a deterministic local verifier.
const R_LOCAL: f64 = 0.05;
/// A network verifier is noisier evidence, so its gain is discounted.
const R_NET: f64 = 0.30;
/// Variance floor so a fact never becomes perfectly rigid.
const VAR_FLOOR: f64 = 1e-4;

/// Half-life in seconds, anchored to the last verification for calendar units.
pub fn half_life_secs(f: &Fact) -> Option<f64> {
    f.schedule.half_life.seconds_at(f.schedule.last_verified)
}

/// Process noise `Q` per second; shorter half-lives accumulate variance faster.
pub fn process_noise(f: &Fact) -> f64 {
    half_life_secs(f).map_or(0.0, |hl| std::f64::consts::LN_2 / hl)
}

fn elapsed(f: &Fact, now: Timestamp) -> f64 {
    (now - f.schedule.last_verified)
        .to_std()
        .unwrap_or_default()
        .as_secs_f64()
}

/// Confidence relaxes toward [`IGNORANCE`] with the fact's half-life.
pub fn decayed_confidence(f: &Fact, now: Timestamp) -> f64 {
    let c0 = f.epistemics.confidence;
    half_life_secs(f).map_or(c0, |hl| {
        IGNORANCE + (c0 - IGNORANCE) * 0.5f64.powf(elapsed(f, now) / hl)
    })
}

/// Variance after the predict step: `variance_at_verify + Q * dt`.
pub fn decayed_variance(f: &Fact, now: Timestamp) -> f64 {
    f.schedule.variance_at_verify + process_noise(f) * elapsed(f, now)
}

/// Scalar Kalman update on a Confirmed (`true`) or Refuted (`false`) outcome.
/// Returns the posterior `(confidence, variance)`.
/// A net-entitled verifier carries more measurement noise, so it moves
/// confidence less.
pub fn kalman_update(
    prior_conf: f64,
    prior_var: f64,
    confirmed: bool,
    net_entitled: bool,
) -> (f64, f64) {
    let z = if confirmed { 1.0 } else { 0.0 };
    let r = if net_entitled { R_NET } else { R_LOCAL };
    let gain = prior_var / (prior_var + r);
    let conf = (prior_conf + gain * (z - prior_conf)).clamp(0.0, 1.0);
    let var = ((1.0 - gain) * prior_var).max(VAR_FLOOR);
    (conf, var)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;
    use chrono::{Duration, Utc};

    fn fact(vol: HalfLife, conf: f64, verified_secs_ago: i64) -> Fact {
        let mut f = crate::test_support::verified_scalar("e.r");
        f.epistemics.confidence = conf;
        f.schedule.half_life = vol;
        f.schedule.last_verified = Utc::now() - Duration::seconds(verified_secs_ago);
        f
    }

    #[test]
    fn never_does_not_decay() {
        let f = fact(HalfLife::Never, 0.9, 1_000_000);
        assert!((decayed_confidence(&f, Utc::now()) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn decay_relaxes_toward_ignorance() {
        let f = fact("PT6H".parse::<HalfLife>().unwrap(), 0.9, 6 * 3600); // one half-life
        let c = decayed_confidence(&f, Utc::now());
        assert!(
            (c - 0.7).abs() < 0.02,
            "one half-life should land near 0.7, got {c}"
        );
        let f2 = fact("PT6H".parse::<HalfLife>().unwrap(), 0.9, 600 * 3600); // very stale
        assert!((decayed_confidence(&f2, Utc::now()) - IGNORANCE).abs() < 0.01);
    }

    #[test]
    fn variance_inflates_with_time() {
        let fresh = fact("PT6H".parse::<HalfLife>().unwrap(), 0.9, 0);
        let stale = fact("PT6H".parse::<HalfLife>().unwrap(), 0.9, 100_000);
        assert!(decayed_variance(&stale, Utc::now()) > decayed_variance(&fresh, Utc::now()));
    }

    #[test]
    fn fractional_half_life_controls_confidence_variance_and_due_time() {
        let mut f = fact("PT0.0021S".parse().unwrap(), 0.9, 0);
        let start = "2024-01-01T00:00:00Z".parse::<Timestamp>().unwrap();
        f.schedule.last_verified = start;
        let end = start + Duration::microseconds(2100);
        assert!((decayed_confidence(&f, end) - 0.7).abs() < 1e-12);
        assert!(
            (decayed_variance(&f, end) - f.schedule.variance_at_verify - std::f64::consts::LN_2)
                .abs()
                < 1e-12
        );
        assert_eq!(f.schedule.half_life.deadline(start), Some(end));
        assert_eq!(decayed_confidence(&f, start - Duration::seconds(1)), 0.9);
        assert_eq!(
            decayed_variance(&f, start - Duration::seconds(1)),
            f.schedule.variance_at_verify
        );
    }

    #[test]
    fn calendar_half_life_reanchors_after_verification() {
        let mut f = fact("P1M".parse().unwrap(), 0.9, 0);
        for (start, end, days) in [
            ("2024-01-31T00:00:00Z", "2024-02-29T00:00:00Z", 29),
            ("2024-02-29T00:00:00Z", "2024-03-29T00:00:00Z", 29),
            ("2023-01-31T00:00:00Z", "2023-02-28T00:00:00Z", 28),
            ("2024-03-01T00:00:00Z", "2024-04-01T00:00:00Z", 31),
        ] {
            f.schedule.last_verified = start.parse().unwrap();
            let end = end.parse().unwrap();
            assert_eq!(half_life_secs(&f), Some(f64::from(days * 86_400)));
            assert!((decayed_confidence(&f, end) - 0.7).abs() < 1e-12);
            assert_eq!(
                f.schedule.half_life.deadline(f.schedule.last_verified),
                Some(end)
            );
        }
    }

    #[test]
    fn aging_never_changes_groundedness() {
        let mut f = fact(HalfLife::Never, 0.4, 1000);
        f.epistemics.groundedness = Groundedness::Ungrounded {
            source: TriageSource::Ingest,
        };
        assert_eq!(decayed_confidence(&f, Utc::now()), 0.4);
        assert_eq!(
            decayed_variance(&f, Utc::now()),
            f.schedule.variance_at_verify
        );
        assert_eq!(f.epistemics.groundedness.label(), "ungrounded");
        f.schedule.half_life = "1ns".parse().unwrap();
        assert_eq!(decayed_confidence(&f, Utc::now()), IGNORANCE);
        assert_eq!(f.epistemics.groundedness.label(), "ungrounded");
    }

    #[test]
    fn confirm_raises_refute_lowers() {
        let (up, _) = kalman_update(0.6, 0.1, true, false);
        let (down, _) = kalman_update(0.6, 0.1, false, false);
        assert!(up > 0.6);
        assert!(down < 0.6);
    }

    #[test]
    fn net_verifier_moves_less_than_local() {
        let (local, _) = kalman_update(0.5, 0.2, true, false);
        let (net, _) = kalman_update(0.5, 0.2, true, true);
        assert!(local > net, "local {local} should beat net {net}");
    }

    #[test]
    fn stale_fact_moves_more_on_evidence() {
        // High prior variance (stale) => larger gain => bigger jump.
        let (low_var, _) = kalman_update(0.5, 0.05, true, false);
        let (high_var, _) = kalman_update(0.5, 0.5, true, false);
        assert!(high_var > low_var);
    }
}
