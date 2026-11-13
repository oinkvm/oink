//! Executes the shared member decisions (`oink_network::member`) on the
//! phone: its C0 table, its group-state file and its pairing snapshot. No
//! membership decision is taken here; the desktop runs the same decisions.
use super::*;
use oink_network::member::{LinkFacts, Links, Membership, Out};
use std::sync::MutexGuard;

/// Lock order: member → controls → state.
pub(super) fn lock(state: &NetworkState) -> Result<MutexGuard<'_, Membership>, MobileNetworkError> {
    state
        .pairing
        .lock()
        .map_err(|_| MobileNetworkError::Stream("pairing mutex poisoned".into()))
}

pub(super) fn current(state: &NetworkState) -> Result<State, MobileNetworkError> {
    Ok(state
        .state
        .lock()
        .map_err(|_| MobileNetworkError::Stream("group state mutex poisoned".into()))?
        .clone())
}

pub(super) fn facts_of(session: &ControlSession) -> Option<LinkFacts> {
    Some(LinkFacts {
        boot: session.peer_boot,
        greeted: session.peer.as_ref()?.anchor().ok()?,
        ready: session.ready,
        departing: session.departing,
        admitted: session.admitted_anchor,
    })
}

/// A copy of the C0 table taken without holding it during execution.
pub(super) struct Snapshot(Vec<(LinkId, LinkFacts)>);
impl Snapshot {
    pub(super) fn take(state: &NetworkState) -> Result<Self, MobileNetworkError> {
        let controls = state
            .controls
            .lock()
            .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
        Ok(Self(
            controls
                .iter()
                .filter_map(|(id, session)| Some((*id, facts_of(session)?)))
                .collect(),
        ))
    }
}
impl Links for Snapshot {
    fn facts(&self, link: LinkId) -> Option<LinkFacts> {
        self.0
            .iter()
            .find(|(id, _)| *id == link)
            .map(|(_, facts)| facts.clone())
    }
    fn greeted(&self) -> Vec<(LinkId, LinkFacts)> {
        self.0.clone()
    }
}

/// Post on this exact C0.
pub(super) fn post(state: &NetworkState, link: LinkId, message: &v2::Message) -> Result<(), MobileNetworkError> {
    let frame = v2::frame::encode(message, v2::limits::CONFIG_FRAME).map_err(protocol_start)?;
    state
        .controls
        .lock()
        .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?
        .get(&link)
        .ok_or_else(|| MobileNetworkError::Stream("C0 retired".into()))?
        .tx
        .post(frame)
        .map_err(|error| MobileNetworkError::Stream(error.to_string()))
}

/// Our admission Request on this C0 with the current signed state.
fn begin_admission(state: &NetworkState, link: LinkId) -> Result<(), MobileNetworkError> {
    let mut controls = state
        .controls
        .lock()
        .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
    let local = current(state)?;
    let session = controls
        .get_mut(&link)
        .ok_or_else(|| MobileNetworkError::Stream("C0 retired".into()))?;
    let request = admission::request(&local, state.identity.id(), 0).map_err(protocol_start)?;
    session.sent = Some(local);
    session
        .tx
        .post(v2::frame::encode(&request, v2::limits::CONFIG_FRAME).map_err(protocol_start)?)
        .map_err(|error| MobileNetworkError::Stream(error.to_string()))
}

/// Admission finished on this C0.
fn ready(state: &Arc<NetworkState>, member: &mut Membership, link: LinkId) -> Result<(), MobileNetworkError> {
    let current = current(state)?;
    let peer = {
        let mut controls = state
            .controls
            .lock()
            .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
        let session = controls
            .get_mut(&link)
            .ok_or_else(|| MobileNetworkError::Stream("C0 retired".into()))?;
        state.links.set_ready(link);
        session.ready = true;
        session.admitted_anchor = Some(current.roster().anchor());
        facts::send(state, session, current.roster().anchor().group)?;
        #[cfg(test)]
        eprintln!("mobile C0 admission Ready peer={}", session.peer_boot.device_id);
        session.peer_boot
    };
    let outs = member.ready(link, peer, &current).map_err(protocol_start)?;
    execute(state, member, Some(link), outs)
}

