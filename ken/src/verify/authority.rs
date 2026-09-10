//! The only place control-plane [`WriteOp`]s can be constructed. Each function
//! mints a [`ControlToken`], which untrusted callers cannot obtain.

use super::ControlToken;
use crate::schema::{Element, Epistemics, FactId, GeneratorHash, GroundBinding, Outcome, Resolved};
use crate::write::WriteOp;

/// Bind an independent ground source to a fact (`ken ground`). A privilege
/// escalation, logged loudly (DESIGN §10).
pub fn ground(fact: FactId, binding: GroundBinding) -> WriteOp {
    WriteOp::Ground {
        fact,
        binding,
        _auth: ControlToken::mint(),
    }
}

/// Record the result of one ground check. The only path that can move
/// `groundedness` (DESIGN §2, §3); a verifier is just the tier-3 case.
pub fn ground_check(
    fact: FactId,
    ground: usize,
    outcome: Outcome,
    by: Option<GeneratorHash>,
    resolved: Option<Resolved>,
    set_update: Option<Vec<Element>>,
    observed_cost: Option<f64>,
) -> WriteOp {
    WriteOp::GroundCheck {
        fact,
        ground,
        outcome,
        by,
        resolved,
        set_update,
        observed_cost,
        _auth: ControlToken::mint(),
    }
}

/// Adjust scheduling priority. Never touches truth (DESIGN §3).
pub fn reschedule(fact: FactId, new_priority: f64) -> WriteOp {
    WriteOp::Reschedule {
        fact,
        new_priority,
        _auth: ControlToken::mint(),
    }
}

/// Human override of belief, always logged loudly (DESIGN §3, §10).
pub fn manual_override(fact: FactId, set: Epistemics, reason: String) -> WriteOp {
    WriteOp::ManualOverride {
        fact,
        set,
        reason,
        _auth: ControlToken::mint(),
    }
}
