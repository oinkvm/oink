//! Oink 0.4.0 protocol v2: retaining CBOR DTOs, bounded framing and admission.
//!
//! This module is deliberately separate from the frozen v1 module.  During the
//! migration the existing desktop owners still compile against `v1`; new
//! mobile/core owners can switch to this namespace without changing the
//! canonical frame or inventing a second wire bus.
//! Native and business models convert explicitly at each domain boundary.

pub mod admission;
pub mod cbor;
pub mod dto;
pub mod error;
pub mod frame;
pub mod hot;
pub mod identity;
pub mod labels;
pub mod layout;
pub mod limits;
pub mod registry;
pub mod retained;
pub mod schema;

pub use cbor::{RawMap, RawValue};
pub use error::{Error, ErrorCode, Result};
pub use registry::Message;

pub const MAJOR: u16 = 2;
pub const MINOR: u16 = 0;
pub const QUIC_ALPN: &[u8] = b"oink-wire/2";
pub const LOW_ALPN: &[u8] = b"oink-low/2";
pub const DISCOVERY_SERVICE: &str = "_oink._udp";
pub const SPEC_REVISION: &str = "2026-11-07-mobile-v040";

/// Surface extents/contact coordinates count this fixed dimensionless quantum.
/// Capture/injection adapters alone convert to/from native platform units.
pub const CONTACT_REFERENCE_QUANTUM: f64 = 1.0 / 68_600.0;
