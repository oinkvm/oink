//! Fixed-storage v1 media frames. The transport owns the flow prefix; these
//! helpers encode/inspect only the registered AudioPacket and AudioSleep bodies.
use crate::{dsp, negotiation::Profile, source::Packet};
use oink_protocol::v1::{
    self, ErrorCode,
    frame::OpaqueFrame,
    hot::{self, Value},
};
use std::time::Instant;

pub const FRAME_CAPACITY: usize = v1::limits::DATAGRAM - 4;
/// Both audio uses share the same codec, packet body and native sink pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Speaker,
    Microphone,
}
impl Kind {
    pub fn to_wire(self) -> v1::dto::AudioMediaKind {
        match self {
            Self::Speaker => v1::dto::AudioMediaKind::Speaker,
            Self::Microphone => v1::dto::AudioMediaKind::Microphone,
        }
    }

    pub fn packet_id(self) -> u16 {
        match self {
            Self::Speaker => 0x0604,
            Self::Microphone => 0x0618,
        }
    }
    pub fn sleep_id(self) -> u16 {
        match self {
            Self::Speaker => 0x0606,
            Self::Microphone => 0x0619,
        }
    }
    pub fn from_frame(bytes: &[u8]) -> Option<Self> {
        match OpaqueFrame::parse_exact(bytes, FRAME_CAPACITY).ok()?.header.type_id {
            0x0604 | 0x0606 => Some(Self::Speaker),
            0x0618 | 0x0619 => Some(Self::Microphone),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Media<'a> {
    Packet {
        sequence: u64,
        burst: u64,
        source_time: u64,
        silent: bool,
        payload: &'a [u8],
    },
    Sleep {
        sequence: u64,
        burst: u64,
    },
}
pub fn encode(packet: &Packet, stream: u64, started: Instant, bytes: &mut [u8]) -> v1::Result<usize> {
    encode_kind(Kind::Speaker, packet, stream, started, bytes)
}
pub fn encode_kind(kind: Kind, packet: &Packet, stream: u64, started: Instant, bytes: &mut [u8]) -> v1::Result<usize> {
    if stream == 0 || packet.burst == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    if packet.sleep {
        hot::encode_into(
            kind.sleep_id(),
            &[
                (1, Value::Uint(stream)),
                (2, Value::Uint(packet.burst)),
                (3, Value::Uint(packet.ordinal)),
            ],
            bytes,
            FRAME_CAPACITY,
        )
    } else {
        let payload = packet.payload.get(..packet.len).ok_or(ErrorCode::LimitExceeded)?;
        let source_time = packet
            .encode_end
            .saturating_duration_since(started)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        hot::encode_into(
            kind.packet_id(),
            &[
                (1, Value::Uint(stream)),
                (2, Value::Uint(packet.ordinal)),
                (3, Value::Uint(packet.burst)),
                (4, Value::Uint(source_time)),
                (5, Value::Bool(packet.silent)),
                (6, Value::Bytes(payload)),
            ],
            bytes,
            FRAME_CAPACITY,
        )
    }
}
pub fn decode(bytes: &[u8], stream: u64, profile: Profile) -> v1::Result<Media<'_>> {
    decode_kind(Kind::Speaker, bytes, stream, profile)
}
pub fn decode_kind(kind: Kind, bytes: &[u8], stream: u64, profile: Profile) -> v1::Result<Media<'_>> {
    if stream == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let frame = OpaqueFrame::parse_exact(bytes, FRAME_CAPACITY)?;
    // Both registered kinds intentionally have the same field schema. Borrow
    // its projection directly after checking the discriminator; no owning tree.
    if frame.header.type_id == kind.packet_id() {
        let packet = hot::View::<v1::dto::AudioPacket>::parse(frame.payload(), FRAME_CAPACITY)?;
        let payload = packet.payload()?;
        if packet.stream_id()? != stream || packet.burst()? == 0 || !dsp::valid_payload(profile.config(), payload) {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        Ok(Media::Packet {
            sequence: packet.sequence()?,
            burst: packet.burst()?,
            source_time: packet.source_time_ns()?,
            silent: packet.silent()?,
            payload,
        })
    } else if frame.header.type_id == kind.sleep_id() {
        let packet = hot::View::<v1::dto::AudioSleep>::parse(frame.payload(), FRAME_CAPACITY)?;
        if packet.stream_id()? != stream || packet.burst()? == 0 {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        Ok(Media::Sleep {
            sequence: packet.sequence()?,
            burst: packet.burst()?,
        })
    } else {
        Err(ErrorCode::UnsupportedValue.into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_media_fields_do_not_allocate_error_paths() {
        let mut bytes = [0; FRAME_CAPACITY];
        let size = hot::encode_into(
            Kind::Speaker.packet_id(),
            &[
                (1, Value::Uint(1)),
                (2, Value::Uint(1)),
                (3, Value::Uint(1)),
                (4, Value::Uint(1)),
                (5, Value::Bool(false)),
                (6, Value::Bytes(&[0; 200])),
            ],
            &mut bytes,
            FRAME_CAPACITY,
        )
        .unwrap();
        bytes[8] = 0xf4; // stream_id becomes a bool after valid encoding
        crate::allocation_audit::assert_no_heap(|| {
            assert!(decode_kind(Kind::Speaker, &bytes[..size], 1, Profile::Opus5ms).is_err());
        });
        bytes[8] = 1;
        // Truncation, unexpected scalar types, invalid UTF-8 and malformed
        // container heads must be as cheap as valid media (including Cbor errors).
        for len in 0..size {
            crate::allocation_audit::assert_no_heap(|| {
                let _ = decode_kind(Kind::Speaker, &bytes[..len], 1, Profile::Opus5ms);
            });
        }
        for index in 0..size {
            let original = bytes[index];
            for value in 0..=255 {
                bytes[index] = value;
                crate::allocation_audit::assert_no_heap(|| {
                    let _ = decode_kind(Kind::Speaker, &bytes[..size], 1, Profile::Opus5ms);
                });
            }
            bytes[index] = original;
        }
    }
    #[test]
    fn captured_packet_encode_decode_and_payload_read_never_allocate() {
        for kind in [Kind::Speaker, Kind::Microphone] {
            let now = Instant::now();
            let mut packet = Packet {
                ordinal: u64::MAX,
                burst: u64::MAX,
                sleep: false,
                payload: [0; dsp::MAX_PACKET_BYTES],
                len: 480,
                silent: true,
                delivered_at: now,
                encode_start: now,
                encode_end: now,
                remaining_samples: 0,
            };
            let mut bytes = [0; FRAME_CAPACITY];
            crate::allocation_audit::assert_no_heap(|| {
                let len = encode_kind(kind, &packet, u64::MAX, now, &mut bytes).unwrap();
                let Media::Packet {
                    payload,
                    sequence,
                    burst,
                    ..
                } = decode_kind(kind, &bytes[..len], u64::MAX, Profile::Pcm16_2p5ms).unwrap()
                else {
                    panic!("media")
                };
                assert_eq!(payload.len(), 480);
                assert_eq!(sequence, u64::MAX);
                assert_eq!(burst, u64::MAX);
                assert!(decode_kind(kind, &bytes[..len], 1, Profile::Pcm16_2p5ms).is_err());
                packet.sleep = true;
                let len = encode_kind(kind, &packet, u64::MAX, now, &mut bytes).unwrap();
                assert_eq!(
                    decode_kind(kind, &bytes[..len], u64::MAX, Profile::Pcm16_2p5ms).unwrap(),
                    Media::Sleep {
                        sequence: u64::MAX,
                        burst: u64::MAX
                    }
                );
            });
            packet.sleep = false;
            packet.len = 479;
            let len = encode_kind(kind, &packet, 1, now, &mut bytes).unwrap();
            assert!(decode_kind(kind, &bytes[..len], 1, Profile::Pcm16_2p5ms).is_err());
            packet.len = 960;
            let len = encode_kind(kind, &packet, 1, now, &mut bytes).unwrap();
            assert!(len + 4 <= 1100);
            assert!(decode_kind(kind, &bytes[..len], 1, Profile::Pcm16_2p5ms).is_err());
        }
    }
}
