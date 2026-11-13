//! PC side adapter for the mobile-v040 seam.
//!
//! Production PC paths use this module to build the v2 audio endpoint
//! release message (a PC turns a phone back to local by authoring the global
//! audio configuration; AudioEndpointClose is retired), B0 clipboard head
//! updates and current-query results, and the guarded native publication of a
//! mobile upload. The proxy cache itself lives in `clip::mobile`.

use oink_clipboard::{ClipboardBackend, ClipboardError, NativeSpec, content::ReaderGuard};
use oink_foundation::{DeviceId, GroupId, session::Boot};
use oink_protocol::v2::{
    self, RawMap, RawValue, dto,
    schema::{FixedBytes, WireDto, WireValue},
};

/// Directory below the agent cache root reserved for completed mobile proxy
/// payloads. It is counted by the cache API and removed only on explicit clear.
pub const PROXY_CACHE_DIR: &str = "transfer-proxy";

/// Identity material needed to fill v2 DTOs.  The foundation identity stays
/// at this boundary; no stringly-typed identity is put on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2Identity {
    pub device: DeviceId,
    pub boot: Boot,
    pub group: GroupId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutboundMessage {
    pub peer: String,
    pub message: v2::Message,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("v2 DTO error: {0}")]
    Protocol(#[from] v2::Error),
}

/// Builds the PC-to-phone audio endpoint controls.
#[derive(Clone, Copy, Debug)]
pub struct MobileV2Adapter {
    identity: V2Identity,
}

impl MobileV2Adapter {
    pub fn new(identity: V2Identity) -> Self {
        Self { identity }
    }

    /// Release an endpoint owned by the caller. Mobile idle timeout and local
    /// stop use this v2 operation; the resulting AudioEndpointState carries
    /// the authoritative close reason (for example `Idle`).
    pub fn release_endpoint(
        &self,
        request_id: [u8; 16],
        target: Boot,
        direction: dto::EndpointDirectionKind,
        generation: u64,
        activation_id: Option<[u8; 16]>,
    ) -> Result<OutboundMessage, AdapterError> {
        let request = dto::AudioEndpointRelease::from_raw(
            [
                (1, v2_id16(request_id).write()),
                (2, v2_boot(self.identity.boot).write()),
                (3, v2_boot(target).write()),
                (4, direction.write()),
                (5, generation.write()),
                (6, activation_id.map(v2_id16).write()),
            ]
            .into(),
        )?;
        Ok(OutboundMessage {
            peer: hex(&target.device_id.0),
            message: v2::Message::AudioEndpointRelease(request),
        })
    }
}

/// PC side clipboard announcements and current-query responses.
#[derive(Clone, Copy, Debug)]
pub struct ClipboardPublisher {
    identity: V2Identity,
}

impl ClipboardPublisher {
    pub fn new(identity: V2Identity) -> Self {
        Self { identity }
    }

    pub fn head_update(&self, offer: dto::ClipboardOffer, fresh: bool) -> v2::Message {
        v2::Message::ClipboardHeadUpdate(
            dto::ClipboardHeadUpdate::from_raw(
                [
                    (1, v2_id16(self.identity.group.0).write()),
                    (2, v2_boot(self.identity.boot).write()),
                    (3, offer.write()),
                    (4, fresh.write()),
                ]
                .into(),
            )
            .expect("publisher creates valid head update"),
        )
    }

    pub fn current_result(
        &self,
        query: &dto::ClipboardCurrentQuery,
        status: dto::CurrentResultStatus,
        offer: Option<dto::ClipboardOffer>,
        source: Option<dto::DeviceId>,
        reason_code: Option<u16>,
        clock: u64,
    ) -> Result<v2::Message, AdapterError> {
        let mut raw = RawMap::from([
            (1, query.request_id()?.write()),
            (2, v2_boot(self.identity.boot).write()),
            (3, status.write()),
            (7, clock.write()),
        ]);
        if let Some(offer) = offer {
            raw.insert(4, offer.write());
        }
        if let Some(source) = source {
            raw.insert(5, source.write());
        }
        if let Some(reason_code) = reason_code {
            raw.insert(6, reason_code.write());
        }
        Ok(v2::Message::ClipboardCurrentResult(
            dto::ClipboardCurrentResult::from_raw(raw)?,
        ))
    }
}

fn v2_device(value: DeviceId) -> dto::DeviceId {
    v2_device_bytes(&value.0).expect("foundation DeviceId is always 32 bytes")
}

fn v2_device_bytes(value: &[u8]) -> Result<dto::DeviceId, v2::Error> {
    dto::DeviceId::from_raw([(1, RawValue::from(dto::ED25519)), (2, RawValue::Bytes(value.to_vec()))].into())
}

fn v2_boot(value: Boot) -> dto::Boot {
    dto::Boot::from_raw(
        [
            (1, v2_device(value.device_id).write()),
            (2, v2_id16(value.nonce).write()),
        ]
        .into(),
    )
    .expect("foundation Boot is canonical")
}

fn v2_id16(value: [u8; 16]) -> dto::Id16 {
    FixedBytes(value)
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

/// The native publication seam.  The caller supplies the platform-built
/// `NativeSpec` and its revision guard; this method is the only place the PC
/// proxy publishes mobile content into the system clipboard.
pub fn write_cached_mobile(
    backend: &dyn ClipboardBackend,
    spec: NativeSpec,
    guard: ReaderGuard,
) -> Result<u64, ClipboardError> {
    backend.write_mobile_guarded(spec, guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> V2Identity {
        let device = DeviceId([7; 32]);
        V2Identity {
            device,
            boot: Boot {
                device_id: device,
                nonce: [8; 16],
            },
            group: GroupId([9; 16]),
        }
    }

    #[test]
    fn current_response_is_a_v2_message() {
        let query = dto::ClipboardCurrentQuery::from_raw(
            [
                (1, v2_id16([5; 16]).write()),
                (2, v2_id16(identity().group.0).write()),
                (3, v2_boot(identity().boot).write()),
                (4, 8u16.write()),
            ]
            .into(),
        )
        .unwrap();
        let v2::Message::ClipboardCurrentResult(result) = ClipboardPublisher::new(identity())
            .current_result(&query, dto::CurrentResultStatus::Empty, None, None, None, 42)
            .unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(result.request_id().unwrap().0, [5; 16]);
        assert_eq!(result.status().unwrap(), dto::CurrentResultStatus::Empty);
        assert_eq!(result.clock().unwrap(), 42);
    }

    #[test]
    fn local_timeout_release_is_a_fenced_v2_audio_message() {
        let adapter = MobileV2Adapter::new(identity());
        let message = adapter
            .release_endpoint(
                [4; 16],
                identity().boot,
                dto::EndpointDirectionKind::MicrophoneSend,
                17,
                Some([5; 16]),
            )
            .unwrap();
        let v2::Message::AudioEndpointRelease(release) = message.message else {
            panic!("wrong message")
        };
        assert_eq!(release.generation().unwrap(), 17);
        assert_eq!(release.direction().unwrap(), dto::EndpointDirectionKind::MicrophoneSend);
        assert_eq!(release.activation_id().unwrap().unwrap().0, [5; 16]);
    }
}
