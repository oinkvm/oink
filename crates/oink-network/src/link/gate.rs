//! The send gate (protocol-v2 1.1): the registry rule a receiver enforces in
//! `admission::Context::admit`, applied once to every frame before it leaves.
//! A frame whose type has no route on this stream kind and transport, or whose
//! stream, message or route capabilities were not negotiated on this link, is
//! refused with a typed error instead of reaching the peer. Modules do not
//! repeat the check with their own capability strings.
//!
//! The registry also owns pause policy: while this device's sharing is paused
//! (`LinkLayer::set_sharing_paused`), a message whose policy is `Suspend`
//! does not leave either; `Allow` messages (answers, refusals, state) do.
//!
//! The allowed type ids are computed when the stream opens (and, for C0, when
//! the link's agreement arrives), so the per-frame check is a lock-free load and
//! a binary search: safe on realtime writer threads.
use arc_swap::ArcSwap;
use oink_protocol::v2::{
    admission::Agreement,
    registry::{self, PausePolicy, Transport},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Why the gate kept a frame back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// No route on this stream, or the link did not negotiate it.
    Unnegotiated,
    /// This device's sharing is paused and the message's policy is `Suspend`.
    Paused,
}

#[derive(Debug)]
pub(super) struct Gate {
    kind: u16,
    transport: Transport,
    /// `None`: an in-process test pair without a link; nothing is refused.
    allowed: Option<ArcSwap<Box<[u16]>>>,
    /// This device's sharing pause, shared by every gate of the link layer.
    paused: Arc<AtomicBool>,
}

impl Gate {
    /// In-process handle pairs (`link::testing`) have no link or agreement.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn unrestricted(kind: u16, transport: Transport) -> Arc<Self> {
        Arc::new(Self {
            kind,
            transport,
            allowed: None,
            paused: Arc::new(AtomicBool::new(false)),
        })
    }
    /// A stream opened on a link with this negotiated agreement.
    pub(super) fn negotiated(
        agreement: &Agreement,
        kind: u16,
        transport: Transport,
        paused: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            kind,
            transport,
            allowed: Some(ArcSwap::from_pointee(allowed(
                &|capability| agreement.contains(capability),
                kind,
                transport,
            ))),
            paused,
        })
    }
    /// C0 before its C0 negotiation reaches the link layer: only messages
    /// whose capabilities are the v2 baseline (admission, pairing, group
    /// state). Any negotiated link has those.
    pub(super) fn baseline(kind: u16, paused: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            transport: Transport::Reliable,
            allowed: Some(ArcSwap::from_pointee(allowed(
                &|capability| registry::BASELINE.contains(&capability),
                kind,
                Transport::Reliable,
            ))),
            paused,
        })
    }
    /// The link's C0 negotiation completed (`LinkLayer::set_agreement`).
    pub(super) fn update(&self, agreement: &Agreement) {
        if let Some(allowed_ids) = &self.allowed {
            allowed_ids.store(Arc::new(allowed(
                &|capability| agreement.contains(capability),
                self.kind,
                self.transport,
            )));
        }
    }
    /// Whether one encoded frame (6-byte header first) may be sent now.
    pub(super) fn check(&self, encoded: &[u8]) -> Result<(), Refusal> {
        let Some(allowed_ids) = &self.allowed else {
            return Ok(());
        };
        let [low, high, ..] = encoded else {
            return Err(Refusal::Unnegotiated);
        };
        let id = u16::from_le_bytes([*low, *high]);
        if allowed_ids.load().binary_search(&id).is_err() {
            return Err(Refusal::Unnegotiated);
        }
        if self.paused.load(Ordering::Acquire) && suspended(id) {
            return Err(Refusal::Paused);
        }
        Ok(())
    }
}

/// The registry pause policy of a registered message: `Suspend` messages do
/// not leave while this device's sharing is paused.
pub(super) fn suspended(id: u16) -> bool {
    registry::message(id).is_some_and(|entry| entry.pause == PausePolicy::Suspend)
}

fn allowed(enabled: &dyn Fn(&str) -> bool, kind: u16, transport: Transport) -> Box<[u16]> {
    let mut ids: Vec<u16> = registry::MESSAGES
        .iter()
        .map(|entry| entry.id)
        .filter(|id| registry::permits(enabled, *id, kind, transport))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.into_boxed_slice()
}

