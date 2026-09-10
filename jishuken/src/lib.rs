//! Jishuken: memory for agents with scheduled verification.
//!
//! The store holds fact files and an append-only operation log.
//! See the [README](https://github.com/mattt/jishuken#readme) for usage,
//! design, and security boundaries.

pub mod calibration;
pub mod centrality;
pub mod config;
pub mod decay;
pub mod duration;
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
