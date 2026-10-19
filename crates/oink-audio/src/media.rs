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
    if stream == 0 || packet.burst == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    if packet.sleep {
        hot::encode_into(
            0x0606,
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
            0x0604,
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
    if stream == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let frame = OpaqueFrame::parse_exact(bytes, FRAME_CAPACITY)?;
    match hot::Message::decode(frame)? {
        hot::Message::AudioPacket(packet) if packet.stream_id()? == stream => {
            let payload = packet.payload()?;
            if packet.burst()? == 0 || !dsp::valid_payload(profile.config(), payload) {
                return Err(ErrorCode::UnsupportedValue.into());
            }
            Ok(Media::Packet {
                sequence: packet.sequence()?,
                burst: packet.burst()?,
                source_time: packet.source_time_ns()?,
                silent: packet.silent()?,
                payload,
            })
        }
        hot::Message::AudioSleep(packet) if packet.stream_id()? == stream && packet.burst()? != 0 => Ok(Media::Sleep {
            sequence: packet.sequence()?,
            burst: packet.burst()?,
        }),
        _ => Err(ErrorCode::UnsupportedValue.into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_packet_encode_decode_and_payload_read_never_allocate() {
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
            let len = encode(&packet, u64::MAX, now, &mut bytes).unwrap();
            let Media::Packet {
                payload,
                sequence,
                burst,
                ..
            } = decode(&bytes[..len], u64::MAX, Profile::Pcm16_2p5ms).unwrap()
            else {
                panic!("media")
            };
            assert_eq!(payload.len(), 480);
            assert_eq!(sequence, u64::MAX);
            assert_eq!(burst, u64::MAX);
            assert!(decode(&bytes[..len], 1, Profile::Pcm16_2p5ms).is_err());
            packet.sleep = true;
            let len = encode(&packet, u64::MAX, now, &mut bytes).unwrap();
            assert_eq!(
                decode(&bytes[..len], u64::MAX, Profile::Pcm16_2p5ms).unwrap(),
                Media::Sleep {
                    sequence: u64::MAX,
                    burst: u64::MAX
                }
            );
        });
        packet.sleep = false;
        packet.len = 479;
        let len = encode(&packet, 1, now, &mut bytes).unwrap();
        assert!(decode(&bytes[..len], 1, Profile::Pcm16_2p5ms).is_err());
        packet.len = 960;
        let len = encode(&packet, 1, now, &mut bytes).unwrap();
        assert!(len + 4 <= 1100);
        assert!(decode(&bytes[..len], 1, Profile::Pcm16_2p5ms).is_err());
    }
}
