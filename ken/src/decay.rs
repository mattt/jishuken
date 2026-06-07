//! Staleness as covariance (DESIGN §5) and the Kalman update (DESIGN §8).
//!
//! Confidence is the scalar state estimate; variance is its covariance. Between
//! verifications variance accrues as process noise `Q` per unit time (the
//! predict step), so confidence relaxes toward maximal ignorance (0.5). On a
//! Confirmed/Refuted outcome we do a confidence-weighted Bayesian update, with a
//! network verifier's gain discounted because its channel is attacker-influenceable
//! (DESIGN §6a point 2).

use crate::config::Config;
use crate::schema::{Fact, Timestamp, Volatility};

/// Maximal-ignorance prior; confidence relaxes toward this as a fact goes stale.
pub const IGNORANCE: f64 = 0.5;

/// Base measurement noise for a deterministic local verifier.
const R_LOCAL: f64 = 0.05;
/// A network verifier is noisier evidence, so its gain is discounted.
const R_NET: f64 = 0.30;
/// Variance floor so a fact never becomes perfectly rigid.
const VAR_FLOOR: f64 = 1e-4;

/// Half-life in seconds for a volatility class, `None` for immutable.
pub fn half_life_secs(v: Volatility, cfg: &Config) -> Option<f64> {
    cfg.volatility.half_life_secs(v)
}

/// Process noise `Q` per second, derived from the class half-life. A shorter
/// half-life means variance accrues faster.
pub fn process_noise(v: Volatility, cfg: &Config) -> f64 {
    match half_life_secs(v, cfg) {
        None => 0.0,
        Some(hl) if hl <= 0.0 => 0.0,
        Some(hl) => std::f64::consts::LN_2 / hl,
    }
}

/// Confidence after decay (DESIGN §5). Relaxes toward [`IGNORANCE`] with the
/// fact's volatility half-life; immutable facts do not decay.
pub fn decayed_confidence(f: &Fact, now: Timestamp, cfg: &Config) -> f64 {
    let dt = (now - f.schedule.last_verified).num_seconds().max(0) as f64;
    let c0 = f.epistemics.confidence;
    match half_life_secs(f.schedule.volatility, cfg) {
        None => c0,
        Some(hl) if hl <= 0.0 => c0,
        Some(hl) => IGNORANCE + (c0 - IGNORANCE) * 0.5f64.powf(dt / hl),
    }
}

/// Variance after the predict step: `variance_at_verify + Q * dt`.
pub fn decayed_variance(f: &Fact, now: Timestamp, cfg: &Config) -> f64 {
    let dt = (now - f.schedule.last_verified).num_seconds().max(0) as f64;
    f.schedule.variance_at_verify + process_noise(f.schedule.volatility, cfg) * dt
}

/// Scalar Kalman update on a Confirmed (`true`) or Refuted (`false`) outcome.
/// Returns the posterior `(confidence, variance)`. A net-entitled verifier
/// carries more measurement noise, so it moves confidence less (DESIGN §6a).
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

    fn fact(vol: Volatility, conf: f64, verified_secs_ago: i64) -> Fact {
        let mut f = crate::test_support::verified_scalar("e.r");
        f.epistemics.confidence = conf;
        f.schedule.volatility = vol;
        f.schedule.last_verified = Utc::now() - Duration::seconds(verified_secs_ago);
        f
    }

    #[test]
    fn immutable_does_not_decay() {
        let cfg = Config::default();
        let f = fact(Volatility::Immutable, 0.9, 1_000_000);
        assert!((decayed_confidence(&f, Utc::now(), &cfg) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn decay_relaxes_toward_ignorance() {
        let cfg = Config::default();
        let f = fact(Volatility::Hours, 0.9, 6 * 3600); // one half-life
        let c = decayed_confidence(&f, Utc::now(), &cfg);
        assert!(
            (c - 0.7).abs() < 0.02,
            "one half-life should land near 0.7, got {c}"
        );
        let f2 = fact(Volatility::Hours, 0.9, 600 * 3600); // very stale
        assert!((decayed_confidence(&f2, Utc::now(), &cfg) - IGNORANCE).abs() < 0.01);
    }

    #[test]
    fn variance_inflates_with_time() {
        let cfg = Config::default();
        let fresh = fact(Volatility::Hours, 0.9, 0);
        let stale = fact(Volatility::Hours, 0.9, 100_000);
        assert!(
            decayed_variance(&stale, Utc::now(), &cfg) > decayed_variance(&fresh, Utc::now(), &cfg)
        );
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
