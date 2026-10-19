//! Live output volume belongs to the selected exit, not to persisted source gain.
//! This owner fences absolute writes with Boot, claim, output generation and
//! revision, and reports the actual readback rather than an optimistic slider.
use oink_protocol::v1::{
    RawMap, RawValue,
    dto::{
        Boot, ControlFailure, ControlOutcome, ExitClaimStamp, ExitVolumeResult, ExitVolumeState, ExitVolumeStatus,
        ExitVolumeVersion, SetExitVolume,
    },
    schema::{WireDto, WireValue},
};
use std::collections::VecDeque;
mod keys;
pub use keys::{KeyAction, KeyEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub percent: u8,
    pub muted: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub status: ExitVolumeStatus,
    pub native_code: Option<i64>,
}
impl Fault {
    pub const fn unavailable() -> Self {
        Self {
            status: ExitVolumeStatus::Unavailable,
            native_code: None,
        }
    }
    pub fn reason(self) -> ControlFailure {
        match self.status {
            ExitVolumeStatus::Unsupported => ControlFailure::VolumeUnsupported,
            ExitVolumeStatus::Unavailable => ControlFailure::OutputUnavailable,
            ExitVolumeStatus::PermissionDenied => ControlFailure::PermissionDenied,
            _ => ControlFailure::ApplyFailed,
        }
    }
}
/// Construct and use on the audio control thread. A callback only marks dirty;
/// reads and writes run here, never on a device or input callback.
pub trait Endpoint {
    fn changed(&self) -> bool;
    fn read(&mut self) -> Result<Reading, Fault>;
    fn write(&mut self, reading: Reading) -> Result<(), Fault>;
    fn key_with_modifiers(&mut self, action: KeyAction, _modifiers: u8) -> Result<(), Fault> {
        self.key(action)
    }
    fn key(&mut self, _action: KeyAction) -> Result<(), Fault> {
        Err(Fault {
            status: ExitVolumeStatus::Unsupported,
            native_code: None,
        })
    }
}

enum ApplyError {
    Refused(ControlFailure),
    Failed(ControlFailure),
}
impl From<ControlFailure> for ApplyError {
    fn from(reason: ControlFailure) -> Self {
        Self::Refused(reason)
    }
}

pub struct Owner {
    boot: Boot,
    generation: u64,
    state: Option<ExitVolumeState>,
    endpoint: Option<Box<dyn Endpoint>>,
    presses: Vec<keys::Press>,
    completed: VecDeque<(Boot, SetExitVolume, ExitVolumeResult)>,
}
impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owner")
            .field("state", &self.state)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
