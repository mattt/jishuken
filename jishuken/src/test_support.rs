//! Shared fact builders for unit tests (M-TEST-UTIL). Each test module keeps a
//! thin wrapper that tunes the knobs its concern cares about; this is the one
//! place the full [`Fact`] skeleton lives.

use crate::schema::{
    Claim, Epistemics, Fact, FactId, FactValue, GeneratorHash, Groundedness, Provenance,
    ScheduleMeta, Volatility,
};
use chrono::Utc;

/// A freshly verified scalar fact with neutral schedule defaults: immutable,
/// centrality `1.0`, verified just now, confidence `0.9`. Tests override the
/// fields they exercise.
pub(crate) fn verified_scalar(key: &str) -> Fact {
    let now = Utc::now();
    let claim = Claim::parse_key(key).expect("valid test key");
    Fact {
        id: FactId::for_claim(&claim),
        claim,
        value: FactValue::Scalar {
            value: serde_json::json!("v"),
        },
        epistemics: Epistemics {
            confidence: 0.9,
            groundedness: Groundedness::Verified {
                at: now,
                by: GeneratorHash("h".into()),
            },
        },
        schedule: ScheduleMeta {
            volatility: Volatility::Immutable,
            centrality: 1.0,
            last_verified: now,
            variance_at_verify: 0.05,
            priority: 0.0,
        },
        grounds: vec![],
        provenance: Provenance {
            ingested_by: "test".into(),
            ingested_at: now,
        },
    }
}
