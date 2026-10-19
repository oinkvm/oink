//! Typed v1 wire contract, canonical CBOR profile, framing and admission rules.
//!
//! Business owners convert their local domain values to the retained DTOs in
//! [`v1`]. All LAN messages use that codec. [`frame`] provides local stream
//! metadata and delegates its wire header representation to the same v1 codec.

pub mod frame;
pub mod v1;
