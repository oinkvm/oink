//! Signed group edits and the phone's commit pipeline: persist the signed
//! state, publish it to ready members, and retire C0 authority it removes.
//! Every membership decision comes from `oink_network::member`.

use super::*;
use oink_network::link::Disconnect;
use std::time::Duration;

impl MobileNetwork {
    pub fn rename_local(&self, name: String) -> Result<(), MobileNetworkError> {
        if name.trim().is_empty() || name.len() > 256 {
            return Err(MobileNetworkError::Protocol("invalid device name".into()));
        }
        self.edit_local_facts(|facts| {
            facts.insert(6, v2::schema::Text::<256>(name).write());
            Ok(())
        })?;
        Ok(())
    }

    pub fn remove_member(&self, device: DeviceId) -> Result<(), MobileNetworkError> {
        if device == self.boot().device_id {
            return Err(MobileNetworkError::Protocol(
                "use leave group for the local device".into(),
            ));
        }
        edit(&self.state, device)
    }

    pub fn leave_group(&self) -> Result<(), MobileNetworkError> {
        edit(&self.state, self.boot().device_id)
    }
}

fn edit(state: &Arc<NetworkState>, device: DeviceId) -> Result<(), MobileNetworkError> {
    let mut member = member_host::lock(state)?;
    if !member.edit_allowed().map_err(journal_error)? {
        return Err(MobileNetworkError::Protocol(
            "group edit conflicts with active pairing".into(),
        ));
    }
    let current = member_host::current(state)?;
    if !current.roster().contains(device) {
        return Err(MobileNetworkError::Protocol(
            "device is outside the current group".into(),
        ));
    }
    let local = state.identity.id();
    if device == local && current.roster().members().count() == 1 {
        return Err(MobileNetworkError::Protocol("device is already alone".into()));
    }
    let old = current.roster().anchor().group;
    let next = member
        .removal(&current, device, None, unix_ms())
        .map_err(protocol_start)?;
    commit(state, next, None)?;
    if device == local {
        // Pairings whose request named the group we left can no longer commit.
        let current = member_host::current(state)?;
        let outs = member.left(old, &current);
        member_host::execute(state, &mut member, None, outs)?;
    }
    Ok(())
}

/// Save and publish a signed state. A state without this device is a
/// departure: the removal is published and a fresh one-device group replaces
/// it locally; every C0 loses its authority and closes after draining.
pub(super) fn commit(
    state: &Arc<NetworkState>,
    candidate: State,
    source: Option<LinkId>,
) -> Result<(), MobileNetworkError> {
    let mut controls = state
        .controls
        .lock()
        .map_err(|_| MobileNetworkError::Stream("C0 owner unavailable".into()))?;
    let mut current = state
        .state
        .lock()
        .map_err(|_| MobileNetworkError::Stream("group state unavailable".into()))?;
    if candidate.wire() == current.wire() {
        return Ok(());
    }
    let local = state.identity.id();
    let departed = !candidate.roster().contains(local);
    let persisted = if departed {
        let advert = advert_for(state, &current);
        State::create(&state.identity, &advert.name, &advert.os, unix_ms()).map_err(protocol_start)?
    } else {
        candidate.clone()
    };
    let frame = v2::frame::encode(
        &v2::Message::ConfigState(candidate.wire().clone()),
        v2::limits::CONFIG_FRAME,
    )
    .map_err(protocol_start)?;
    persist_state(&state.state_path, &persisted)?;
    *current = persisted;

    for (&link, session) in controls.iter_mut() {
        if session.departing {
            continue;
        }
        let removed = !candidate.roster().contains(session.peer_boot.device_id);
        if session.ready && Some(link) != source && session.tx.post(frame.clone()).is_err() {
            state.links.disconnect(link, Disconnect::ControlStreamFailed);
        }
        if departed || removed {
            // Let the signed departure already queued on the reliable control
            // stream drain. Business streams and pending edits lose authority now.
            session.ready = false;
            session.departing = true;
            session.pending_controls.clear();
            state.links.retire_for_departure(link);
            let reason = if departed {
                Disconnect::LeavingGroup
            } else {
                Disconnect::MemberRemoved
            };
            if Some(link) == source {
                state.links.disconnect(link, reason);
                continue;
            }
            let links = state.links.clone();
            state.runtime.spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                links.disconnect(link, reason);
            });
        }
    }
    drop(current);
    drop(controls);
    if let Some(sink) = state.input_sink.get() {
        sink.layout_changed();
    }
    Ok(())
}

/// Our signed state went to a member we removed: business authority is gone
/// at once; the reply drains, then the C0 closes (bounded like a departure).
pub(super) fn retire_removed(state: &Arc<NetworkState>, link: LinkId) -> Result<(), MobileNetworkError> {
    let mut controls = state
        .controls
        .lock()
        .map_err(|_| MobileNetworkError::Stream("C0 owner unavailable".into()))?;
    let Some(session) = controls.get_mut(&link) else {
        return Ok(());
    };
    session.ready = false;
    session.departing = true;
    session.pending_controls.clear();
    state.links.retire_for_departure(link);
    let links = state.links.clone();
    state.runtime.spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        links.disconnect(link, Disconnect::MemberRemoved);
    });
    Ok(())
}

pub(super) fn journal_error(error: oink_network::pair_journal::Error) -> MobileNetworkError {
    MobileNetworkError::Protocol(error.to_string())
}
