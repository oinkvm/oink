//! Business owners manage their streams independently: each outbound stream
//! opens with current state and retires on its own failure. Opening messages
//! are built by each owner; the shared hooks do not expose domain payloads.

#[cfg(all(test, debug_assertions))]
pub(crate) mod lifecycle_tests;

pub mod audio;
pub mod clipboard;
pub mod diagnostics;
pub mod dnd;
pub mod input;
pub mod machine;
pub mod projection;
pub mod wallpaper;
pub(crate) mod wire;

use oink_foundation::DeviceId;
use oink_network::link::{InboundLow, InboundStream, LinkId, LinkLost, StreamClosed, StreamId};
use oink_protocol::frame::StreamKind;

/// What every business module's stream task provides. One task per module on the
/// ordinary thread pool; it owns the module's `ModulePort`, its `ConfigChanges`
/// subscription and one [`machine::StreamMachine`] per peer and outbound kind, and calls
/// these in the order the notifications arrive. Nothing here may wait for another module.
///
pub trait StreamModule {
    /// Kinds this module registers with the network layer.
    const KINDS: &'static [StreamKind];

    /// A link became ready. The module may now open streams on it, if this device is not
    /// paused (modules that obey pause). No version is compared and no answer awaited.
    fn link_ready(&mut self, link: LinkId, peer: DeviceId);

    /// The link is gone; all its streams are already closed. Do the module's own clean-up.
    /// Ordinals restart with the next link.
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, reason: LinkLost);

    /// A stream of the peer arrived. Accept it, or reject it with a code. A larger
    /// ordinal replaces the older stream of the same kind: close the old one, clear the
    /// receive state of that kind, rebuild it from the new stream's opening state. An
    /// ordinal not larger than one already seen is rejected with `StaleOrdinal`
    /// ([`machine::InboundOrdinal`]). A module whose streams close while this
    /// device is paused (B0, I0) rejects a new one with `StreamCode::Paused`
    /// meanwhile, so the opener fails at once with the reason; on resume (the
    /// paused peer's next stream or facts) the opener's machine reopens.
    fn inbound(&mut self, stream: InboundStream);

    /// A low-priority operation of the peer arrived. Answered also while paused.
    fn inbound_low(&mut self, operation: InboundLow);

    /// One of this module's handles closed. Outbound: feed the machine (`Busy` and
    /// `Peer(code)`) and release the outbound state. Inbound: clear the receive state
    /// of that kind and cancel what depends on it. A notification for an old
    /// handle never acts on the stream that replaced it.
    fn stream_closed(&mut self, stream: StreamId, reason: StreamClosed);

    /// The applied pause (or the local emergency flag) changed. Pausing: close the
    /// streams this module writes on every link, cancel its own low-priority operations.
    /// B0 also closes the streams the peers write, with `StreamCode::Paused`: their
    /// requests could not be answered (I0's are closed by the input owner).
    /// While paused, content on streams the peer writes follows these rules:
    /// state messages are recorded and take effect on resume, clean-up messages
    /// are processed, action messages are ignored. Resuming: streams may be opened again.
    fn pause_changed(&mut self, paused: bool);

    /// Facts of `peer` arrived or changed. Only when `capabilities_changed`: re-evaluate
    /// whether to open streams to it and forget "unsupported". A module whose
    /// streams a paused peer refuses (B0, I0) also gives the machine the peer's
    /// pause from them ([`machine::StreamMachine::peer_facts`]).
    fn facts_changed(&mut self, peer: DeviceId, capabilities_changed: bool);
}

/// The peer's own pause in the Facts of its current Boot; `None` while there
/// are none (retained Facts of an older Boot do not count).
pub(crate) fn peer_facts_paused(config: &crate::config_module::ConfigModule, peer: DeviceId) -> Option<bool> {
    let boot = config.peer_boot(peer)?;
    let members = config.members();
    let facts = members.members.iter().find(|m| m.device == peer)?.facts.as_ref()?;
    if oink_network::wire_identity::read_boot(&facts.boot().ok()?).ok()? != boot {
        return None;
    }
    Some(
        facts
            .pause()
            .ok()
            .flatten()
            .is_some_and(|pause| pause.paused() == Ok(true)),
    )
}
