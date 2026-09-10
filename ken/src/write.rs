//! Write authority (DESIGN §3). Every mutation is one [`WriteOp`] and becomes
//! one op-log record, tagged in the description so the invariant is auditable.
//!
//! The control-plane constructors live in [`crate::verify::authority`] and
//! require a [`ControlToken`], whose only field is private to that module.
//! Untrusted/data-plane callers can therefore construct [`WriteOp::Ingest`] and
//! nothing else — enforced by the compiler, not by convention.
//!
//! The boundary is a compile-time fact. The following does not compile, because
//! a data-plane caller cannot mint the authorization token:
//!
//! ```compile_fail
//! use ken::schema::{FactId, Outcome};
//! use ken::write::WriteOp;
//! // error: cannot construct GroundCheck; field `_auth` (ControlToken) is private
//! let _ = WriteOp::GroundCheck {
//!     fact: FactId("x".into()),
//!     ground: 0,
//!     outcome: Outcome::Confirmed,
//!     by: None,
//!     resolved: None,
//!     set_update: None,
//!     observed_cost: None,
//! };
//! ```

use crate::calibration::Recalibrator;
use crate::schema::{
    Claim, Element, Epistemics, FactId, FactValue, GeneratorHash, GroundBinding, Resolved,
    TriageSource, Volatility,
};
use crate::verify::ControlToken;

/// `Outcome` lives in `schema` (both the write path and a ground binding need
/// it); re-exported here so `crate::write::Outcome` keeps resolving.
pub use crate::schema::Outcome;

/// Maximum confidence an ungrounded ingest may land at. The data plane cannot
/// assert certainty; only a ground check can move confidence above this.
pub const INGEST_CONFIDENCE_CEILING: f64 = 0.5;

/// Confidence a bare ingest (neither LLM-scored nor manual) lands at: a weak
/// prior, well under the ceiling, pending its first ground check.
const INGEST_DEFAULT_CONFIDENCE: f64 = 0.4;

/// Every mutation of the store. Each becomes one tagged op-log record.
#[derive(Debug, Clone)]
pub enum WriteOp {
    /// Data plane. The ONLY op untrusted ingestion may construct. Always lands
    /// `Ungrounded` and cannot exceed [`INGEST_CONFIDENCE_CEILING`]. May carry a
    /// *draft* grounding (a `--ground` hint): a source + locator with no
    /// verifier, which does not count until the control plane grounds it.
    Ingest {
        claim: Claim,
        value: FactValue,
        triage: TriageSource,
        volatility: Volatility,
        draft_ground: Option<GroundBinding>,
    },

    /// Control plane. Bind an independent ground source to a fact (`ken ground`).
    /// A privilege escalation, logged loudly (DESIGN §10).
    Ground {
        fact: FactId,
        binding: GroundBinding,
        _auth: ControlToken,
    },

    /// Control plane. Record the result of one ground check. The only path that
    /// can move `groundedness` (DESIGN §2, §3). `by` is the generator hash for a
    /// `Generator` source, else `None`/a synthetic marker.
    GroundCheck {
        fact: FactId,
        ground: usize,
        outcome: Outcome,
        by: Option<GeneratorHash>,
        resolved: Option<Resolved>,
        /// For set-valued facts: the merged element list after diffing the
        /// returned set. `None` for scalar facts.
        set_update: Option<Vec<Element>>,
        /// Wall-clock seconds the read took, folded into the generator's cost
        /// sketch (DESIGN §7, Idea 2). `None` when there is nothing to measure.
        observed_cost: Option<f64>,
        _auth: ControlToken,
    },

    /// Control plane. Scheduler-only. Adjusts ordering, never truth.
    Reschedule {
        fact: FactId,
        new_priority: f64,
        _auth: ControlToken,
    },

    /// Control plane. Human override, always logged loudly.
    ManualOverride {
        fact: FactId,
        set: Epistemics,
        reason: String,
        _auth: ControlToken,
    },
}

impl WriteOp {
    /// Data-plane constructor. Available to anyone; lands `Ungrounded`.
    pub fn ingest(
        claim: Claim,
        value: FactValue,
        triage: TriageSource,
        volatility: Volatility,
        draft_ground: Option<GroundBinding>,
    ) -> WriteOp {
        WriteOp::Ingest {
            claim,
            value,
            triage,
            volatility,
            draft_ground,
        }
    }