impl Owner {
    pub fn new(boot: Boot) -> Self {
        Self {
            boot,
            generation: 0,
            state: None,
            endpoint: None,
            presses: Vec::new(),
            completed: VecDeque::new(),
        }
    }
    pub fn state(&self) -> Option<&ExitVolumeState> {
        self.state.as_ref()
    }
    pub fn retire(&mut self) {
        self.endpoint = None;
        self.state = None;
        self.presses.clear();
    }
    /// Even rebinding the same UID gets a new generation: an unplug/replug or
    /// audio-service restart cannot revive an old volume or media context.
    pub fn bind(
        &mut self,
        claim: ExitClaimStamp,
        output_id: Option<String>,
        endpoint: Result<Box<dyn Endpoint>, Fault>,
    ) -> Result<(), ControlFailure> {
        if claim.owner().and_then(|owner| owner.ordering_key()).ok()
            != self.boot.device_id().and_then(|owner| owner.ordering_key()).ok()
        {
            return Err(ControlFailure::NotExit);
        }
        self.generation = crate::session::next_generation()?;
        self.presses.clear();
        let version = ExitVolumeVersion::from_raw(RawMap::from([
            (1, self.boot.write()),
            (3, output_id.map_or(RawValue::Null, RawValue::Text)),
            (4, self.generation.into()),
            (5, 1_u64.into()),
            (6, claim.write()),
        ]))
        .map_err(|_| ControlFailure::InvalidValue)?;
        let reading = match endpoint {
            Ok(mut endpoint) => {
                let reading = endpoint.read();
                self.endpoint = Some(endpoint);
                reading
            }
            Err(fault) => {
                self.endpoint = None;
                Err(fault)
            }
        };
        self.state = Some(state(version, reading)?);
        Ok(())
    }
    pub fn refresh(&mut self, force: bool) -> Result<bool, ControlFailure> {
        let Some(endpoint) = &mut self.endpoint else {
            return Ok(false);
        };
        if !force && !endpoint.changed() {
            return Ok(false);
        }
        let reading = endpoint.read();
        self.apply_reading(reading)
    }
    fn apply_reading(&mut self, reading: Result<Reading, Fault>) -> Result<bool, ControlFailure> {
        let Some(previous) = &self.state else { return Ok(false) };
        let mut version = previous.version().map_err(|_| ControlFailure::InvalidValue)?;
        let next = state(version.clone(), reading)?;
        if &next == previous {
            return Ok(false);
        }
        let revision = version
            .revision()
            .map_err(|_| ControlFailure::InvalidValue)?
            .checked_add(1)
            .ok_or(ControlFailure::StateConflict)?;
        version
            .edit(|raw| {
                raw.insert(5, revision.into());
            })
            .map_err(|_| ControlFailure::InvalidValue)?;
        self.state = Some(state(version, reading)?);
        Ok(true)
    }
    pub fn set(&mut self, requester: &Boot, request: SetExitVolume) -> ExitVolumeResult {
        let id = request.request_id().expect("validated request");
        if let Some((_, old, result)) = self
            .completed
            .iter()
            .find(|(boot, old, _)| boot == requester && old.request_id() == Ok(id.clone()))
        {
            return if old == &request {
                result.clone()
            } else {
                self.result(&request, ControlOutcome::Refused, ControlFailure::StateConflict)
            };
        }
        let result = match self.apply(&request) {
            Ok(changed) => self.result(
                &request,
                if changed {
                    ControlOutcome::Applied
                } else {
                    ControlOutcome::NoChange
                },
                ControlFailure::None,
            ),
            Err(ApplyError::Refused(reason)) => self.result(&request, ControlOutcome::Refused, reason),
            Err(ApplyError::Failed(reason)) => self.result(&request, ControlOutcome::Failed, reason),
        };
        if self.completed.len() == 256 {
            self.completed.pop_front();
        }
        self.completed.push_back((requester.clone(), request, result.clone()));
        result
    }
    fn apply(&mut self, request: &SetExitVolume) -> Result<bool, ApplyError> {
        let expected = request.expected().map_err(|_| ControlFailure::InvalidValue)?;
        let percent = request.volume_percent().map_err(|_| ControlFailure::InvalidValue)?;
        let muted = request.muted().map_err(|_| ControlFailure::InvalidValue)?;
        if percent > 100 {
            return Err(ControlFailure::InvalidValue.into());
        }
        // Read current system state before CAS. A physical knob/key movement
        // must invalidate an earlier slider revision, even before its callback.
        self.refresh(true)?;
        let current = self.state.as_ref().ok_or(ControlFailure::NotExit)?;
        let version = current.version().map_err(|_| ControlFailure::InvalidValue)?;
        if expected.exit() != version.exit() {
            return Err(ControlFailure::StaleBoot.into());
        }
        if expected.exit_claim() != version.exit_claim() {
            return Err(ControlFailure::RouteChanged.into());
        }
        if expected.output_generation() != version.output_generation() || expected.output_id() != version.output_id() {
            return Err(ControlFailure::OutputChanged.into());
        }
        if expected.revision() != version.revision() {
            return Err(ControlFailure::StaleRevision.into());
        }
        let status = current.status().map_err(|_| ControlFailure::InvalidValue)?;
        if status != ExitVolumeStatus::Ready {
            return Err(Fault {
                status,
                native_code: None,
            }
            .reason()
            .into());
        }
        if current.volume_percent() == Ok(Some(percent)) && current.muted() == Ok(Some(muted)) {
            return Ok(false);
        }
        let endpoint = self.endpoint.as_mut().ok_or(ControlFailure::OutputUnavailable)?;
        let written = endpoint.write(Reading { percent, muted });
        // Also read back a partial failure. No success result may conceal a
        // system write or claim that a rounded endpoint accepted the exact value.
        let reading = endpoint.read();
        self.apply_reading(reading)?;
        written.map_err(|fault| ApplyError::Failed(fault.reason()))?;
        reading.map_err(|fault| ApplyError::Failed(fault.reason()))?;
        Ok(true)
    }
    fn result(&self, request: &SetExitVolume, outcome: ControlOutcome, reason: ControlFailure) -> ExitVolumeResult {
        ExitVolumeResult::from_raw(RawMap::from([
            (1, request.request_id().expect("validated request").write()),
            (2, self.boot.write()),
            (3, outcome.write()),
            (4, reason.write()),
            (5, self.state.as_ref().map_or(RawValue::Null, WireValue::write)),
        ]))
        .expect("validated volume result")
    }
}
fn state(version: ExitVolumeVersion, reading: Result<Reading, Fault>) -> Result<ExitVolumeState, ControlFailure> {
    let mut raw = RawMap::from([(1, version.write())]);
    match reading {
        Ok(reading) => {
            raw.insert(2, u64::from(reading.percent).into());
            raw.insert(3, reading.muted.into());
            raw.insert(4, ExitVolumeStatus::Ready.write());
        }
        Err(fault) => {
            raw.insert(4, fault.status.write());
        }
    }
    ExitVolumeState::from_raw(raw).map_err(|_| ControlFailure::InvalidValue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::Stamp;
    use oink_foundation::DeviceId;
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Fake(Rc<RefCell<(Reading, u32, bool)>>);
    impl Endpoint for Fake {
        fn changed(&self) -> bool {
            self.0.borrow().2
        }
        fn read(&mut self) -> Result<Reading, Fault> {
            let mut s = self.0.borrow_mut();
            s.2 = false;
            Ok(s.0)
        }
        fn key(&mut self, action: KeyAction) -> Result<(), Fault> {
            let mut reading = self.0.borrow().0;
            match action {
                KeyAction::Up => reading.percent = reading.percent.saturating_add(5).min(100),
                KeyAction::Down => reading.percent = reading.percent.saturating_sub(5),
                KeyAction::Mute => reading.muted = !reading.muted,
            }
            self.write(reading)
        }
        fn write(&mut self, mut reading: Reading) -> Result<(), Fault> {
            let mut s = self.0.borrow_mut();
            s.1 += 1;
            reading.percent = reading.percent / 5 * 5;
            s.0 = reading;
            Ok(())
        }
    }
    fn boot(nonce: u8) -> Boot {
        let stamp = Stamp {
            counter: 1,
            owner: DeviceId([1; 32]),
        }
        .to_wire();
        Boot::from_raw(RawMap::from([
            (1, stamp.owner().unwrap().write()),
            (2, RawValue::Bytes(vec![nonce; 16])),
        ]))
        .unwrap()
    }
    fn claim(counter: u64) -> ExitClaimStamp {
        Stamp {
            counter,
            owner: DeviceId([1; 32]),
        }
        .to_wire()
    }
    fn setup() -> (Owner, Fake) {
        let fake = Fake(Rc::new(RefCell::new((
            Reading {
                percent: 50,
                muted: false,
            },
            0,
            false,
        ))));
        let mut owner = Owner::new(boot(1));
        owner
            .bind(claim(1), Some("synthetic-output".into()), Ok(Box::new(fake.clone())))
            .unwrap();
        (owner, fake)
    }
    fn request(owner: &Owner, id: u8, percent: u8) -> SetExitVolume {
        SetExitVolume::from_raw(RawMap::from([
            (1, RawValue::Bytes(vec![id; 16])),
            (2, owner.state().unwrap().version().unwrap().write()),
            (3, u64::from(percent).into()),
            (4, false.into()),
        ]))
        .unwrap()
    }
    #[test]
    fn absolute_write_reports_actual_readback_and_request_retries_do_not_write_twice() {
        let (mut owner, fake) = setup();
        let request = request(&owner, 1, 67);
        let result = owner.set(&boot(9), request.clone());
        assert_eq!(result.outcome().unwrap(), ControlOutcome::Applied);
        assert_eq!(result.current().unwrap().unwrap().volume_percent().unwrap(), Some(65));
        assert_eq!(owner.set(&boot(9), request), result);
        assert_eq!(fake.0.borrow().1, 1);
    }
    #[test]
    fn physical_changes_increment_revision_and_reject_stale_absolute_set() {
        let (mut owner, fake) = setup();
        let request = request(&owner, 1, 20);
        fake.0.borrow_mut().0.percent = 75;
        assert_eq!(
            owner.set(&boot(9), request).reason().unwrap(),
            ControlFailure::StaleRevision
        );
        assert_eq!(fake.0.borrow().1, 0);
        fake.0.borrow_mut().0.muted = true;
        fake.0.borrow_mut().2 = true;
        assert!(owner.refresh(false).unwrap());
        assert_eq!(owner.state().unwrap().muted().unwrap(), Some(true));
    }
    #[test]
    fn output_claim_and_boot_aba_are_fenced_and_retired_exit_cannot_write() {
        for kind in 0..4 {
            let (mut owner, fake) = setup();
            let request = request(&owner, 1, 20);
            let reason = match kind {
                0 => {
                    owner
                        .bind(claim(1), Some("synthetic-output".into()), Ok(Box::new(fake.clone())))
                        .unwrap();
                    ControlFailure::OutputChanged
                }
                1 => {
                    owner
                        .bind(claim(3), Some("synthetic-output".into()), Ok(Box::new(fake.clone())))
                        .unwrap();
                    ControlFailure::RouteChanged
                }
                2 => {
                    owner = Owner::new(boot(2));
                    owner
                        .bind(claim(1), Some("synthetic-output".into()), Ok(Box::new(fake.clone())))
                        .unwrap();
                    ControlFailure::StaleBoot
                }
                _ => {
                    owner.retire();
                    ControlFailure::NotExit
                }
            };
            assert_eq!(owner.set(&boot(9), request).reason().unwrap(), reason);
            assert_eq!(fake.0.borrow().1, 0);
        }
    }
    #[test]
    fn retry_keys_include_authenticated_requester_boot() {
        let (mut owner, fake) = setup();
        let first = request(&owner, 1, 30);
        assert_eq!(owner.set(&boot(8), first).outcome().unwrap(), ControlOutcome::Applied);
        let second = request(&owner, 1, 80);
        assert_eq!(owner.set(&boot(9), second).outcome().unwrap(), ControlOutcome::Applied);
        assert_eq!(fake.0.borrow().1, 2);
    }
    #[test]
    fn partial_native_failure_returns_failed_with_actual_readback() {
        struct Partial(Fake);
        impl Endpoint for Partial {
            fn changed(&self) -> bool {
                false
            }
            fn read(&mut self) -> Result<Reading, Fault> {
                self.0.read()
            }
            fn write(&mut self, reading: Reading) -> Result<(), Fault> {
                self.0.write(reading)?;
                Err(Fault {
                    status: ExitVolumeStatus::PermissionDenied,
                    native_code: Some(5),
                })
            }
        }
        let (mut owner, fake) = setup();
        owner
            .bind(claim(1), Some("synthetic-output".into()), Ok(Box::new(Partial(fake))))
            .unwrap();
        let command = request(&owner, 1, 20);
        let result = owner.set(&boot(9), command);
        assert_eq!(result.outcome().unwrap(), ControlOutcome::Failed);
        assert_eq!(result.reason().unwrap(), ControlFailure::PermissionDenied);
        assert_eq!(result.current().unwrap().unwrap().volume_percent().unwrap(), Some(20));
    }
    #[test]
    fn physical_key_press_is_pinned_deduplicated_and_retired_with_output() {
        use oink_protocol::v1::dto::{AudioSessionContext, ReceiverOutputBinding};
        let (mut owner, fake) = setup();
        let version = owner.state().unwrap().version().unwrap();
        let output = ReceiverOutputBinding::from_raw(
            [
                (1, version.exit().unwrap().write()),
                (2, version.output_generation().unwrap().write()),
                (3, version.output_id().unwrap().unwrap().write()),
            ]
            .into(),
        )
        .unwrap();
        let session = AudioSessionContext::from_raw(
            [
                (1, boot(9).write()),
                (2, 1u64.write()),
                (3, output.write()),
                (4, claim(1).write()),
            ]
            .into(),
        )
        .unwrap();
        let mut event = KeyEvent {
            source: boot(9),
            session,
            press_id: 1,
            action: KeyAction::Up,
            pressed: true,
            repeat: false,
            modifiers: 0,
        };
        assert!(owner.key(event.clone()).unwrap());
        assert!(!owner.key(event.clone()).unwrap());
        event.repeat = true;
        assert!(owner.key(event.clone()).unwrap());
        event.pressed = false;
        assert!(!owner.key(event.clone()).unwrap());
        event.pressed = true;
        assert!(!owner.key(event.clone()).unwrap());
        assert_eq!(fake.0.borrow().1, 2);
        let generation = owner.state().unwrap().version().unwrap().output_generation().unwrap();
        owner.retain_sources(|_| false);
        assert_eq!(owner.key(event.clone()).unwrap_err(), ControlFailure::StateConflict);
        assert!(owner.presses.is_empty());
        event.repeat = false;
        event.press_id = 2;
        assert!(owner.key(event.clone()).unwrap());
        assert_eq!(
            owner.state().unwrap().version().unwrap().output_generation().unwrap(),
            generation
        );
        assert_eq!(fake.0.borrow().1, 3);
        owner
            .bind(claim(1), Some("synthetic-output".into()), Ok(Box::new(fake.clone())))
            .unwrap();
        assert_eq!(owner.key(event).unwrap_err(), ControlFailure::OutputChanged);
        assert_eq!(fake.0.borrow().1, 3);
    }
    #[test]
    fn native_key_keeps_modifiers_and_publishes_deferred_system_readback() {
        struct Deferred(Fake);
        impl Endpoint for Deferred {
            fn changed(&self) -> bool {
                self.0.changed()
            }
            fn read(&mut self) -> Result<Reading, Fault> {
                self.0.read()
            }
            fn write(&mut self, reading: Reading) -> Result<(), Fault> {
                self.0.write(reading)
            }
            fn key_with_modifiers(&mut self, action: KeyAction, modifiers: u8) -> Result<(), Fault> {
                assert_eq!(action, KeyAction::Up);
                assert_eq!(modifiers, 0x66);
                // Native event delivery is asynchronous, without optimistic writes.
                Ok(())
            }
        }
        let (mut owner, fake) = setup();
        owner
            .bind(
                claim(1),
                Some("synthetic-output".into()),
                Ok(Box::new(Deferred(fake.clone()))),
            )
            .unwrap();
        let version = owner.state().unwrap().version().unwrap();
        let session = crate::session::volume_context(&boot(9), owner.state().unwrap()).unwrap();
        assert!(
            !owner
                .key(KeyEvent {
                    source: boot(9),
                    session,
                    press_id: 1,
                    action: KeyAction::Up,
                    pressed: true,
                    repeat: false,
                    modifiers: 0x66,
                })
                .unwrap()
        );
        assert_eq!(owner.state().unwrap().volume_percent().unwrap(), Some(50));
        assert_eq!(fake.0.borrow().1, 0);
        fake.0.borrow_mut().0.percent = 52;
        fake.0.borrow_mut().2 = true;
        assert!(owner.refresh(false).unwrap());
        assert_eq!(owner.state().unwrap().volume_percent().unwrap(), Some(52));
        assert_eq!(
            owner.state().unwrap().version().unwrap().revision().unwrap(),
            version.revision().unwrap() + 1
        );
    }
    #[test]
    fn unavailable_volume_is_unknown_and_pause_is_not_an_authorization_gate() {
        let (mut owner, _) = setup();
        owner.bind(claim(1), None, Err(Fault::unavailable())).unwrap();
        assert_eq!(owner.state().unwrap().volume_percent().unwrap(), None);
        let request = request(&owner, 1, 20);
        assert_eq!(
            owner.set(&boot(9), request).reason().unwrap(),
            ControlFailure::OutputUnavailable
        );
        // The volume owner deliberately has no sharing-pause flag: control of a
        // live output remains legal while input/media participation is paused.
    }
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
/// Opens the exact selected output. No fallback to another/default device.
pub fn open(output: &str, wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> Result<Box<dyn Endpoint>, Fault> {
    #[cfg(target_os = "macos")]
    {
        macos::open(output, wake)
    }
    #[cfg(windows)]
    {
        windows::open(output, wake)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (output, wake);
        Err(Fault {
            status: ExitVolumeStatus::Unsupported,
            native_code: None,
        })
    }
}
