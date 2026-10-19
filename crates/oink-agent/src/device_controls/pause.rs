//! Session-local pause state. An emergency high-water survives local resume and
//! same-Boot reconnect; retiring membership or the origin Boot clears it.
use super::transactions::Requester;
use oink_protocol::v1::{
    RawMap, dto,
    schema::{WireDto, WireValue},
};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct PauseOwner {
    paused: bool,
    reason: dto::PauseReason,
    revision: u64,
    emergency_origin: Option<dto::DeviceId>,
    high_water: BTreeMap<Requester, u64>,
    event_sequence: u64,
}
impl Default for PauseOwner {
    fn default() -> Self {
        Self {
            paused: false,
            reason: dto::PauseReason::None,
            revision: 1,
            emergency_origin: None,
            high_water: BTreeMap::new(),
            event_sequence: 0,
        }
    }
}
impl PauseOwner {
    pub fn input_backlog(&mut self) {
        self.revision = self.revision.checked_add(1).expect("pause revision exhausted");
        self.paused = true;
        self.reason = dto::PauseReason::InputBacklog;
        self.emergency_origin = None;
    }
    pub fn state(&self) -> dto::LocalPauseState {
        let mut raw: RawMap = [
            (1, self.paused.write()),
            (2, self.reason.write()),
            (3, self.revision.write()),
        ]
        .into_iter()
        .collect();
        if let Some(origin) = &self.emergency_origin {
            raw.insert(4, origin.write());
        }
        dto::LocalPauseState::from_raw(raw).expect("owner pause state is valid")
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn paused(&self) -> bool {
        self.paused
    }
    pub fn command(
        &mut self,
        expected: u64,
        paused: bool,
        remote: bool,
    ) -> Result<dto::ControlOutcome, dto::ControlFailure> {
        if expected != self.revision {
            return Err(dto::ControlFailure::StaleRevision);
        }
        if paused == self.paused {
            return Ok(dto::ControlOutcome::NoChange);
        }
        self.revision = self.revision.checked_add(1).ok_or(dto::ControlFailure::ApplyFailed)?;
        self.paused = paused;
        self.reason = if !paused {
            dto::PauseReason::None
        } else if remote {
            dto::PauseReason::RemoteUser
        } else {
            dto::PauseReason::LocalUser
        };
        self.emergency_origin = None;
        Ok(dto::ControlOutcome::Applied)
    }
    /// Call only after checking current group admission and the addressed Boot.
    /// A duplicate never pauses a device that has since resumed independently.
    pub fn emergency(
        &mut self,
        origin: Requester,
        origin_id: dto::DeviceId,
        sequence: u64,
    ) -> Result<dto::EmergencyOutcome, dto::ControlFailure> {
        if sequence == 0 {
            return Err(dto::ControlFailure::InvalidValue);
        }
        if self.high_water.get(&origin).is_some_and(|n| *n >= sequence) {
            return Ok(dto::EmergencyOutcome::AlreadySeen);
        }
        if !self.high_water.contains_key(&origin) && self.high_water.len() >= 16 {
            return Err(dto::ControlFailure::NotMember);
        }
        let outcome = if self.paused {
            dto::EmergencyOutcome::AlreadyPaused
        } else {
            dto::EmergencyOutcome::EnteredPause
        };
        // A new emergency advances the fence even if ordinary pause was active:
        // a previously issued resume must not undo this newer safety decision.
        self.revision = self.revision.checked_add(1).ok_or(dto::ControlFailure::ApplyFailed)?;
        self.high_water.insert(origin, sequence);
        self.paused = true;
        self.reason = dto::PauseReason::Emergency;
        self.emergency_origin = Some(origin_id);
        Ok(outcome)
    }
    pub fn next_event_sequence(&mut self) -> Result<u64, dto::ControlFailure> {
        self.event_sequence = self
            .event_sequence
            .checked_add(1)
            .ok_or(dto::ControlFailure::ApplyFailed)?;
        Ok(self.event_sequence)
    }
    pub fn retain_origins(&mut self, mut admitted: impl FnMut(Requester) -> bool) {
        self.high_water.retain(|origin, _| admitted(*origin));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::{
        RawValue,
        schema::{Ascii, Bytes},
    };
    fn origin() -> (Requester, dto::DeviceId) {
        (
            Requester {
                device: oink_foundation::DeviceId([1; 32]).into(),
                nonce: [2; 16],
            },
            dto::DeviceId::from_raw(
                [
                    (1, Ascii::<96>("ed25519".into()).write()),
                    (2, Bytes::<4096>(vec![1; 32]).write()),
                ]
                .into_iter()
                .collect(),
            )
            .unwrap(),
        )
    }
    #[test]
    fn backlog_is_local_explicit_and_fences_an_older_resume() {
        let mut state = PauseOwner::default();
        let old = state.revision();
        state.input_backlog();
        let wire = state.state();
        assert!(wire.paused().unwrap());
        assert_eq!(wire.reason().unwrap(), dto::PauseReason::InputBacklog);
        assert_eq!(wire.reason().unwrap().write(), RawValue::from(5_u64));
        assert_eq!(
            state.command(old, false, false),
            Err(dto::ControlFailure::StaleRevision)
        );
        assert_eq!(
            state.event_sequence, 0,
            "local backlog never originates group emergency"
        );
        assert!(state.emergency_origin.is_none());
        state.command(state.revision(), false, false).unwrap();
        assert_eq!(state.state().reason().unwrap(), dto::PauseReason::None);
    }

    #[test]
    fn emergency_fences_stale_resume_and_duplicate_does_not_resume_or_repause() {
        let mut state = PauseOwner::default();
        let (origin, id) = origin();
        assert_eq!(state.command(1, true, true), Ok(dto::ControlOutcome::Applied));
        let old = state.revision();
        assert_eq!(
            state.emergency(origin, id.clone(), 1),
            Ok(dto::EmergencyOutcome::AlreadyPaused)
        );
        assert_eq!(state.command(old, false, true), Err(dto::ControlFailure::StaleRevision));
        state.command(state.revision(), false, false).unwrap();
        assert_eq!(
            state.emergency(origin, id.clone(), 1),
            Ok(dto::EmergencyOutcome::AlreadySeen)
        );
        assert!(!state.paused());
        assert_eq!(state.emergency(origin, id, 2), Ok(dto::EmergencyOutcome::EnteredPause));
        assert!(state.paused());
    }
    #[test]
    fn stale_no_change_is_refused_and_new_session_is_unpaused() {
        let mut state = PauseOwner::default();
        assert_eq!(state.command(0, false, true), Err(dto::ControlFailure::StaleRevision));
        assert_eq!(state.command(1, false, true), Ok(dto::ControlOutcome::NoChange));
        assert_eq!(state.state().raw().get(&1), Some(&RawValue::Bool(false)));
        let (origin, id) = origin();
        state.emergency(origin, id, 1).unwrap();
        assert!(!PauseOwner::default().paused());
        assert_eq!(PauseOwner::default().revision(), 1);
    }
}
