//! The opening of this phone's outbound I0 (protocol-v2 1.1, plan section 4).
//!
//! The main app always negotiates `oink.input/2`, so every member with input
//! waits for this phone's I0 opening. Whether the phone can be controlled is
//! not a capability: it is `InputCapability.controllable` in that opening (the
//! Android privileged helper ready and the screen unlocked). A PC reads it when
//! it attaches the I0 pair, so a change reopens the outbound I0 (its stream
//! machine in `outbound`) with a fresh opening instead of renegotiating or
//! dropping the link.
use super::{MobileNetwork, MobileNetworkError, NetworkState};
use oink_foundation::{DeviceId, session::Boot};
use oink_protocol::{
    frame::StreamKind,
    v2::{
        self, Message, dto,
        schema::{FixedBytes, List, WireDto, WireValue},
    },
};
use std::sync::{Arc, atomic::Ordering};

/// Supplies the I0 opening records. The embedded router replaces the default
/// (`MobileNetwork::set_input_opening`); this module decides when they are
/// sent and keeps `controllable` as given.
pub trait InputOpening: Send + Sync {
    fn opening(&self, local: Boot, controllable: bool) -> Vec<Message>;
}

/// The installed opening provider (opaque to `Debug`).
pub(super) struct Provider(pub(super) Arc<dyn InputOpening>);

impl std::fmt::Debug for Provider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InputOpening")
    }
}

/// The opening of a phone that is never a focus holder: its capability, a
/// default feel, an epoch-0 focus scope of itself and game mode off.
#[derive(Debug, Default)]
pub struct DefaultInputOpening;

impl InputOpening for DefaultInputOpening {
    fn opening(&self, local: Boot, controllable: bool) -> Vec<Message> {
        default_opening(local, controllable).expect("constant I0 opening")
    }
}

fn default_opening(local: Boot, controllable: bool) -> v2::Result<Vec<Message>> {
    let me = oink_network::wire_identity::device(local.device_id);
    let member = dto::FocusMember::from_raw([(1, me.write()), (2, FixedBytes(local.nonce).write())].into())?;
    let scope = dto::FocusScope::from_raw([(1, List::<_, 16>(vec![member]).write())].into())?;
    Ok(vec![
        Message::InputCapability(dto::InputCapability::from_raw([(1, controllable.write())].into())?),
        Message::ControlFeel(dto::ControlFeel::from_raw(
            [(1, dto::FeelProfile::from_raw(Default::default())?.write())].into(),
        )?),
        Message::ControlFocusState(dto::ControlFocusState::from_raw(
            [
                (1, 0u64.write()),
                (2, me.write()),
                (3, scope.write()),
                (4, FixedBytes(local.nonce).write()),
            ]
            .into(),
        )?),
        Message::ControlGameMode(dto::ControlGameMode::from_raw(
            [
                (1, 0u64.write()),
                (2, 0u64.write()),
                (3, dto::GameMode::Off.write()),
                (4, false.write()),
                (5, false.write()),
            ]
            .into(),
        )?),
    ])
}

/// Posts the opening on the current outbound I0 to `peer` (just opened by
/// its stream machine in `outbound`). `false`: the stream already refused it.
pub(super) fn post_opening(state: &NetworkState, peer: DeviceId) -> bool {
    let kind = StreamKind::Input;
    let senders = state.senders.lock().expect("mobile sender mutex poisoned");
    let Some(sender) = senders.get(&(peer, kind)) else {
        return false;
    };
    opening_messages(state).into_iter().all(|message| {
        v2::frame::encode(&message, kind.max_message())
            .ok()
            .is_some_and(|frame| sender.post(frame).is_ok())
    })
}

/// The records a fresh outbound I0 starts with: the provider's content with
/// the current `controllable`.
pub(super) fn opening_messages(state: &NetworkState) -> Vec<Message> {
    state
        .input_opening
        .read()
        .expect("input opening lock poisoned")
        .0
        .opening(state.links.boot(), state.controllable.load(Ordering::Acquire))
}

impl MobileNetwork {
    /// Whether a member may drive this phone: the Android privileged helper is
    /// ready and the screen is unlocked. Announced in each I0 opening; a change
    /// reopens the outbound I0s. Never a capability change.
    pub fn set_controllable(&self, controllable: bool) {
        if self.state.controllable.swap(controllable, Ordering::AcqRel) != controllable {
            super::outbound::reopen(&self.state, StreamKind::Input);
        }
    }

    pub fn controllable(&self) -> bool {
        self.state.controllable.load(Ordering::Acquire)
    }

    /// Install the provider of the I0 opening records (the embedded router).
    /// Opens sent afterwards use it; existing I0s are reopened with it.
    pub fn set_input_opening(&self, opening: Arc<dyn InputOpening>) -> Result<(), MobileNetworkError> {
        *self
            .state
            .input_opening
            .write()
            .map_err(|_| MobileNetworkError::Stream("input opening lock poisoned".into()))? = Provider(opening);
        super::outbound::reopen(&self.state, StreamKind::Input);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_opening_carries_controllable_and_an_own_epoch_zero_scope() {
        let local = Boot {
            device_id: DeviceId([3; 32]),
            nonce: [4; 16],
        };
        for controllable in [false, true] {
            let opening = DefaultInputOpening.opening(local, controllable);
            assert_eq!(opening.len(), 4);
            let Message::InputCapability(capability) = &opening[0] else {
                panic!("first opening record must be InputCapability");
            };
            assert_eq!(capability.controllable().unwrap(), controllable);
            let Message::ControlFocusState(focus) = &opening[2] else {
                panic!("third opening record must be FocusState");
            };
            assert_eq!(focus.epoch().unwrap(), 0);
            for message in &opening {
                v2::frame::encode(message, StreamKind::Input.max_message()).unwrap();
            }
        }
    }
}
