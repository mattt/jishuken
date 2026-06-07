//! Jishuken (`ken`): self-verifying memory for agents, with jujutsu as the
//! storage and versioning spine. See `DESIGN.md` for the model and `README.md`
//! for the surface. The trust spine is: schema → write authority → store →
//! verifier → kalman → scheduler → centrality.

pub mod calibration;
pub mod centrality;
pub mod config;
pub mod decay;
pub mod engine;
pub mod error;
pub mod ground;
pub mod predicate;
pub mod scheduler;
pub mod schema;
pub mod sketch;
pub mod store;
pub mod verify;
pub mod write;

#[cfg(test)]
pub(crate) mod test_support;

pub use error::{Error, Result};
