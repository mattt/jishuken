//! The control-plane write authority. Holds the authority token: the only place
//! privileged `WriteOp`s can be minted. Judging is now a pure predicate
//! (`crate::predicate`) and reading lives in `crate::ground`, so this module is
//! just the compile-time boundary.

pub mod authority;

/// Proof that a [`crate::write::WriteOp`] was constructed inside this module.
///
/// The inner `()` is private to `verify`, so the token can be minted only here
/// (and in descendant modules such as [`authority`]). No other module — not the
/// store, not the CLI, not the MCP server — can fabricate a control-plane
/// operation. This is what makes "only a ground check sets groundedness" a
/// compile-time guarantee rather than a convention.
#[derive(Debug, Clone)]
pub struct ControlToken(());

impl ControlToken {
    fn mint() -> ControlToken {
        ControlToken(())
    }
}

pub use authority::{ground, ground_check, manual_override, reschedule};
