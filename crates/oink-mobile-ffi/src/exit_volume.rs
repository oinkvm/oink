//! Output volume for the currently resolved mobile speaker exit: the system
//! media volume on Android, Oink's own playout gain on iOS
//! (`crate::playout_volume`). Also the phone's volume-key context for the
//! group's exit, which the embedded router reads at each press.
use crate::{
    MobileMediaSession,
    playout_volume::{PlayoutEndpoint, PlayoutVolume},
};
use oink_audio::volume::{Endpoint, Fault, KeyAction, KeyEvent, Reading, SendOwner};
use oink_foundation::session::Boot;
use oink_mobile_core::ExitVolumeKey;
use oink_network::wire_identity::boot;
use oink_protocol::v2::{
    dto::{self, ControlFailure, ExitVolumeStatus},
    schema::{WireDto, WireValue},
};
use std::sync::Arc;

/// A system volume key, as a desktop exit posts the native media key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MobileVolumeKey {
    Up,
    Down,
    Mute,
}

impl From<KeyAction> for MobileVolumeKey {
    fn from(action: KeyAction) -> Self {
        match action {
            KeyAction::Up => Self::Up,
            KeyAction::Down => Self::Down,
            KeyAction::Mute => Self::Mute,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MobileExitVolumeStatus {
    Ready,
    Unsupported,
    Unavailable,
    PermissionDenied,
    BackendFailed,
}

impl MobileExitVolumeStatus {
    fn fault(self) -> Fault {
        Fault {
            status: match self {
                Self::Ready | Self::BackendFailed => ExitVolumeStatus::BackendFailed,
                Self::Unsupported => ExitVolumeStatus::Unsupported,
                Self::Unavailable => ExitVolumeStatus::Unavailable,
                Self::PermissionDenied => ExitVolumeStatus::PermissionDenied,
            },
            native_code: None,
        }
    }
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct MobileExitVolumeReading {
    pub status: MobileExitVolumeStatus,
    pub volume_percent: Option<u8>,
    pub muted: Option<bool>,
}

/// Ordinary-thread platform operations. The host must reject obsolete native
/// generations and return actual system readback. These operations must not
/// call back into Rust or wait on the audio callback thread.
#[uniffi::export(callback_interface)]
pub trait MobileExitVolumeEndpoint: Send + Sync {
    fn read_volume(&self, generation: u64) -> MobileExitVolumeReading;
    fn write_volume(&self, generation: u64, volume_percent: u8, muted: bool) -> MobileExitVolumeStatus;
    /// One forwarded volume key: the system's own step and mute toggle, the
    /// way a desktop exit posts the native key. Readback follows with `read_volume`.
    fn key_volume(&self, generation: u64, key: MobileVolumeKey) -> MobileExitVolumeStatus;
}

struct PlatformEndpoint {
    host: Arc<dyn MobileExitVolumeEndpoint>,
    generation: u64,
}

impl Endpoint for PlatformEndpoint {
    fn changed(&self) -> bool {
        true
    }

    fn read(&mut self) -> Result<Reading, Fault> {
        let reading = self.host.read_volume(self.generation);
        match (reading.status, reading.volume_percent, reading.muted) {
            (MobileExitVolumeStatus::Ready, Some(percent @ 0..=100), Some(muted)) => Ok(Reading { percent, muted }),
            (status, _, _) => Err(status.fault()),
        }
    }

    fn write(&mut self, reading: Reading) -> Result<(), Fault> {
        let status = self.host.write_volume(self.generation, reading.percent, reading.muted);
        if status == MobileExitVolumeStatus::Ready {
            Ok(())
        } else {
            Err(status.fault())
        }
    }

    fn key(&mut self, action: KeyAction) -> Result<(), Fault> {
        let status = self.host.key_volume(self.generation, action.into());
        if status == MobileExitVolumeStatus::Ready {
            Ok(())
        } else {
            Err(status.fault())
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Route {
    version: dto::AudioRouteVersion,
    output: dto::ReceiverOutputBinding,
}

pub(crate) struct MobileExitVolume {
    local: Boot,
    owner: SendOwner,
    route: Option<Route>,
    endpoint: Option<Arc<dyn MobileExitVolumeEndpoint>>,
    /// iOS: the exit volume is the playout gain, without a host endpoint.
    playout: Option<Arc<PlayoutVolume>>,
    dirty: bool,
}

impl MobileExitVolume {
    pub(crate) fn new(local: Boot, playout: Option<Arc<PlayoutVolume>>) -> Self {
        Self {
            local,
            owner: SendOwner::new(boot(local)),
            route: None,
            endpoint: None,
            playout,
            dirty: false,
        }
    }

    /// The playout gain position (0..=200) the speaker output applies, `None`
    /// where the exit volume is the system's (Android).
    pub(crate) fn playout_gain(&self) -> Option<u16> {
        self.playout.as_ref().map(|playout| playout.gain_percent())
    }

    /// Execute one volume key as the exit. The router admitted the source's
    /// current Boot; the owner checks the pinned exit, route and output, as a
    /// desktop exit's audio owner does, and publishes the readback.
    pub(crate) fn key(&mut self, key: ExitVolumeKey) -> Result<bool, ControlFailure> {
        let action = KeyAction::from_code(key.code).ok_or(ControlFailure::InvalidValue)?;
        self.owner.key(KeyEvent {
            source: boot(key.source),
            session: (*key.route.session).clone(),
            press_id: key.press_id,
            action,
            pressed: key.pressed,
            repeat: key.repeat,
            modifiers: key.modifiers,
        })
    }

    pub(crate) fn set_route(
        &mut self,
        route: Option<(&dto::AudioRouteVersion, &dto::ReceiverOutputBinding)>,
    ) -> Result<(), ControlFailure> {
        let Some((version, output)) = route else {
            if self.route.take().is_some() {
                self.owner.retire();
            }
            return Ok(());
        };
        if self
            .route
            .as_ref()
            .is_some_and(|old| old.version == *version && old.output == *output)
        {
            return Ok(());
        }
        self.owner.retire();
        self.route = None;
        let generation = output.generation().map_err(|_| ControlFailure::InvalidValue)?;
        self.owner.bind_at_generation(
            version.clone(),
            output
                .output_id()
                .map_err(|_| ControlFailure::InvalidValue)?
                .map(|value| value.0),
            generation,
            platform(self.endpoint.as_ref(), self.playout.as_ref(), generation),
        )?;
        self.route = Some(Route {
            version: version.clone(),
            output: output.clone(),
        });
        self.dirty = false;
        Ok(())
    }

    pub(crate) fn refresh(&mut self) -> Result<(), ControlFailure> {
        let Some(route) = &self.route else {
            return Ok(());
        };
        if self.dirty {
            let generation = route.output.generation().map_err(|_| ControlFailure::InvalidValue)?;
            self.owner
                .replace_endpoint(platform(self.endpoint.as_ref(), self.playout.as_ref(), generation))?;
            self.dirty = false;
        } else {
            self.owner.refresh(true)?;
        }
        Ok(())
    }

    pub(crate) fn state(&self) -> Option<&dto::ExitVolumeState> {
        self.owner.state()
    }

    pub(crate) fn handle(&mut self, peer: Boot, request: dto::SetExitVolume) -> dto::ExitVolumeResult {
        self.owner.set(&boot(peer), request)
    }

    /// This phone's own volume-key context while it is the exit.
    pub(crate) fn own_key_route(&self) -> Option<oink_routing::input::VolumeRoute> {
        key_route(self.local, self.owner.state()?)
    }
}

/// A phone has no forwarding session with the exit, so its volume-key context
/// pins what the exit publishes: the exit's Boot, route version and output
/// binding from its Ready exit volume state, with this phone as the source and
/// the output generation as the source route generation (any nonzero value;
/// a new output or route makes a new context). The exit's owner accepts it
/// exactly as a forwarding source's session (`oink_audio::volume::keys`).
pub(crate) fn key_route(local: Boot, state: &dto::ExitVolumeState) -> Option<oink_routing::input::VolumeRoute> {
    if state.status().ok()? != ExitVolumeStatus::Ready {
        return None;
    }
    let version = state.version().ok()?;
    let generation = version.output_generation().ok()?;
    let mut output = oink_protocol::v2::RawMap::from([(1, version.exit().ok()?.write()), (2, generation.write())]);
    if let Some(id) = version.output_id().ok()? {
        output.insert(3, id.write());
    }
    let output = dto::ReceiverOutputBinding::from_raw(output).ok()?;
    let session = dto::AudioSessionContext::from_raw(
        [
            (1, boot(local).write()),
            (2, generation.write()),
            (3, output.write()),
            (4, version.route_version().ok()?.write()),
        ]
        .into(),
    )
    .ok()?;
    oink_routing::input::VolumeRoute::from_session(Arc::new(session))
}

fn platform(
    host: Option<&Arc<dyn MobileExitVolumeEndpoint>>,
    playout: Option<&Arc<PlayoutVolume>>,
    generation: u64,
) -> Result<Box<dyn Endpoint + Send>, Fault> {
    if let Some(host) = host {
        return Ok(Box::new(PlatformEndpoint {
            host: host.clone(),
            generation,
        }));
    }
    match playout {
        Some(playout) => Ok(Box::new(PlayoutEndpoint(playout.clone()))),
        None => Err(MobileExitVolumeStatus::Unsupported.fault()),
    }
}

#[uniffi::export]
impl MobileMediaSession {
    pub fn set_exit_volume_endpoint(&self, endpoint: Box<dyn MobileExitVolumeEndpoint>) {
        let mut volume = self.volume.lock().expect("exit volume mutex");
        volume.endpoint = Some(Arc::from(endpoint));
        volume.dirty = true;
        self.wake_media();
    }

    pub fn clear_exit_volume_endpoint(&self) {
        let mut volume = self.volume.lock().expect("exit volume mutex");
        volume.endpoint = None;
        volume.dirty = true;
        self.wake_media();
    }

    pub fn refresh_exit_volume(&self) {
        self.wake_media();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;
    use oink_protocol::v2::RawValue;
    use std::sync::Mutex;

    const PHONE: Boot = Boot {
        device_id: DeviceId([9; 32]),
        nonce: [9; 16],
    };
    const PC: Boot = Boot {
        device_id: DeviceId([1; 32]),
        nonce: [1; 16],
    };

    fn route(exit: &MobileExitVolume) -> (dto::AudioRouteVersion, dto::ReceiverOutputBinding) {
        let version = dto::AudioRouteVersion::from_raw(
            [(1, 7u64.write()), (2, boot(exit.local).device_id().unwrap().write())].into(),
        )
        .unwrap();
        let output = dto::ReceiverOutputBinding::from_raw(
            [
                (1, boot(exit.local).write()),
                (2, 4u64.write()),
                (3, RawValue::from("mobile:default")),
            ]
            .into(),
        )
        .unwrap();
        (version, output)
    }

    fn bound(playout: Option<Arc<PlayoutVolume>>, host: Option<Arc<dyn MobileExitVolumeEndpoint>>) -> MobileExitVolume {
        let mut exit = MobileExitVolume::new(PHONE, playout);
        exit.endpoint = host;
        let (version, output) = route(&exit);
        exit.set_route(Some((&version, &output))).unwrap();
        exit
    }

    fn reading(exit: &MobileExitVolume) -> (ExitVolumeStatus, Option<u8>, Option<bool>) {
        let state = exit.state().unwrap();
        (
            state.status().unwrap(),
            state.volume_percent().unwrap(),
            state.muted().unwrap(),
        )
    }

    /// One press and release of `code` from `source` under its context.
    fn press(exit: &mut MobileExitVolume, source: Boot, route: &oink_routing::input::VolumeRoute, id: u64, code: u32) {
        for pressed in [true, false] {
            exit.key(ExitVolumeKey {
                source,
                route: route.clone(),
                press_id: id,
                code: oink_foundation::input::KeyCode(code),
                pressed,
                repeat: false,
                modifiers: 0,
            })
            .unwrap();
        }
    }

    const UP: u32 = (0x0c << 16) | 0xe9;
    const MUTE: u32 = (0x0c << 16) | 0xe2;

    #[test]
    fn an_iphone_exit_publishes_and_steps_its_playout_gain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("playout-volume.json");
        let mut exit = bound(Some(PlayoutVolume::open(path.clone())), None);
        assert_eq!(reading(&exit), (ExitVolumeStatus::Ready, Some(50), Some(false)));
        assert_eq!(exit.playout_gain(), Some(100), "the default is unity");

        // A focus PC forwarding here, with the context its router builds from
        // this phone's published state.
        let from_pc = key_route(PC, exit.state().unwrap()).unwrap();
        press(&mut exit, PC, &from_pc, 1, UP);
        assert_eq!(reading(&exit), (ExitVolumeStatus::Ready, Some(55), Some(false)));
        // The phone's own control page while it is the exit.
        let own = exit.own_key_route().unwrap();
        assert_eq!(own.source, PHONE);
        press(&mut exit, PHONE, &own, 1, MUTE);
        assert_eq!(reading(&exit), (ExitVolumeStatus::Ready, Some(55), Some(true)));
        assert_eq!(exit.playout_gain(), Some(0));
        assert_eq!(
            PlayoutVolume::open(path).reading(),
            Reading {
                percent: 55,
                muted: true
            },
            "persisted on the phone"
        );
    }

    #[test]
    fn a_key_for_another_route_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut exit = bound(Some(PlayoutVolume::open(dir.path().join("v.json"))), None);
        // The context of an earlier output generation of this exit.
        let mut state = exit.state().unwrap().clone();
        let mut version = state.version().unwrap();
        version
            .edit(|raw| {
                raw.insert(4, 99u64.write());
            })
            .unwrap();
        state
            .edit(|raw| {
                raw.insert(1, version.write());
            })
            .unwrap();
        let stale = key_route(PC, &state).unwrap();
        assert_eq!(
            exit.key(ExitVolumeKey {
                source: PC,
                route: stale,
                press_id: 1,
                code: oink_foundation::input::KeyCode(UP),
                pressed: true,
                repeat: false,
                modifiers: 0,
            }),
            Err(ControlFailure::OutputChanged)
        );
        assert_eq!(reading(&exit).1, Some(50));
    }

    #[test]
    fn without_a_platform_volume_the_exit_is_unsupported() {
        let exit = bound(None, None);
        assert_eq!(reading(&exit).0, ExitVolumeStatus::Unsupported);
        assert_eq!(exit.playout_gain(), None);
        assert!(exit.own_key_route().is_none(), "no key context for an unsupported exit");
    }

    /// Android's host bridge: the system's own step, read back after the key.
    struct SystemVolume(Mutex<(u8, bool, Vec<MobileVolumeKey>)>);
    impl MobileExitVolumeEndpoint for SystemVolume {
        fn read_volume(&self, _: u64) -> MobileExitVolumeReading {
            let s = self.0.lock().unwrap();
            MobileExitVolumeReading {
                status: MobileExitVolumeStatus::Ready,
                volume_percent: Some(s.0),
                muted: Some(s.1),
            }
        }
        fn write_volume(&self, _: u64, volume_percent: u8, muted: bool) -> MobileExitVolumeStatus {
            let mut s = self.0.lock().unwrap();
            s.0 = volume_percent;
            s.1 = muted;
            MobileExitVolumeStatus::Ready
        }
        fn key_volume(&self, _: u64, key: MobileVolumeKey) -> MobileExitVolumeStatus {
            let mut s = self.0.lock().unwrap();
            match key {
                MobileVolumeKey::Up => {
                    s.0 = (s.0 + 7).min(100);
                    s.1 = false;
                }
                MobileVolumeKey::Down => {
                    s.0 = s.0.saturating_sub(7);
                    s.1 = false;
                }
                MobileVolumeKey::Mute => s.1 = !s.1,
            }
            s.2.push(key);
            MobileExitVolumeStatus::Ready
        }
    }

    #[test]
    fn an_android_exit_hands_forwarded_keys_to_the_system_volume() {
        let host = Arc::new(SystemVolume(Mutex::new((40, false, Vec::new()))));
        let mut exit = bound(None, Some(host.clone()));
        assert_eq!(
            exit.playout_gain(),
            None,
            "Android plays at unity; the system volume applies"
        );
        let from_pc = key_route(PC, exit.state().unwrap()).unwrap();
        press(&mut exit, PC, &from_pc, 1, UP);
        press(&mut exit, PC, &from_pc, 2, MUTE);
        assert_eq!(host.0.lock().unwrap().2, [MobileVolumeKey::Up, MobileVolumeKey::Mute]);
        assert_eq!(reading(&exit), (ExitVolumeStatus::Ready, Some(47), Some(true)));
    }
}
