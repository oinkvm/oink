//! Game-only path submission and bounded per-game deduplication. No sharing
//! session, connection, pause or migration event is consumed here.
use oink_foundation::{DeviceId, session::Boot};
use oink_locator::aim::model::{AimMsg, GameEvent, GameInstance};
use std::collections::BTreeMap;

pub(super) fn obsolete_game(current: GameInstance, incoming: GameInstance) -> bool {
    current.authority_boot == incoming.authority_boot && incoming.game_counter <= current.game_counter
}

pub(super) fn message_event(message: &AimMsg) -> Option<GameEvent> {
    match message {
        AimMsg::Open { event }
        | AimMsg::Joined { event }
        | AimMsg::Declined { event }
        | AimMsg::Click { event, .. }
        | AimMsg::Close { event } => Some(*event),
        AimMsg::State(_) => None,
    }
}

pub(super) fn accept_event(processed: &mut BTreeMap<DeviceId, (Boot, u64)>, event: GameEvent) -> bool {
    let key = event.sender_boot.device_id;
    if event.event_seq == 0
        || processed
            .get(&key)
            .is_some_and(|(boot, seq)| *boot != event.sender_boot || event.event_seq <= *seq)
    {
        return false;
    }
    processed.insert(key, (event.sender_boot, event.event_seq));
    true
}
