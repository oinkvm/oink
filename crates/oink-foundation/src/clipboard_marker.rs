//! The Oink clip marker: the identity of a clip that Oink wrote into the system
//! clipboard from a received offer, stored next to the content as one private
//! format. A synchronizer that copies the clipboard wholesale (Universal
//! Clipboard between two Macs, Logi Flow, any mirroring tool) brings the
//! marker back, and the receiving Oink recognises the clip without comparing
//! content.
//!
//! The marker is internal to one device: it is never announced, never compared
//! as content and never re-created on another machine.
use crate::DeviceId;
use crate::content::{ClipId, OfferId};

/// Marker type on macOS. `com.hachiwii.oink.*` is never re-created from
/// another machine's offer.
pub const MARKER_APPLE: &str = "com.hachiwii.oink.clip";
/// Registered clipboard format on Windows (names starting with "Oink" are never
/// re-created from another machine's offer).
pub const MARKER_WINDOWS: &str = "Oink Clip";
/// Record magic of the 0.3.x line (wire protocol v1 offer identities). 0.4.0
/// writes `OKC1` records for its protocol-v2 identities under the same format
/// names; the two lines do not interoperate on the network, but a marker can
/// survive on the clipboard across an upgrade or downgrade, so neither line
/// parses the other's records.
const MAGIC: &[u8; 4] = b"OKW1";
/// magic, origin device, clip sequence (LE), generation (LE).
pub const MARKER_BYTES: usize = 4 + 32 + 8 + 8;

/// Marker bytes of a written clip; None for identities without an origin
/// (unannounced captures).
pub fn encode(id: OfferId) -> Option<Vec<u8>> {
    if id.clip.origin.0 == [0; 32] {
        return None;
    }
    let mut bytes = Vec::with_capacity(MARKER_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&id.clip.origin.0);
    bytes.extend_from_slice(&id.clip.seq.to_le_bytes());
    bytes.extend_from_slice(&id.generation.to_le_bytes());
    Some(bytes)
}

pub fn parse(bytes: &[u8]) -> Option<OfferId> {
    // Windows HGLOBALs may carry allocation padding after the fixed record.
    let record = bytes.get(..MARKER_BYTES)?;
    if &record[..4] != MAGIC || bytes[MARKER_BYTES..].iter().any(|b| *b != 0) {
        return None;
    }
    let origin = DeviceId(record[4..36].try_into().ok()?);
    let seq = u64::from_le_bytes(record[36..44].try_into().ok()?);
    let generation = u64::from_le_bytes(record[44..52].try_into().ok()?);
    (origin.0 != [0; 32]).then_some(OfferId {
        clip: ClipId { origin, seq },
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> OfferId {
        OfferId {
            clip: ClipId {
                origin: DeviceId([7; 32]),
                seq: 42,
            },
            generation: 43,
        }
    }

    #[test]
    fn marker_round_trips_with_padding_and_rejects_bad_records() {
        let bytes = encode(id()).unwrap();
        assert_eq!(bytes.len(), MARKER_BYTES);
        assert_eq!(&bytes[..4], b"OKW1");
        assert_eq!(parse(&bytes), Some(id()));
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0; 12]);
        assert_eq!(parse(&padded), Some(id()), "HGLOBAL padding");
        padded.push(1);
        assert_eq!(parse(&padded), None);
        assert_eq!(parse(&bytes[..MARKER_BYTES - 1]), None);
        assert_eq!(parse(&[]), None);
        let mut wrong = bytes.clone();
        wrong[0] = b'X';
        assert_eq!(parse(&wrong), None);
        let mut no_origin = bytes;
        no_origin[4..36].fill(0);
        assert_eq!(parse(&no_origin), None);
        let unannounced = OfferId {
            clip: ClipId {
                origin: DeviceId([0; 32]),
                seq: 1,
            },
            generation: 1,
        };
        assert_eq!(encode(unannounced), None);
    }

    /// A 0.4.0 record (same layout, `OKC1`) left on the clipboard across an
    /// upgrade or downgrade is not a 0.3.x marker.
    #[test]
    fn records_of_the_040_line_are_not_parsed() {
        let mut v040 = encode(id()).unwrap();
        v040[..4].copy_from_slice(b"OKC1");
        assert_eq!(parse(&v040), None);
    }
}