/// Whether the stream gate of `kind` is negotiated in `agreement`.
pub(super) fn opens(agreement: &Agreement, kind: u16) -> bool {
    registry::stream(kind).is_some_and(|stream| stream.capabilities.iter().all(|c| agreement.contains(c)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v2::registry::type_id;

    fn agreement(capabilities: &[&str]) -> Agreement {
        Agreement::new(capabilities.iter().map(|c| c.to_string())).unwrap()
    }
    fn frame(id: u16) -> Vec<u8> {
        let mut bytes = id.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[1, 0, 0, 0, 0xa0]);
        bytes
    }

    fn running() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn a_negotiated_gate_admits_only_routed_and_negotiated_types() {
        let clipboard = agreement(&["oink.core/2", "oink.group/2", "oink.clipboard/2"]);
        let gate = Gate::negotiated(&clipboard, 4, Transport::Reliable, running());
        assert_eq!(gate.check(&frame(type_id::ClipboardOffer)), Ok(()));
        // Registered, but only on Low (and needs oink.low/1).
        assert_eq!(
            gate.check(&frame(type_id::ClipboardShareTextChunk)),
            Err(Refusal::Unnegotiated)
        );
        // Registered on another stream kind.
        assert_eq!(gate.check(&frame(type_id::ConfigFacts)), Err(Refusal::Unnegotiated));
        // Unregistered and malformed frames never leave.
        assert_eq!(gate.check(&frame(0x0343)), Err(Refusal::Unnegotiated));
        assert_eq!(gate.check(&[0x00]), Err(Refusal::Unnegotiated));
    }

    #[test]
    fn c0_widens_from_baseline_to_its_agreement() {
        let gate = Gate::baseline(2, running());
        assert_eq!(gate.check(&frame(type_id::ConfigRequest)), Ok(()));
        assert!(gate.check(&frame(type_id::ConfigSetLocalOption)).is_err());
        gate.update(&agreement(&["oink.core/2", "oink.group/2", "oink.device/2"]));
        assert_eq!(gate.check(&frame(type_id::ConfigSetLocalOption)), Ok(()));
        // Exit volume also needs oink.audio/2, which this link did not negotiate.
        assert!(gate.check(&frame(type_id::ConfigSetExitVolume)).is_err());
    }

    #[test]
    fn datagram_types_are_checked_against_the_datagram_routes() {
        let audio = agreement(&["oink.core/2", "oink.group/2", "oink.audio/2"]);
        let gate = Gate::negotiated(&audio, 5, Transport::Datagram, running());
        assert_eq!(gate.check(&frame(type_id::AudioPacket)), Ok(()));
        // A microphone packet also needs oink.audio.microphone/1.
        assert!(gate.check(&frame(type_id::MicrophonePacket)).is_err());
        assert!(gate.check(&frame(type_id::AudioState)).is_err());
        assert_eq!(
            Gate::unrestricted(5, Transport::Datagram).check(&frame(type_id::MicrophonePacket)),
            Ok(())
        );
    }

    /// Audit (pause): the registry pause policy is the runtime rule. While
    /// this device is paused, Suspend messages stay back and Allow messages
    /// (refusals, state, C0) still leave; resuming lets Suspend through again.
    #[test]
    fn a_paused_device_sends_only_allow_messages() {
        let paused = running();
        let audio = agreement(&["oink.core/2", "oink.group/2", "oink.audio/2"]);
        let control = Gate::negotiated(&audio, 5, Transport::Reliable, paused.clone());
        let media = Gate::negotiated(&audio, 5, Transport::Datagram, paused.clone());
        let config = Gate::baseline(2, paused.clone());
        config.update(&audio);
        paused.store(true, Ordering::Release);
        assert_eq!(media.check(&frame(type_id::AudioPacket)), Err(Refusal::Paused));
        assert_eq!(
            control.check(&frame(type_id::AudioSourceRouteOffer)),
            Err(Refusal::Paused)
        );
        assert_eq!(control.check(&frame(type_id::AudioPrepareRefused)), Ok(()));
        assert_eq!(control.check(&frame(type_id::AudioState)), Ok(()));
        assert_eq!(config.check(&frame(type_id::ConfigFacts)), Ok(()));
        // Negotiation still comes first.
        assert_eq!(
            control.check(&frame(type_id::MicrophoneDemand)),
            Err(Refusal::Unnegotiated)
        );
        paused.store(false, Ordering::Release);
        assert_eq!(media.check(&frame(type_id::AudioPacket)), Ok(()));
    }

    #[test]
    fn stream_gate_follows_the_registry_table() {
        let core = agreement(&["oink.core/2", "oink.group/2"]);
        assert!(opens(&core, 2));
        assert!(!opens(&core, 4));
        assert!(!opens(&core, 13));
        let layout = agreement(&["oink.core/2", "oink.group/2", "oink.layout/2"]);
        assert!(opens(&layout, 13));
        assert!(!opens(&layout, 0xf0));
    }
}
