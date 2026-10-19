//! Owner-targeted device controls. Durable options, volatile pause and native
//! output volume share request identity, admission limits and completion rules.
pub mod completion;
pub mod pause;
pub mod requests;
pub mod transactions;

mod emergency;
mod emergency_runtime;
mod owner;
pub(crate) mod runtime;
