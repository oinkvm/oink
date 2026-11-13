//! The outbound stream state machine lives in `oink-network` so that the
//! mobile core manages the streams it writes by the same rules.
pub use oink_network::stream_machine::*;
