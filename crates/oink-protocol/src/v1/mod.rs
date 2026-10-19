//! Retaining CBOR wire DTOs, bounded framing, and capability admission.
//! Native and business models convert explicitly at each domain boundary.

pub mod admission;
pub mod cbor;
pub mod dto;
pub mod error;
pub mod frame;
pub mod hot;
pub mod labels;
pub mod layout;
pub mod limits;
pub mod registry;
pub mod retained;
pub mod schema;

pub use cbor::{RawMap, RawValue};
pub use error::{Error, ErrorCode, Result};
pub use registry::Message;

pub const MAJOR: u16 = 1;
pub const MINOR: u16 = 0;
pub const QUIC_ALPN: &[u8] = b"oink-wire/1";
pub const LOW_ALPN: &[u8] = b"oink-low/1";
pub const DISCOVERY_SERVICE: &str = "_oink._udp";
pub const SPEC_REVISION: &str = "2026-10-15-ui-freeze";

/// Surface extents/contact coordinates count this fixed dimensionless quantum.
/// Capture/injection adapters alone convert to/from native platform units.
pub const CONTACT_REFERENCE_QUANTUM: f64 = 1.0 / 68_600.0;