    /// The variant name, used to tag the op-log record so the audit is greppable.
    pub fn tag(&self) -> String {
        match self {
            WriteOp::Ingest { .. } => "Ingest".to_string(),
            WriteOp::Ground { .. } => "Ground".to_string(),
            WriteOp::GroundCheck { outcome, .. } => format!("GroundCheck:{}", outcome.label()),
            WriteOp::Reschedule { .. } => "Reschedule".to_string(),
            WriteOp::ManualOverride { .. } => "ManualOverride".to_string(),
        }
    }

    pub fn fact_key(&self) -> String {
        match self {
            WriteOp::Ingest { claim, .. } => claim.key(),
            WriteOp::Ground { fact, .. }
            | WriteOp::GroundCheck { fact, .. }
            | WriteOp::Reschedule { fact, .. }
            | WriteOp::ManualOverride { fact, .. } => fact.0.clone(),
        }
    }
}

/// Confidence an ingest lands at, clamped to the triage ceiling. The data plane
/// cannot exceed this no matter what confidence it requests.
pub fn landed_confidence(triage: &TriageSource) -> f64 {
    landed_confidence_with(triage, &Recalibrator::default())
}

/// [`landed_confidence`] with a fitted recalibration map (DESIGN §8) applied to
/// LLM-triaged priors before the ceiling clamp. Grades the triage, never the
/// ground truth: bare ingests and manual triage are not LLM claims, so the map
/// does not touch them.
pub fn landed_confidence_with(triage: &TriageSource, recalibrator: &Recalibrator) -> f64 {
    let requested = match triage {
        TriageSource::Llm { meta_confidence } => recalibrator.apply(*meta_confidence),
        TriageSource::Ingest => INGEST_DEFAULT_CONFIDENCE,
        TriageSource::Manual => INGEST_CONFIDENCE_CEILING,
    };
    requested.clamp(0.0, INGEST_CONFIDENCE_CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::EntityId;

    fn claim() -> Claim {
        Claim {
            entity: EntityId("staging".into()),
            relation: "url".into(),
            nl: None,
        }
    }

    #[test]
    fn ingest_is_constructible_by_data_plane() {
        let op = WriteOp::ingest(
            claim(),
            FactValue::Scalar {
                value: serde_json::json!("x"),
            },
            TriageSource::Ingest,
            Volatility::Hours,
            None,
        );
        assert_eq!(op.tag(), "Ingest");
    }

    #[test]
    fn ingest_cannot_exceed_ceiling() {
        let hot = landed_confidence(&TriageSource::Llm {
            meta_confidence: 0.99,
        });
        assert!(hot <= INGEST_CONFIDENCE_CEILING);
        let cool = landed_confidence(&TriageSource::Llm {
            meta_confidence: 0.1,
        });
        assert!((cool - 0.1).abs() < 1e-9);
    }

    #[test]
    fn recalibration_discounts_hot_llm_priors_on_ingest() {
        use crate::calibration::{recalibrate, CalibrationSample};
        // The triage claimed ~0.45 every time but held up only ~20% of the time.
        let samples: Vec<CalibrationSample> = (0..100)
            .map(|i| CalibrationSample {
                prior: 0.45,
                grounded_outcome: i % 10 < 2,
            })
            .collect();
        let map = recalibrate(&samples);
        let triage = TriageSource::Llm {
            meta_confidence: 0.45,
        };
        let recalibrated = landed_confidence_with(&triage, &map);
        let raw = landed_confidence(&triage);
        assert!(
            recalibrated < raw,
            "fitted map should discount the hot prior: {recalibrated} vs {raw}"
        );
    }

    #[test]
    fn recalibration_leaves_non_llm_triage_alone() {
        use crate::calibration::Recalibrator;
        let fitted = Recalibrator { a: 0.2, b: -2.0 };
        assert!(
            (landed_confidence_with(&TriageSource::Ingest, &fitted)
                - landed_confidence(&TriageSource::Ingest))
            .abs()
                < 1e-9
        );
        assert!(
            (landed_confidence_with(&TriageSource::Manual, &fitted)
                - landed_confidence(&TriageSource::Manual))
            .abs()
                < 1e-9
        );
    }
}
