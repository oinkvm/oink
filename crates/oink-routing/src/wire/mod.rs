//! The router's I0 messages as v2 wire messages (`oink_protocol::v2`), shared
//! by the desktop input owner and the phone's embedded router. Transport
//! admission and drag messages stay with their owners.
pub mod control;
pub mod input;
pub mod layout;
pub mod transform;
