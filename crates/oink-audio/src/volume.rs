//! Live output volume belongs to the selected exit, not to persisted source gain.
//! This owner fences absolute writes with Boot, route version, output generation and
//! revision, and reports the actual readback rather than an optimistic slider.
use oink_protocol::v2::{
    RawMap, RawValue,
    dto::{
        AudioRouteVersion, Boot, ControlFailure, ControlOutcome, ExitVolumeResult, ExitVolumeState, ExitVolumeStatus,
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

pub type Owner = VolumeOwner<dyn Endpoint>;
pub type SendOwner = VolumeOwner<dyn Endpoint + Send>;

pub struct VolumeOwner<E: Endpoint + ?Sized> {
    boot: Boot,
    generation: u64,
    state: Option<ExitVolumeState>,
    endpoint: Option<Box<E>>,
    presses: Vec<keys::Press>,
    completed: VecDeque<(Boot, SetExitVolume, ExitVolumeResult)>,
}
impl<E: Endpoint + ?Sized> std::fmt::Debug for VolumeOwner<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owner")
            .field("state", &self.state)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
impl<E: Endpoint + ?Sized> VolumeOwner<E> {
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
        route: AudioRouteVersion,
        output_id: Option<String>,
        endpoint: Result<Box<E>, Fault>,
    ) -> Result<(), ControlFailure> {
        self.bind_at_generation(route, output_id, crate::session::next_generation()?, endpoint)
    }

    /// Bind the same already allocated native generation as the media route.
    /// The caller binds only while this device is the resolved exit of `route`.
    pub fn bind_at_generation(
        &mut self,
        route: AudioRouteVersion,
        output_id: Option<String>,
        generation: u64,
        endpoint: Result<Box<E>, Fault>,
    ) -> Result<(), ControlFailure> {
        if generation == 0 {
            return Err(ControlFailure::InvalidValue);
        }
        let version = ExitVolumeVersion::from_raw(RawMap::from([
            (1, self.boot.write()),
            (3, output_id.map_or(RawValue::Null, RawValue::Text)),
            (4, generation.into()),
            (5, 1_u64.into()),
            (6, route.write()),
        ]))
        .map_err(|_| ControlFailure::InvalidValue)?;
        let (endpoint, reading) = read_endpoint(endpoint);
        let state = state(version, reading)?;
        self.endpoint = endpoint;
        self.generation = generation;
        self.presses.clear();
        self.state = Some(state);
        Ok(())
    }

    /// The route is unchanged when a platform bridge registers or detaches.
    /// Keep its generation and advance the revision if the readback changes.
    pub fn replace_endpoint(&mut self, endpoint: Result<Box<E>, Fault>) -> Result<(), ControlFailure> {
        let (endpoint, reading) = read_endpoint(endpoint);
        self.apply_reading(reading)?;
        self.endpoint = endpoint;
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
        if expected.route_version() != version.route_version() {
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
fn read_endpoint<E: Endpoint + ?Sized>(endpoint: Result<Box<E>, Fault>) -> (Option<Box<E>>, Result<Reading, Fault>) {
    match endpoint {
        Ok(mut endpoint) => {
            let reading = endpoint.read();
            (Some(endpoint), reading)
        }
        Err(fault) => (None, Err(fault)),
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
    use crate::route::Version;
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
        let stamp = route_version(1);
        Boot::from_raw(RawMap::from([
            (1, stamp.author().unwrap().write()),
            (2, RawValue::Bytes(vec![nonce; 16])),
        ]))
        .unwrap()
    }
    fn route_version(stamp: u64) -> AudioRouteVersion {
        Version {
            stamp,
            author: DeviceId([1; 32]),
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
            .bind(
                route_version(1),
                Some("synthetic-output".into()),
                Ok(Box::new(fake.clone())),
            )
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
    fn unsupported_endpoint_can_refuse_or_fail_after_a_successful_native_write() {
        let unsupported = Fault {
            status: ExitVolumeStatus::Unsupported,
            native_code: None,
        };
        let (mut owner, _) = setup();
        owner.bind(route_version(1), None, Err(unsupported)).unwrap();
        let command = request(&owner, 1, 20);
        let result = owner.set(&boot(9), command);
        assert_eq!(result.outcome().unwrap(), ControlOutcome::Refused);
        assert_eq!(result.reason().unwrap(), ControlFailure::VolumeUnsupported);

        struct Partial {
            fake: Fake,
            fail_readback: bool,
            written: bool,
        }
        impl Endpoint for Partial {
            fn changed(&self) -> bool {
                false
            }
            fn read(&mut self) -> Result<Reading, Fault> {
                if self.written && self.fail_readback {
                    Err(Fault {
                        status: ExitVolumeStatus::Unsupported,
                        native_code: None,
                    })
                } else {
                    self.fake.read()
                }
            }
            fn write(&mut self, reading: Reading) -> Result<(), Fault> {
                self.fake.write(reading)?;
                self.written = true;
                if self.fail_readback {
                    Ok(())
                } else {
                    Err(Fault {
                        status: ExitVolumeStatus::Unsupported,
                        native_code: None,
                    })
                }
            }
        }
        // A native setter may partially succeed and return Unsupported, or its
        // readback may fail after a successful write. Neither implies no effect.
        for fail_readback in [false, true] {
            let (mut owner, fake) = setup();
            owner
                .bind(
                    route_version(1),
                    Some("synthetic-output".into()),
                    Ok(Box::new(Partial {
                        fake: fake.clone(),
                        fail_readback,
                        written: false,
                    })),
                )
                .unwrap();
            let command = request(&owner, 1, 20);
            let result = owner.set(&boot(9), command);
            assert_eq!(result.outcome().unwrap(), ControlOutcome::Failed);
            assert_eq!(result.reason().unwrap(), ControlFailure::VolumeUnsupported);
            assert_eq!(fake.0.borrow().0.percent, 20);
            assert_eq!(fake.0.borrow().1, 1);
        }
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
    fn native_binding_generation_survives_platform_endpoint_replacement() {
        let (mut owner, original) = setup();
        owner
            .bind_at_generation(
                route_version(1),
                Some("native".into()),
                321,
                Ok(Box::new(original.clone())),
            )
            .unwrap();
        let stale = request(&owner, 7, 80);
        let replacement = Fake(Rc::new(RefCell::new((
            Reading {
                percent: 55,
                muted: false,
            },
            0,
            false,
        ))));
        owner.replace_endpoint(Ok(Box::new(replacement.clone()))).unwrap();
        let version = owner.state().unwrap().version().unwrap();
        assert_eq!(version.output_generation(), Ok(321));
        assert_eq!(version.revision(), Ok(2));
        assert_eq!(owner.set(&boot(9), stale).reason(), Ok(ControlFailure::StaleRevision));
        assert_eq!(replacement.0.borrow().1, 0);
    }

    #[test]
    fn output_route_and_boot_aba_are_fenced_and_retired_exit_cannot_write() {
        for kind in 0..4 {
            let (mut owner, fake) = setup();
            let request = request(&owner, 1, 20);
            let reason = match kind {
                0 => {
                    owner
                        .bind(
                            route_version(1),
                            Some("synthetic-output".into()),
                            Ok(Box::new(fake.clone())),
                        )
                        .unwrap();
                    ControlFailure::OutputChanged
                }
                1 => {
                    owner
                        .bind(
                            route_version(3),
                            Some("synthetic-output".into()),
                            Ok(Box::new(fake.clone())),
                        )
                        .unwrap();
                    ControlFailure::RouteChanged
                }
                2 => {
                    owner = Owner::new(boot(2));
                    owner
                        .bind(
                            route_version(1),
                            Some("synthetic-output".into()),
                            Ok(Box::new(fake.clone())),
                        )
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
            .bind(
                route_version(1),
                Some("synthetic-output".into()),
                Ok(Box::new(Partial(fake))),
            )
            .unwrap();
        let command = request(&owner, 1, 20);
        let result = owner.set(&boot(9), command);
        assert_eq!(result.outcome().unwrap(), ControlOutcome::Failed);
        assert_eq!(result.reason().unwrap(), ControlFailure::PermissionDenied);
        assert_eq!(result.current().unwrap().unwrap().volume_percent().unwrap(), Some(20));
    }
    #[test]
    fn physical_key_press_is_pinned_deduplicated_and_retired_with_output() {
        use oink_protocol::v2::dto::{AudioSessionContext, ReceiverOutputBinding};
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
                (4, route_version(1).write()),
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
            .bind(
                route_version(1),
                Some("synthetic-output".into()),
                Ok(Box::new(fake.clone())),
            )
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
                route_version(1),
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
        owner.bind(route_version(1), None, Err(Fault::unavailable())).unwrap();
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