/// Install the journal's committed projection.
fn project(state: &Arc<NetworkState>, member: &mut Membership) -> Result<(), MobileNetworkError> {
    let current = current(state)?;
    let ready = {
        let controls = state
            .controls
            .lock()
            .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
        controls
            .iter()
            .filter(|(_, s)| s.ready && !s.departing)
            .map(|(id, s)| (*id, s.peer_boot.device_id))
            .collect::<Vec<_>>()
    };
    let Some(projection) = member.projection(&current, &ready).map_err(protocol_start)? else {
        let outs = member
            .resume(&current, &Snapshot::take(state)?)
            .map_err(protocol_start)?;
        return execute(state, member, None, outs);
    };
    // The transition proof goes out on every old ready C0 first.
    execute(state, member, None, projection.outs)?;
    {
        let mut controls = state
            .controls
            .lock()
            .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?;
        for link in projection.retire {
            if let Some(session) = controls.get_mut(&link) {
                session.ready = false;
                state.links.retire(link);
            }
        }
        persist_state(&state.state_path, &projection.candidate)?;
        *state
            .state
            .lock()
            .map_err(|_| MobileNetworkError::Stream("group state mutex poisoned".into()))? = projection.candidate;
    }
    if let Some(sink) = state.input_sink.get() {
        sink.layout_changed();
    }
    #[cfg(test)]
    eprintln!("mobile merge projection persisted");
    let current = self::current(state)?;
    let outs = member
        .projected(projection.digest, &current, &Snapshot::take(state)?)
        .map_err(protocol_start)?;
    execute(state, member, None, outs)
}

/// Run the shared decisions' actions in order.
pub(super) fn execute(
    state: &Arc<NetworkState>,
    member: &mut Membership,
    origin: Option<LinkId>,
    outs: Vec<Out>,
) -> Result<(), MobileNetworkError> {
    for out in outs {
        match out {
            Out::Send { link, message } => post(state, link, &message)?,
            // A peer whose C0 is gone has nothing left to receive.
            Out::SendPeer { peer, message } => {
                let _ = post_control(state, peer.device_id, message);
            }
            // The phone keeps no admission deadlines; the peer bounds them.
            Out::ArmAdmission { .. } | Out::AwaitRemoved { .. } => {}
            Out::OldMemberLane { link } => state.links.set_old_member_lane(link, true),
            Out::BeginAdmission { link } => begin_admission(state, link)?,
            Out::AdmitPeer { peer } => {
                let link = state
                    .peer_links
                    .lock()
                    .map_err(|_| MobileNetworkError::Stream("peer link mutex poisoned".into()))?
                    .get(&peer.device_id)
                    .copied();
                if let Some(link) = link {
                    begin_admission(state, link)?;
                }
            }
            Out::Commit(candidate) => membership::commit(state, candidate, origin)?,
            Out::Accepted { link, state: accepted } => {
                if let Some(session) = state
                    .controls
                    .lock()
                    .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?
                    .get_mut(&link)
                {
                    session.accepted = Some(accepted);
                }
                state.links.set_half_open(link);
            }
            Out::HalfOpen { link } => state.links.set_half_open(link),
            Out::Ready { link } => ready(state, member, link)?,
            Out::SendStateIfDiffers { link, acknowledged } => {
                let current = current(state)?;
                if current.digest().map_err(protocol_start)? != acknowledged {
                    post(state, link, &v2::Message::ConfigState(current.wire().clone()))?;
                }
            }
            Out::RetireRemoved { link } => membership::retire_removed(state, link)?,
            Out::CloseMembers { devices, reason } => {
                let links = state
                    .controls
                    .lock()
                    .map_err(|_| MobileNetworkError::Stream("control mutex poisoned".into()))?
                    .iter()
                    .filter_map(|(id, s)| devices.contains(&s.peer_boot.device_id).then_some(*id))
                    .collect::<Vec<_>>();
                for link in links {
                    state.links.disconnect(link, reason);
                }
            }
            // The commit pipeline turns a state without this device into a
            // fresh group and retires every C0.
            Out::Revoked(candidate) => membership::commit(state, candidate, origin)?,
            Out::Project => project(state, member)?,
            Out::Completed { pair, bundle, .. } => {
                let current = current(state)?;
                // The host reads outcomes from the pairing snapshot; the
                // journal keeps no outbox for it.
                let (event, _, outs) = member
                    .complete(pair, &bundle, [(1, 1u32.write())].into(), &current)
                    .map_err(membership::journal_error)?;
                member.journal.acknowledge(event).map_err(membership::journal_error)?;
                execute(state, member, origin, outs)?;
            }
            // The pairing snapshot carries these; migration has no phone UI yet.
            Out::PairingChanged | Out::PairingFailed { .. } | Out::RecoveredFailure { .. } | Out::Migration(_) => {}
        }
    }
    Ok(())
}

/// A pairing session operation (API command), then its effects.
pub(super) fn pairing(
    state: &Arc<NetworkState>,
    run: impl FnOnce(&mut pairing::Sessions) -> pairing::Result<Vec<pairing::Effect>>,
) -> Result<(), MobileNetworkError> {
    let mut member = lock(state)?;
    let effects = run(&mut member.pairs).map_err(pairing_error)?;
    let current = current(state)?;
    let outs = member.pairing_effects(effects, &current);
    execute(state, &mut member, None, outs)
}

/// Periodic owner tick of the shared decisions.
pub(super) fn tick(state: &Arc<NetworkState>) -> Result<(), MobileNetworkError> {
    let mut member = lock(state)?;
    let current = current(state)?;
    let links = Snapshot::take(state)?;
    let outs = member.tick(Instant::now(), &current, &links);
    execute(state, &mut member, None, outs)
}
