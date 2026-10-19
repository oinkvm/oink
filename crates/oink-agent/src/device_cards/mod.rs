//! Device-card projection and local control contract. Presentation never authors
//! owner state: opaque actions fence an absolute operation to its Boot/revision.
mod codes;
mod projector;
mod tokens;
mod types;
pub use projector::*;
pub use tokens::*;
pub use types::*;
