//! `ConfigEmergencyPause` on a phone (change plan §2.3, M1). Every phone
//! acknowledges. Remote input is not queued in the core: each I0 message goes
//! to the input owner (`MobileInputSink`) on its link's receive task, so the
//! owner drops what it has not injected yet and releases everything
//! (`MobileInputSink::emergency_pause`). A phone has no local pause state of
//! its own, so its pause revision stays at the facts' initial 1.
use super::*;
use oink_network::member::Out;

const PAUSE_REVISION: u64 = 1;

pub(super) fn receive(
    state: &NetworkState,
    link: LinkId,
    peer: Boot,
    request: dto::EmergencyPause,
) -> Result<Vec<Out>, MobileNetworkError> {
    let event = request.event().map_err(protocol_start)?;
    let origin =
        oink_network::wire_identity::read_boot(&event.origin().map_err(protocol_start)?).map_err(protocol_start)?;
    let target =
        oink_network::wire_identity::read_boot(&request.target().map_err(protocol_start)?).map_err(protocol_start)?;
    // Only the requester's own current event, addressed to this Boot; a stale
    // one (another Boot) is ignored like the desktop does.
    if origin != peer || target != state.links.boot() {
        return Ok(Vec::new());
    }
    let sequence = event.sequence().map_err(protocol_start)?;
    let outcome = {
        let mut seen = state
            .emergencies
            .lock()
            .map_err(|_| MobileNetworkError::Stream("emergency mutex poisoned".into()))?;
        if seen.contains(&(origin, sequence)) {
            dto::EmergencyOutcome::AlreadySeen
        } else {
            if seen.len() >= 64 {
                seen.clear();
            }
            seen.insert((origin, sequence));
            dto::EmergencyOutcome::EnteredPause
        }
    };
    if outcome == dto::EmergencyOutcome::EnteredPause
        && let Some(sink) = state.input_sink.get()
    {
        sink.emergency_pause();
    }
    let ack = dto::EmergencyPauseAck::from_raw(
        [
            (1, event.write()),
            (2, oink_network::wire_identity::boot(state.links.boot()).write()),
            (3, outcome.write()),
            (4, PAUSE_REVISION.write()),
            (5, false.write()),
        ]
        .into(),
    )
    .map_err(protocol_start)?;
    Ok(vec![Out::Send {
        link,
        message: v2::Message::ConfigEmergencyPauseAck(ack),
    }])
}
