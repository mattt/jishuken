//! Ground sources: the typed reference (source + locator) a fact is checked
//! against, and the resolver that reduces it to a hashed span (README "Sources
//! and locators"). The verifier judges that span; the locator addresses it.

pub mod generator;
pub mod locator;
pub mod resolve;

pub use generator::{GeneratorRegistry, GeneratorRun, GeneratorSrc, Sandbox};
pub use locator::{parse_source, render_ground, render_source};
pub use resolve::{ground_source_for, read_binding, ReadResult, ResolvedSpan};
