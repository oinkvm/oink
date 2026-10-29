//! Native content transport and target filename policy.
mod link;
pub use link::*;
pub(crate) mod names;
pub use names::{TargetOs, sanitize_component, sanitize_entries};
