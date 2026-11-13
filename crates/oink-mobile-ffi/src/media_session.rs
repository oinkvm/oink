//! Device-local audio intent and native callback admission.
#[path = "media_activation.rs"]
mod activation;
pub use activation::{AndroidActivationProgress, AndroidActivationRequest, AndroidControlListener, AndroidControls};

use crate::{
    MobileError,
    audio::NativeAudio,
    audio_config::{PhoneAudio, peer_config, wall_ms},
    exit_volume::MobileExitVolume,
    microphone_controls::{MicrophoneControls, publish_result},
    microphone_transport::MicrophoneTransport,
    speaker_transport::SpeakerTransport,
};
use oink_audio::mobile::{CloseReason, Controller, Direction, EndpointSnapshot, IdleTimeout};
use oink_foundation::{DeviceId, GroupId, session::Boot, storage};
use oink_mobile_core::MobileNetwork;
use oink_network::{
    link::StreamId,
    wire_identity::{boot, read_boot},
};
use oink_protocol::v2::{
    self, Message, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{Notify, watch},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MobileMediaKind {
    Speaker,
    Microphone,
    Forward,
}

const KINDS: [MobileMediaKind; 3] = [
    MobileMediaKind::Speaker,
    MobileMediaKind::Microphone,
    MobileMediaKind::Forward,
];

type Failure = dto::EndpointRefusal;
/// How this phone answers an endpoint Request/Release (M22).
pub(crate) type EndpointOutcome = Result<dto::ControlOutcome, Failure>;
const AUDIO_MASK: u8 = 7;
const DUPLEX_MASK: u8 = 3;

impl MobileMediaKind {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Speaker => 0,
            Self::Microphone => 1,
            Self::Forward => 2,
        }
    }

    pub(crate) fn bit(self) -> u8 {
        1 << self.index()
    }

    fn direction(self) -> Direction {
        match self {
            Self::Speaker => Direction::SpeakerReceive,
            Self::Microphone => Direction::MicrophoneSend,
            Self::Forward => Direction::SpeakerSend,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Enum)]
pub enum MobileMediaFailure {
    #[default]
    None,
    PermissionRequired,
    Interrupted,
    SystemBlocked,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Enum)]
pub enum MobileMicrophonePermission {
    #[default]
    NotDetermined,
    Granted,
    Denied,
    Restricted,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum MobileAudioIdleTimeout {
    OneMinute,
    FiveMinutes,
    TenMinutes,
    TwentyMinutes,
    ThirtyMinutes,
    SixtyMinutes,
    Unlimited,
}

impl From<MobileAudioIdleTimeout> for IdleTimeout {
    fn from(value: MobileAudioIdleTimeout) -> Self {
        match value {
            MobileAudioIdleTimeout::OneMinute => Self::OneMinute,
            MobileAudioIdleTimeout::FiveMinutes => Self::FiveMinutes,
            MobileAudioIdleTimeout::TenMinutes => Self::TenMinutes,
            MobileAudioIdleTimeout::TwentyMinutes => Self::TwentyMinutes,
            MobileAudioIdleTimeout::ThirtyMinutes => Self::ThirtyMinutes,
            MobileAudioIdleTimeout::SixtyMinutes => Self::SixtyMinutes,
            MobileAudioIdleTimeout::Unlimited => Self::Unlimited,
        }
    }
}

impl From<IdleTimeout> for MobileAudioIdleTimeout {
    fn from(value: IdleTimeout) -> Self {
        match value {
            IdleTimeout::OneMinute => Self::OneMinute,
            IdleTimeout::FiveMinutes => Self::FiveMinutes,
            IdleTimeout::TenMinutes => Self::TenMinutes,
            IdleTimeout::TwentyMinutes => Self::TwentyMinutes,
            IdleTimeout::ThirtyMinutes => Self::ThirtyMinutes,
            IdleTimeout::SixtyMinutes => Self::SixtyMinutes,
            IdleTimeout::Unlimited => Self::Unlimited,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MobileRemoteMediaOwner {
    pub caller: String,
    pub boot_nonce: Vec<u8>,
    pub request: Vec<u8>,
    pub resources: u8,
}

impl MobileRemoteMediaOwner {
    fn same_request(&self, other: &Self) -> bool {
        self.caller == other.caller && self.boot_nonce == other.boot_nonce && self.request == other.request
    }

    fn matches_boot(&self, peer: Boot) -> bool {
        self.boot_nonce == peer.nonce && self.caller == peer.device_id.to_hex()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MobileMediaState {
    pub generation: u64,
    pub enabled_mask: u8,
    pub local_enabled_mask: u8,
    pub remote_owners: Vec<MobileRemoteMediaOwner>,
    pub pause_mask: u8,
    pub start_mask: u8,
    pub stop_mask: u8,
    pub running_mask: u8,
    pub failures: Vec<MobileMediaFailure>,
    pub microphone_permission: MobileMicrophonePermission,
    /// Native stop acknowledges the reserved generation even after a rebuild.
    pub owner_generations: Vec<u64>,
    /// False from the core start (or a group switch) until this phone merged
    /// with the group's audio configuration. An enabled speaker / microphone
    /// then shows "waiting for connection", not exit / sending (protocol-v2 6.1).
    pub audio_joined: bool,
    pub forward_target: Option<String>,
}

type VolumePresses = HashMap<u16, (oink_routing::input::VolumeRoute, u64)>;

/// Retire using the press-time output context, even after the new exit is known.
fn retire_volume_presses(
    presses: &mut VolumePresses,
    current: Option<&oink_routing::input::VolumeRoute>,
    forwarding: bool,
) -> Vec<(u16, oink_routing::input::VolumeRoute, u64)> {
    let mut retired = Vec::new();
    presses.retain(|usage, (route, id)| {
        let keep = forwarding
            && current.is_some_and(|current| {
                current.target == route.target
                    && current.session.route_version() == route.session.route_version()
                    && current.session.receiver_output() == route.session.receiver_output()
            });
        if !keep {
            retired.push((*usage, route.clone(), *id));
        }
        keep
    });
    retired
}

fn next_native_generation() -> u64 {
    // C handles share the AEC tap within an engine epoch. Distinct sessions
    // and rebuilds use disjoint epochs even when they run in the same process.
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

struct State {
    generation: u64,
    local_enabled: u8,
    remote: Vec<MobileRemoteMediaOwner>,
    activation: activation::Activation,
    permission: MobileMicrophonePermission,
    failures: [MobileMediaFailure; 3],
    owners: [u64; 3],
    native: [Option<NativeAudio>; 3],
    running: u8,
    stopping: u8,
    closed: bool,
    echo_cancellation: bool,
    speaker_exit: bool,
    forward_ready: bool,
    forward_target: Option<DeviceId>,
    /// A receiver takes this phone's microphone (it has routes): the
    /// captured audio is sent.
    microphone_ready: bool,
    /// iOS (user 2026-11-12): the microphone captures as soon as it is on,
    /// also while no receiver takes it, and only the sending follows the
    /// demand. Android captures only on demand.
    capture_while_enabled: bool,
    /// Merged with the group's audio configuration since this start.
    audio_joined: bool,
    idle: Controller,
    revision: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            generation: next_native_generation(),
            local_enabled: 0,
            remote: vec![],
            activation: activation::Activation::default(),
            permission: MobileMicrophonePermission::NotDetermined,
            failures: [MobileMediaFailure::None; 3],
            owners: [0; 3],
            native: [None, None, None],
            running: 0,
            stopping: 0,
            closed: false,
            echo_cancellation: true,
            speaker_exit: false,
            forward_ready: false,
            forward_target: None,
            microphone_ready: false,
            capture_while_enabled: false,
            audio_joined: false,
            idle: Controller::default(),
            revision: 1,
        }
    }
}

impl State {
    fn enabled(&self) -> u8 {
        self.remote
            .iter()
            .fold(self.local_enabled, |mask, owner| mask | owner.resources)
    }

    /// The native microphone may run: a receiver takes it, or (iOS) it is
    /// simply on. Without a receiver the captured audio is not sent: the
    /// native input gets no routes ([`MobileMediaSession::service_audio`]).
    fn microphone_capture(&self) -> bool {
        self.microphone_ready
            || (self.capture_while_enabled
                && self.permission == MobileMicrophonePermission::Granted
                && self.enabled() & MobileMediaKind::Microphone.bit() != 0)
    }

    fn owned(&self) -> u8 {
        KINDS
            .iter()
            .filter(|kind| self.owners[kind.index()] != 0)
            .fold(0, |mask, kind| mask | kind.bit())
    }

    fn snapshot(&self) -> MobileMediaState {
        let blocked = KINDS
            .iter()
            .filter(|kind| self.failures[kind.index()] != MobileMediaFailure::None)
            .fold(0, |mask, kind| mask | kind.bit());
        let enabled = self.enabled();
        let eligible = u8::from(self.speaker_exit)
            | (u8::from(self.microphone_capture()) << 1)
            | (u8::from(self.forward_ready || self.speaker_exit) << 2);
        let start_mask = if self.closed || self.stopping != 0 {
            0
        } else {
            enabled & eligible & !self.owned() & !blocked
        };
        MobileMediaState {
            generation: self.generation,
            enabled_mask: enabled,
            local_enabled_mask: self.local_enabled,
            remote_owners: self.remote.clone(),
            pause_mask: 0,
            start_mask,
            stop_mask: self.stopping,
            running_mask: self.running,
            failures: self.failures.to_vec(),
            microphone_permission: self.permission,
            owner_generations: self.owners.to_vec(),
            audio_joined: self.audio_joined,
            forward_target: self.forward_target.map(|id| id.to_hex()),
        }
    }

    /// Native ownership follows the usable exit, not its M0 handshake. During
    /// a switch the packet route can be empty while AudioPolicy/ReplayKit keep
    /// capturing; revoking here would restore local output and end forwarding.
    fn update_forward_readiness(&mut self, available: bool, prepared: bool) {
        self.forward_ready = prepared;
        if !available && !self.speaker_exit && self.owned() & 4 != 0 && self.stopping & 4 == 0 {
            self.revoke(4);
        }
    }

    fn revoke(&mut self, resources: u8) {
        self.generation = next_native_generation();
        self.stopping |= self.owned() & resources;
        self.running &= !resources;
        for kind in KINDS {
            if resources & kind.bit() != 0
                && let Some(native) = &self.native[kind.index()]
            {
                native.revoke();
            }
        }
    }

    /// Case A AEC pairs the two native directions through the one control of
    /// their shared native generation (`audio::control_for`). A direction
    /// started in a newer generation than the running other one would get its
    /// own control: the microphone's processor would then have no reference
    /// and AEC would silently do nothing. So whenever one direction starts
    /// beside the other running an older generation, or both run apart, both
    /// stop and restart together in one new generation. Only while AEC is on;
    /// Android's independent streams are the case (an iOS engine starts both
    /// directions together).
    fn pair_case_a(&mut self) {
        if !self.echo_cancellation || self.closed || self.stopping != 0 {
            return;
        }
        let start = self.snapshot().start_mask;
        let stale = |owner: u64| owner != 0 && owner != self.generation;
        let [speaker, microphone, _] = self.owners;
        let beside_stale = (start & MobileMediaKind::Speaker.bit() != 0 && stale(microphone))
            || (start & MobileMediaKind::Microphone.bit() != 0 && stale(speaker));
        let apart = speaker != 0 && microphone != 0 && speaker != microphone;
        if beside_stale || apart {
            tracing::info!(target: oink_audio::echo::LOG_TARGET, event = "aec_pairing_restart",
                speaker_generation = speaker, microphone_generation = microphone,
                generation = self.generation,
                "restarting both audio directions in one generation for echo cancellation");
            self.revoke(DUPLEX_MASK);
        }
    }

    fn prune_remote(&mut self) {
        self.remote.retain(|owner| owner.resources != 0);
    }

    fn sync_intent(&mut self, previous: u8, reason: CloseReason, now: Instant) {
        let enabled = self.enabled();
        for kind in KINDS {
            let bit = kind.bit();
            match (previous & bit != 0, enabled & bit != 0) {
                (false, true) => {
                    self.idle.enable(kind.direction(), now);
                }
                (true, false) => {
                    self.idle.disable(kind.direction(), reason);
                }
                _ => {}
            }
        }
        if previous != enabled {
            self.revoke(previous ^ enabled);
        }
    }

    fn close(&mut self, kind: MobileMediaKind, reason: CloseReason, now: Instant) {
        let previous = self.enabled();
        let bit = kind.bit();
        self.local_enabled &= !bit;
        for owner in &mut self.remote {
            owner.resources &= !bit;
        }
        self.prune_remote();
        self.failures[kind.index()] = MobileMediaFailure::None;
        self.sync_intent(previous, reason, now);
        if self.activation_owner().is_some_and(|owner| owner.resources & bit != 0) {
            self.cancel_activation(now);
        }
    }

    /// After merging, an endpoint that is on while this phone does not hold
    /// its role lost it: another device's later choice won, or the group's
    /// choice was later than this phone's when it joined. The endpoint closes
    /// with Remote and the intent is cleared (M21); the phone is local again.
    fn follow_roles(&mut self, held: [bool; 2], intents: [Option<u64>; 2], now: Instant) {
        for kind in [MobileMediaKind::Speaker, MobileMediaKind::Microphone] {
            let index = kind.index();
            if self.audio_joined && intents[index].is_some() && !held[index] {
                self.close(kind, CloseReason::Remote, now);
            }
        }
    }

    fn advance_idle(&mut self, now: Instant) {
        for event in self.idle.poll(now).into_iter().flatten() {
            let kind = match event.direction {
                Direction::SpeakerReceive => MobileMediaKind::Speaker,
                Direction::MicrophoneSend => MobileMediaKind::Microphone,
                Direction::SpeakerSend => MobileMediaKind::Forward,
            };
            self.close(kind, event.reason, now);
        }
    }

    /// One direction of the published endpoint state, with the reason it is
    /// not running: needs permission, awaits a receiver's demand, the route
    /// setting failed (a recorded failure, L10), or a pending remote activation
    /// (review H6/M23).
    fn direction_view(&self, kind: MobileMediaKind, supported: bool) -> v2::Result<dto::EndpointDirection> {
        let snapshot = self.idle.snapshot(kind.direction());
        let enabled = self.enabled() & kind.bit() != 0;
        let microphone = kind == MobileMediaKind::Microphone;
        // Peers see the microphone running only while it is sent: an iOS
        // microphone that captures without a receiver still reports
        // Starting / AwaitingDemand, as on Android.
        let running = self.running & kind.bit() != 0 && (!microphone || self.microphone_ready);
        let permitted = !microphone || self.permission == MobileMicrophonePermission::Granted;
        let mut failure = self.failures[kind.index()];
        if failure == MobileMediaFailure::None && enabled && !running && !permitted {
            failure = MobileMediaFailure::PermissionRequired;
        }
        let mut raw = endpoint(supported, enabled, running, failure, snapshot)?.into_raw();
        if failure == MobileMediaFailure::None {
            if !enabled && !permitted {
                // Needs the system permission before it can be turned on.
                raw.insert(5, dto::EndpointCloseReason::PermissionRequired.write());
            } else if enabled && !running && microphone && !self.microphone_ready {
                raw.insert(5, dto::EndpointCloseReason::AwaitingDemand.write());
            }
        }
        if let Some(activation) = self.direction_activation(kind) {
            raw.insert(6, activation.write());
        }
        if !enabled && let Some(permission_required) = self.activation_pending(kind) {
            raw.insert(3, dto::EndpointPhase::Starting.write());
            let reason = if permission_required {
                dto::EndpointCloseReason::PermissionRequired
            } else {
                dto::EndpointCloseReason::None
            };
            raw.insert(5, reason.write());
        }
        dto::EndpointDirection::from_raw(raw)
    }

    fn endpoint_message(&self, network: &MobileNetwork, peers: &[(Boot, StreamId)]) -> v2::Result<Message> {
        let capabilities = network.platform_facts();
        let speaker = self.idle.snapshot(Direction::SpeakerReceive);
        let microphone = self.idle.snapshot(Direction::MicrophoneSend);
        let forward = self.idle.snapshot(Direction::SpeakerSend);
        let inactive = endpoint(
            false,
            false,
            false,
            MobileMediaFailure::None,
            Controller::default().snapshot(Direction::SpeakerReceive),
        )?;
        let remote = self.activation_owner();
        let owner_boot = remote.and_then(|owner| {
            peers
                .iter()
                .find(|(peer, _)| owner.matches_boot(*peer))
                .map(|(peer, _)| boot(*peer))
        });
        let close_reason = forward
            .close_reason
            .or(microphone.close_reason)
            .or(speaker.close_reason);
        dto::AudioEndpointState::from_raw(
            [
                (1, boot(network.boot()).write()),
                (2, self.revision.write()),
                (
                    3,
                    self.direction_view(MobileMediaKind::Microphone, capabilities.microphone_send)?
                        .write(),
                ),
                (4, inactive.write()),
                (
                    5,
                    self.direction_view(MobileMediaKind::Forward, capabilities.speaker_send)?
                        .write(),
                ),
                (
                    6,
                    self.direction_view(MobileMediaKind::Speaker, capabilities.speaker_receive)?
                        .write(),
                ),
                (7, owner_boot.write()),
                (
                    9,
                    speaker
                        .generation
                        .max(microphone.generation)
                        .max(forward.generation)
                        .write(),
                ),
                (
                    10,
                    speaker
                        .activity_seq
                        .saturating_add(microphone.activity_seq)
                        .saturating_add(forward.activity_seq)
                        .write(),
                ),
                (11, wire_reason(close_reason).write()),
            ]
            .into(),
        )
        .map(Message::AudioEndpointState)
    }
}

fn endpoint_result(
    local: Boot,
    request_id: dto::Id16,
    direction: dto::EndpointDirectionKind,
    outcome: EndpointOutcome,
) -> v2::Result<Message> {
    let (outcome, reason) = match outcome {
        Ok(outcome) => (outcome, Failure::None),
        Err(reason @ (Failure::Failed | Failure::Unavailable)) => (dto::ControlOutcome::Failed, reason),
        Err(reason) => (dto::ControlOutcome::Refused, reason),
    };
    dto::AudioEndpointResult::from_raw(
        [
            (1, request_id.write()),
            (2, boot(local).write()),
            (3, direction.write()),
            (4, outcome.write()),
            (5, reason.write()),
        ]
        .into(),
    )
    .map(Message::AudioEndpointResult)
}

fn wire_reason(reason: Option<CloseReason>) -> dto::EndpointCloseReason {
    use dto::EndpointCloseReason as Wire;
    match reason {
        None => Wire::None,
        Some(CloseReason::Manual | CloseReason::Pause) => Wire::Local,
        Some(CloseReason::Remote) => Wire::Remote,
        Some(CloseReason::NoData) => Wire::Idle,
        Some(CloseReason::Fault) => Wire::Error,
        Some(CloseReason::Interrupted) => Wire::Interrupted,
        Some(CloseReason::Permission) => Wire::PermissionRequired,
    }
}

fn endpoint(
    supported: bool,
    enabled: bool,
    running: bool,
    failure: MobileMediaFailure,
    snapshot: EndpointSnapshot,
) -> v2::Result<dto::EndpointDirection> {
    let (phase, reason) = match failure {
        MobileMediaFailure::Interrupted => (dto::EndpointPhase::Interrupted, dto::EndpointCloseReason::Interrupted),
        MobileMediaFailure::PermissionRequired => {
            (dto::EndpointPhase::Failed, dto::EndpointCloseReason::PermissionRequired)
        }
        MobileMediaFailure::SystemBlocked | MobileMediaFailure::Unavailable => {
            (dto::EndpointPhase::Failed, dto::EndpointCloseReason::Error)
        }
        MobileMediaFailure::None => (
            if running {
                dto::EndpointPhase::Running
            } else if enabled {
                dto::EndpointPhase::Starting
            } else {
                dto::EndpointPhase::Off
            },
            wire_reason(snapshot.close_reason),
        ),
    };
    // `enabled` only selects the phase: whether this phone is the exit or the
    // sender is published in the global audio configuration (key 33).
    dto::EndpointDirection::from_raw(
        [
            (1, supported.write()),
            (3, phase.write()),
            (4, snapshot.generation.write()),
            (5, reason.write()),
        ]
        .into(),
    )
}

/// This phone's own microphone sender gain (0..=200, 100 = unity). All
/// receivers hear the same processed stream; PCs edit it through C0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct MobileMicrophoneControls {
    pub volume_percent: u16,
    pub muted: bool,
    pub revision: u64,
}

#[derive(uniffi::Object)]
pub struct MobileMediaSession {
    state: Mutex<State>,
    pub(crate) volume: Mutex<MobileExitVolume>,
    controls: Mutex<MicrophoneControls>,
    forward_controls: Mutex<MicrophoneControls>,
    volume_presses: Mutex<VolumePresses>,
    diagnostics: Mutex<std::sync::Weak<crate::MobileDiagnosticCollector>>,
    changed: watch::Sender<MobileMediaState>,
    runtime: Handle,
    network: MobileNetwork,
    timer_changed: Arc<Notify>,
    settings_path: PathBuf,
    allowed: bool,
    /// The global audio configuration this phone executes and broadcasts.
    audio: Mutex<PhoneAudio>,
}

impl std::fmt::Debug for MobileMediaSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileMediaSession").finish_non_exhaustive()
    }
}

#[uniffi::export]
impl MobileMediaSession {
    pub fn snapshot(&self) -> MobileMediaState {
        self.state.lock().expect("media state mutex").snapshot()
    }

    pub fn set_enabled(&self, kind: MobileMediaKind, enabled: bool) -> MobileMediaState {
        let mut state = self.state.lock().expect("media state mutex");
        if self.allowed && !state.closed {
            let now = Instant::now();
            if enabled && kind == MobileMediaKind::Forward && !self.network.platform_facts().speaker_send {
                state.failures[kind.index()] = MobileMediaFailure::SystemBlocked;
            } else if enabled
                && kind == MobileMediaKind::Microphone
                && state.permission != MobileMicrophonePermission::Granted
            {
                // Review H13: without the permission the intent is not taken.
                // The endpoint says why (Failed, PermissionRequired) instead of
                // showing the microphone on and Starting forever.
                state.failures[kind.index()] = MobileMediaFailure::PermissionRequired;
            } else if enabled {
                if kind == MobileMediaKind::Speaker && state.enabled() & 4 != 0 {
                    state.close(MobileMediaKind::Forward, CloseReason::Manual, now);
                }
                let previous = state.enabled();
                state.local_enabled |= kind.bit();
                state.sync_intent(previous, CloseReason::Manual, now);
            } else {
                state.close(kind, CloseReason::Manual, now);
            }
        }
        self.publish(&mut state)
    }

    /// Android helper-observed consumer usages; never falls through to focus input.
    pub fn forward_volume_key(&self, usage: u16, pressed: bool) -> bool {
        if ![0xe9, 0xea, 0xe2].contains(&usage) {
            return false;
        }
        let forwarding = self.snapshot().running_mask & 4 != 0;
        let mut presses = self.volume_presses.lock().expect("forward volume presses");
        let repeat = pressed && presses.contains_key(&usage);
        let (route, press_id) = if pressed {
            if !forwarding {
                return false;
            }
            if let Some(press) = presses.get(&usage) {
                press.clone()
            } else {
                let Some(route) = self.network.volume_exit() else {
                    return false;
                };
                static NEXT: AtomicU64 = AtomicU64::new(1);
                let press = (route, NEXT.fetch_add(1, Ordering::Relaxed));
                presses.insert(usage, press.clone());
                press
            }
        } else {
            let Some(press) = presses.remove(&usage) else {
                return false;
            };
            press
        };
        self.send_forward_volume(route, press_id, usage, pressed, repeat)
    }

    pub fn idle_timeout(&self, kind: MobileMediaKind) -> MobileAudioIdleTimeout {
        self.state
            .lock()
            .expect("media state mutex")
            .idle
            .snapshot(kind.direction())
            .timeout
            .into()
    }

    pub fn set_idle_timeout(
        &self,
        kind: MobileMediaKind,
        timeout: MobileAudioIdleTimeout,
    ) -> Result<MobileMediaState, MobileError> {
        let mut state = self.state.lock().expect("media state mutex");
        let mut settings = KINDS.map(|kind| state.idle.snapshot(kind.direction()).timeout);
        settings[kind.index()] = timeout.into();
        self.save_settings(state.echo_cancellation, settings)?;
        state.idle.set_timeout(kind.direction(), timeout.into());
        self.wake_media();
        state.advance_idle(Instant::now());
        Ok(self.publish(&mut state))
    }

    pub fn echo_cancellation(&self) -> bool {
        self.state.lock().expect("media state mutex").echo_cancellation
    }

    pub fn set_echo_cancellation(&self, enabled: bool) -> Result<(), MobileError> {
        let mut state = self.state.lock().expect("media state mutex");
        self.save_settings(enabled, KINDS.map(|kind| state.idle.snapshot(kind.direction()).timeout))?;
        state.echo_cancellation = enabled;
        self.publish(&mut state);
        Ok(())
    }

    /// Read-only phone UI state; PC owns the usual 0..200 forwarding slider.
    pub fn forwarding_controls(&self) -> MobileMicrophoneControls {
        self.forward_controls.lock().expect("forward controls").snapshot()
    }

    pub fn microphone_controls(&self) -> MobileMicrophoneControls {
        self.controls.lock().expect("microphone controls mutex").snapshot()
    }

    /// Local edit of this phone's sender gain (the device sheet slider when
    /// this phone is the microphone sender). Persists before it applies.
    pub fn set_microphone_controls(
        &self,
        volume_percent: u16,
        muted: bool,
    ) -> Result<MobileMicrophoneControls, MobileError> {
        let mut controls = self.controls.lock().expect("microphone controls mutex");
        controls
            .set_local(volume_percent, muted)
            .map_err(|reason| MobileError::Command {
                code: match reason {
                    dto::ControlFailure::InvalidValue => crate::UserFacingCode::InvalidValue,
                    _ => crate::UserFacingCode::PersistenceFailed,
                },
                detail: format!("microphone controls: {reason:?}"),
            })?;
        let snapshot = controls.snapshot();
        drop(controls);
        // Republish the facts preference and apply the native gain.
        self.wake_media();
        Ok(snapshot)
    }

    pub fn set_microphone_permission(&self, permission: MobileMicrophonePermission) -> MobileMediaState {
        let mut state = self.state.lock().expect("media state mutex");
        if state.permission == MobileMicrophonePermission::Granted && permission != state.permission {
            state.close(MobileMediaKind::Microphone, CloseReason::Permission, Instant::now());
        }
        let microphone = MobileMediaKind::Microphone.index();
        if permission == MobileMicrophonePermission::Granted
            && state.failures[microphone] == MobileMediaFailure::PermissionRequired
        {
            // Granting clears the refusal; turning the microphone on is the
            // user's next, separate action.
            state.failures[microphone] = MobileMediaFailure::None;
        }
        state.permission = permission;
        self.publish(&mut state)
    }

    pub fn clear_failure(&self, kind: MobileMediaKind) -> MobileMediaState {
        let mut state = self.state.lock().expect("media state mutex");
        state.failures[kind.index()] = MobileMediaFailure::None;
        self.publish(&mut state)
    }

    pub fn report_failure(
        &self,
        kind: MobileMediaKind,
        generation: u64,
        failure: MobileMediaFailure,
    ) -> MobileMediaState {
        let mut state = self.state.lock().expect("media state mutex");
        if generation != 0 && (state.owners[kind.index()] == generation || state.generation == generation) {
            if kind == MobileMediaKind::Forward {
                state.close(kind, CloseReason::Fault, Instant::now());
            }
            state.failures[kind.index()] = failure;
            state.revoke(kind.bit());
        }
        self.publish(&mut state)
    }

    pub fn claim_start(&self, kind: MobileMediaKind, generation: u64) -> bool {
        let mut state = self.state.lock().expect("media state mutex");
        if generation != state.generation || state.snapshot().start_mask & kind.bit() == 0 {
            return false;
        }
        state.owners[kind.index()] = generation;
        self.publish(&mut state);
        true
    }

    pub fn acknowledge_started(&self, kind: MobileMediaKind, generation: u64) -> bool {
        let mut state = self.state.lock().expect("media state mutex");
        if generation == 0
            || state.owners[kind.index()] != generation
            || state.stopping & kind.bit() != 0
            || state.native[kind.index()].is_none()
        {
            return false;
        }
        state.running |= kind.bit();
        if kind == MobileMediaKind::Forward {
            state.close(MobileMediaKind::Speaker, CloseReason::Manual, Instant::now());
        }
        self.publish(&mut state);
        true
    }

    pub fn acknowledge_stopped(&self, kind: MobileMediaKind, generation: u64) -> MobileMediaState {
        let mut state = self.state.lock().expect("media state mutex");
        let index = kind.index();
        if generation != 0 && state.owners[index] == generation {
            if let Some(native) = state.native[index].take() {
                native.revoke();
            }
            state.owners[index] = 0;
            state.stopping &= !kind.bit();
            state.running &= !kind.bit();
        }
        self.publish(&mut state)
    }

    pub fn request_rebuild_resources(&self, resources: u8) -> MobileMediaState {
        let mut state = self.state.lock().expect("media state mutex");
        let resources = if state.enabled() & DUPLEX_MASK == DUPLEX_MASK && resources & DUPLEX_MASK != 0 {
            (resources & AUDIO_MASK) | DUPLEX_MASK
        } else {
            resources & AUDIO_MASK
        };
        state.revoke(resources);
        self.publish(&mut state)
    }

    pub fn attach_audio(&self, kind: MobileMediaKind, generation: u64, handle: u64) -> bool {
        let mut state = self.state.lock().expect("media state mutex");
        let index = kind.index();
        if generation == 0
            || generation != state.owners[index]
            || state.stopping & kind.bit() != 0
            || state.native[index].is_some()
        {
            return false;
        }
        let Some(native) = NativeAudio::lookup(handle) else {
            return false;
        };
        if native.generation() != generation
            || !matches!(
                (&native, kind),
                (NativeAudio::Output(_), MobileMediaKind::Speaker)
                    | (NativeAudio::Input(_), MobileMediaKind::Microphone)
                    | (NativeAudio::Forward(_), MobileMediaKind::Forward)
            )
        {
            return false;
        }
        native.set_aec_waker(self.timer_changed.clone());
        // Before the first callback: the iPhone exit plays at its own volume
        // from the first sample (lock order state → volume, as in the service).
        native.set_playout_gain(self.volume.lock().expect("exit volume mutex").playout_gain());
        if let NativeAudio::Forward(handle) = &native {
            // Capture may deliver as soon as the native start is acknowledged.
            // Apply persisted controls before any route can admit its first packet.
            let (gain, muted) = self.forward_controls.lock().expect("forward controls").gain();
            handle.set_gain(gain, muted);
        }
        state.native[index] = Some(native);
        self.publish(&mut state);
        true
    }

    pub fn subscribe(&self, listener: Box<dyn MobileMediaListener>) -> Arc<MobileMediaSubscription> {
        let mut changes = self.changed.subscribe();
        let task = self.runtime.spawn(async move {
            loop {
                let snapshot = changes.borrow_and_update().clone();
                listener.on_media_state(snapshot);
                if changes.changed().await.is_err() {
                    break;
                }
            }
        });
        Arc::new(MobileMediaSubscription {
            task: task.abort_handle(),
        })
    }
}

#[uniffi::export(callback_interface)]
pub trait MobileMediaListener: Send + Sync {
    fn on_media_state(&self, state: MobileMediaState);
}

#[derive(Debug, uniffi::Object)]
pub struct MobileMediaSubscription {
    task: tokio::task::AbortHandle,
}

#[uniffi::export]
impl MobileMediaSubscription {
    pub fn cancel(&self) {
        self.task.abort();
    }
}

impl Drop for MobileMediaSubscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl MobileMediaSession {
    pub(crate) fn new(network: MobileNetwork, allowed: bool, data_dir: PathBuf) -> Arc<Self> {
        let settings_path = data_dir.join("audio-options-v2.json");
        let mut state = State::default();
        if let Ok(bytes) = fs::read(&settings_path)
            && let Ok((enabled, timeouts)) = serde_json::from_slice::<(bool, [IdleTimeout; 3])>(&bytes)
                .inspect_err(|error| tracing::warn!(%error, "discarding unrecognized mobile audio options"))
        {
            state.echo_cancellation = enabled;
            for (kind, timeout) in KINDS.into_iter().zip(timeouts) {
                state.idle.set_timeout(kind.direction(), timeout);
            }
        }
        state.capture_while_enabled = cfg!(target_os = "ios");
        state.idle.set_timeout(Direction::SpeakerSend, IdleTimeout::Unlimited);
        let changed = watch::channel(state.snapshot()).0;
        let mut updates = changed.subscribe();
        let media = Arc::new(Self {
            state: Mutex::new(state),
            // iOS has no public API that sets the system output volume, so an
            // iPhone exit's volume is Oink's own playout gain, kept here.
            volume: Mutex::new(MobileExitVolume::new(
                network.boot(),
                cfg!(target_os = "ios")
                    .then(|| crate::playout_volume::PlayoutVolume::open(data_dir.join("playout-volume.json"))),
            )),
            controls: Mutex::new(MicrophoneControls::open(data_dir.join("microphone-controls-v2.cbor"))),
            forward_controls: Mutex::new(MicrophoneControls::open_option(
                data_dir.join("forward-controls-v2.cbor"),
                dto::LocalOption::ForwardingControls,
            )),
            volume_presses: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(std::sync::Weak::new()),
            changed,
            runtime: Handle::current(),
            network: network.clone(),
            timer_changed: Arc::new(Notify::new()),
            settings_path,
            allowed,
            // Never persisted: every core start begins all local (§2.6).
            audio: Mutex::new(PhoneAudio::new()),
        });
        if allowed {
            let weak = Arc::downgrade(&media);
            let timer_changed = media.timer_changed.clone();
            media.runtime.spawn(async move {
                let mut audio = AudioRuntime::new(network.boot());
                loop {
                    let Some(media) = weak.upgrade() else { break };
                    updates.borrow_and_update();
                    let deadline = media.service_audio(&network, &mut audio);
                    drop(media);
                    tokio::select! {
                        update = updates.changed() => if update.is_err() { break },
                        () = network.audio_changed() => {},
                        () = timer_changed.notified() => {},
                        () = async {
                            match deadline {
                                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                                None => std::future::pending().await,
                            }
                        } => {},
                    }
                }
            });
        }
        media
    }

    fn send_forward_volume(
        &self,
        route: oink_routing::input::VolumeRoute,
        press_id: u64,
        usage: u16,
        pressed: bool,
        repeat: bool,
    ) -> bool {
        let input = oink_routing::input::InputMsg::Volume {
            route: route.clone(),
            press_id,
            code: oink_foundation::input::KeyCode::new(oink_foundation::input::KeyCode::PAGE_CONSUMER, usage),
            pressed,
            repeat,
            modifiers: 0,
        };
        let mut bytes = vec![0; v2::limits::RELIABLE_FRAME];
        let Ok(len) = oink_routing::wire::input::encode(&input, None, &mut bytes) else {
            return false;
        };
        let Ok(frame) = v2::frame::OpaqueFrame::parse_exact(&bytes[..len], v2::limits::RELIABLE_FRAME) else {
            return false;
        };
        frame
            .decode()
            .is_ok_and(|message| self.network.send_message(route.target.device_id, message).is_ok())
    }

    fn retire_volume_keys(&self, forwarding: bool) {
        let current = self.network.volume_exit();
        let mut presses = self.volume_presses.lock().expect("forward volume presses");
        for (usage, route, id) in retire_volume_presses(&mut presses, current.as_ref(), forwarding) {
            self.send_forward_volume(route, id, usage, false, false);
        }
    }

    fn save_settings(&self, echo_cancellation: bool, timeouts: [IdleTimeout; 3]) -> Result<(), MobileError> {
        let bytes = serde_json::to_vec(&(echo_cancellation, timeouts)).map_err(|error| MobileError::Worker {
            detail: error.to_string(),
        })?;
        storage::atomic_write(&self.settings_path, &bytes).map_err(|error| MobileError::Worker {
            detail: error.to_string(),
        })
    }

    /// The collector the host opened for this session; AEC statistics go there.
    pub(crate) fn attach_diagnostics(&self, collector: &Arc<crate::MobileDiagnosticCollector>) {
        *self.diagnostics.lock().expect("media diagnostics mutex") = Arc::downgrade(collector);
    }

    /// Record Case A AEC bypass / over-budget blocks (contracts/audio-v2.md).
    /// A window without either is not recorded (D33: keep mobile data lean).
    fn record_aec(&self, stats: crate::audio::AecStats) {
        let Some(collector) = self.diagnostics.lock().expect("media diagnostics mutex").upgrade() else {
            return;
        };
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        for (code, count) in [
            (oink_mobile_core::DiagnosticCode::AecBypass, stats.bypass_blocks),
            (oink_mobile_core::DiagnosticCode::AecDeadlineMiss, stats.deadline_misses),
        ] {
            if count == 0 {
                continue;
            }
            let _ = collector.record(oink_mobile_core::DiagnosticEvent {
                timestamp_ms,
                kind: oink_mobile_core::DiagnosticKind::Audio,
                code,
                metric: Some(stats.blocks),
                count: u32::try_from(count).unwrap_or(u32::MAX),
            });
        }
    }

    /// A refused endpoint command is kept in the phone's diagnostics (M22).
    fn record_refusal(&self, reason: Failure) {
        let Some(collector) = self.diagnostics.lock().expect("media diagnostics mutex").upgrade() else {
            return;
        };
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let _ = collector.record(oink_mobile_core::DiagnosticEvent {
            timestamp_ms,
            kind: oink_mobile_core::DiagnosticKind::Error,
            code: oink_mobile_core::DiagnosticCode::Failed,
            metric: Some(u64::from(reason.number())),
            count: 1,
        });
    }

    pub(crate) fn wake_media(&self) {
        self.timer_changed.notify_one();
    }

    /// The group's speaker exit as the volume-key context of this phone's
    /// control page: this phone while it is the exit, otherwise the selected
    /// exit's published volume state if it belongs to the exit's current Boot
    /// and route version. `None` while there is no exit or its volume is not
    /// Ready; the keys then stay ordinary keys for the focus computer.
    fn volume_key_route(
        &self,
        network: &MobileNetwork,
        local: Boot,
        volume: &MobileExitVolume,
    ) -> Option<oink_routing::input::VolumeRoute> {
        let (route, exit) = self.with_audio(|audio| {
            Some((
                audio.route(oink_audio::route::Role::Speaker)?,
                audio.holder(oink_audio::route::Role::Speaker)?,
            ))
        })?;
        if exit == local.device_id {
            return volume.own_key_route();
        }
        let facts = network.peer_facts(exit)?;
        let state = facts.exit_volume().ok()??;
        let version = state.version().ok()?;
        if facts.boot().ok()? != version.exit().ok()? || version.route_version().ok()? != route.version.to_wire() {
            return None;
        }
        crate::exit_volume::key_route(local, &state)
    }

    /// Read the global audio configuration this phone executes.
    pub(crate) fn with_audio<T>(&self, read: impl FnOnce(&PhoneAudio) -> T) -> T {
        read(&self.audio.lock().expect("audio configuration mutex"))
    }

    /// This phone's user set a device's speaker or microphone mode (for
    /// example a PC's exit or forward switch from its card). The phone authors
    /// it and broadcasts the whole configuration with its next facts; no PC
    /// is asked (setter decides, protocol-v2 6.1).
    pub(crate) fn set_audio_mode(
        &self,
        device: DeviceId,
        role: oink_audio::route::Role,
        mode: oink_audio::route::Mode,
    ) -> Result<dto::ControlOutcome, dto::ControlFailure> {
        let outcome = self
            .audio
            .lock()
            .expect("audio configuration mutex")
            .select(device, role, mode)?;
        self.wake_media();
        Ok(outcome)
    }

    fn publish(&self, state: &mut State) -> MobileMediaState {
        state.pair_case_a();
        if let Some(input) = &state.native[MobileMediaKind::Microphone.index()] {
            input.set_aec(
                state.echo_cancellation,
                state.microphone_ready && state.running & 2 != 0,
                state.speaker_exit && state.running & 1 != 0,
            );
        }
        let snapshot = state.snapshot();
        if self.changed.send_if_modified(|published| {
            if *published == snapshot {
                return false;
            }
            *published = snapshot.clone();
            true
        }) {
            state.revision += 1;
        }
        snapshot
    }

    fn service_audio(&self, network: &MobileNetwork, audio: &mut AudioRuntime) -> Option<Instant> {
        let mut state = self.state.lock().expect("media state mutex");
        let now = Instant::now();
        let previous = state.snapshot();
        let group = network.group_id();
        if audio.group.replace(group).is_some_and(|prior| prior != group) {
            state.cancel_activation(now);
            state.close(MobileMediaKind::Forward, CloseReason::Remote, now);
            state.revoke(AUDIO_MASK);
            state.speaker_exit = false;
            state.microphone_ready = false;
            audio.speaker = None;
            audio.microphone.set_route(None);
            audio.sent.clear();
        }
        let peers = network.audio_peers();
        state.refresh_activation(network, &peers, now);
        let messages = network.poll_audio_messages();
        for (peer, message) in &messages {
            let Some((_, route)) = peers.iter().find(|(current, _)| current == peer) else {
                continue;
            };
            let (request_id, direction, outcome) = match message {
                Message::AudioEndpointRequest(request) => (
                    request.request_id(),
                    request.direction(),
                    state.activation_message(network, *peer, *route, message, now),
                ),
                Message::AudioEndpointRelease(request) => (
                    request.request_id(),
                    request.direction(),
                    state.activation_message(network, *peer, *route, message, now),
                ),
                _ => continue,
            };
            if let Err(reason) = outcome {
                tracing::warn!(reason = reason.number(), "refused an audio endpoint command");
                self.record_refusal(reason);
            }
            // An accepted activation request is answered when it settles
            // (activated, expired or declined); everything else at once.
            let pending = matches!(message, Message::AudioEndpointRequest(_)) && outcome.is_ok();
            if !pending
                && let (Ok(request_id), Ok(direction)) = (request_id, direction)
                && let Ok(result) = endpoint_result(network.boot(), request_id, direction, outcome)
            {
                let _ = network.send_audio_message(*peer, *route, &result);
            }
            // Return the authoritative state even if a stale command was rejected.
            audio
                .sent
                .retain(|stream, _| !peers.iter().any(|(boot, route)| boot == peer && route == stream));
        }
        state.refresh_activation(network, &peers, now);
        for (peer, request_id, direction, outcome) in std::mem::take(&mut state.activation.results) {
            if let Err(reason) = outcome {
                self.record_refusal(reason);
            }
            if let Some((_, route)) = peers.iter().find(|(current, _)| *current == peer)
                && let Ok(result) = endpoint_result(network.boot(), request_id, direction, outcome)
            {
                let _ = network.send_audio_message(peer, *route, &result);
            }
        }
        let local = network.boot();
        let capabilities = network.platform_facts();
        if !capabilities.speaker_send && state.enabled() & 4 != 0 {
            state.close(MobileMediaKind::Forward, CloseReason::Fault, now);
            state.failures[2] = MobileMediaFailure::SystemBlocked;
        }
        // Linked online members: a ready link and its current Boot's facts.
        let linked: Vec<_> = network
            .admitted_peers()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|device| {
                let facts = network.peer_facts(device)?;
                Some(oink_audio::route::Peer {
                    device,
                    config: peer_config(&facts),
                })
            })
            .collect();
        let supported = [
            capabilities.speaker_receive,
            capabilities.microphone_send && state.permission == MobileMicrophonePermission::Granted,
        ];
        let intents = [MobileMediaKind::Speaker, MobileMediaKind::Microphone].map(|kind| {
            (supported[kind.index()] && state.enabled() & kind.bit() != 0)
                .then(|| state.idle.snapshot(kind.direction()).generation)
        });
        let ([speaker_route, microphone_route], speaker_runs) = {
            let mut audio_config = self.audio.lock().expect("audio configuration mutex");
            let forward = state.enabled() & 4 != 0 && (!state.speaker_exit || state.running & 4 != 0);
            let versions = audio_config.reconcile_with_forward(
                group,
                local.device_id,
                &linked,
                intents,
                forward.then(|| state.idle.snapshot(Direction::SpeakerSend).generation),
                wall_ms(),
            );
            state.audio_joined = audio_config.joined();
            (versions, audio_config.runs_own(oink_audio::route::Role::Speaker))
        };
        let forward_available = {
            let config = self.audio.lock().expect("audio configuration mutex");
            // Keep the old exit until platform capture has acknowledged success.
            let forward = state.enabled() & 4 != 0 && (!state.speaker_exit || state.running & 4 != 0);
            if state.audio_joined
                && forward
                && config.switch(local.device_id, oink_audio::route::Role::Speaker) == Some(false)
            {
                state.close(MobileMediaKind::Forward, CloseReason::Remote, now);
            }
            let target = config
                .holder(oink_audio::route::Role::Speaker)
                .filter(|id| *id != local.device_id);
            state.forward_target = if state.enabled() & 4 != 0 { target } else { None };
            let available = capabilities.speaker_send
                && target.is_some_and(|id| {
                    linked.iter().any(|peer| peer.device == id)
                        && network.peer_facts(id).is_some_and(|facts| {
                            facts.observed().is_ok_and(|observed| observed.awake() == Ok(true))
                                && facts
                                    .pause()
                                    .is_ok_and(|pause| pause.is_none_or(|pause| pause.paused() == Ok(false)))
                        })
                });
            audio.source.reconcile(
                network,
                &peers,
                if forward && state.enabled() & 4 != 0 && available {
                    target.zip(config.route(oink_audio::route::Role::Speaker).map(|r| r.version))
                } else {
                    None
                },
            );
            available
        };
        // Another device's later choice took the exit or microphone sender
        // from this phone, or the group's was later when it joined: turn the
        // endpoint off instead of leaving it on beside the holder (M21). The
        // user turns it on again here.
        state.follow_roles([speaker_route.is_some(), microphone_route.is_some()], intents, now);
        let intents = [MobileMediaKind::Speaker, MobileMediaKind::Microphone].map(|kind| {
            (supported[kind.index()] && state.enabled() & kind.bit() != 0)
                .then(|| state.idle.snapshot(kind.direction()).generation)
        });

        // The native speaker session follows this phone's own configuration
        // (also before merging, see `runs_own`); the transport and exit volume
        // below still bind only a register version this phone holds, so no
        // source sends here until the group made it the exit.
        let speaker_exit = speaker_runs && intents[0].is_some() && state.running & 4 == 0;
        if state.speaker_exit && !speaker_exit {
            state.revoke(MobileMediaKind::Speaker.bit());
        } else if !state.speaker_exit && speaker_exit && state.running & 2 != 0 {
            state.revoke(DUPLEX_MASK);
        }
        state.speaker_exit = speaker_exit;
        let output = state.speaker_binding(local);
        if let Some(output) = &output {
            let generation = state.owners[0];
            if audio.speaker.as_ref().is_none_or(|(old, _)| *old != generation)
                && let Some(sources) = state.native[0].as_ref().and_then(NativeAudio::sources)
            {
                audio.speaker = Some((generation, SpeakerTransport::new(sources)));
            }
            if let Some((_, speaker)) = &mut audio.speaker {
                if let Some(arrived) = speaker.take_activity() {
                    state.idle.record_activity(Direction::SpeakerReceive, arrived);
                }
                speaker.set_route(speaker_route.map(|version| (version.to_wire(), output.clone())));
                speaker.retain_peers(|peer, stream| {
                    network
                        .audio_datagram_receiver(peer)
                        .is_some_and(|receiver| receiver.follows() == stream)
                });
            }
        } else {
            audio.speaker = None;
        }
        audio.microphone.set_route(
            microphone_route
                .filter(|_| intents[1].is_some())
                .map(|version| version.to_wire()),
        );

        for (peer, message) in messages {
            let Some((_, stream)) = peers.iter().find(|(current, _)| *current == peer) else {
                continue;
            };
            let (reply, microphone_activity) = audio.microphone.receive(local, peer, &message, now);
            if microphone_activity {
                state.idle.record_activity(Direction::MicrophoneSend, now);
            }
            let response = reply
                .or_else(|| audio.source.receive(network, peer, &message))
                .or_else(|| {
                    let Some((_, speaker)) = audio.speaker.as_mut() else {
                        // Not running as the exit: refuse offers so the source stops waiting.
                        return match &message {
                            Message::AudioSourceRouteOffer(offer) | Message::AudioQualitySourceOffer(offer) => {
                                crate::speaker_transport::refuse_offer(offer, dto::ControlFailure::NotExit, None)
                            }
                            _ => None,
                        };
                    };
                    match message {
                        Message::AudioSourceRouteOffer(_)
                        | Message::AudioQualitySourceOffer(_)
                        | Message::AudioPreparedAck(_)
                        | Message::AudioRouteFault(_) => speaker.receive(
                            peer,
                            &message,
                            network.audio_datagram_receiver(peer),
                            &network.audio_support(peer),
                        ),
                        _ => None,
                    }
                });
            if let Some(response) = response {
                let _ = network.send_audio_message(peer, *stream, &response);
            }
        }
        state.advance_idle(now);
        let routes = audio.microphone.routes(network, &peers, now);
        let microphone_ready = !routes.is_empty() && state.enabled() & MobileMediaKind::Microphone.bit() != 0;
        let demand_changed = state.microphone_ready != microphone_ready;
        if demand_changed && !state.capture_while_enabled {
            let affected = if microphone_ready && state.running & MobileMediaKind::Speaker.bit() != 0 {
                // Speaker receive and microphone share one Case A clock epoch.
                DUPLEX_MASK
            } else {
                MobileMediaKind::Microphone.bit()
            };
            state.revoke(affected);
        }
        // iOS keeps capturing across demand changes; only the routes below
        // (and the AEC reference in `publish`) follow the demand.
        state.microphone_ready = microphone_ready;
        if let Some(marked) = audio_marking(
            &mut audio.marked,
            state.speaker_exit,
            state.microphone_ready || state.running & 4 != 0,
        ) {
            network.set_audio_marking(marked);
        }
        let mut volume = self.volume.lock().expect("exit volume mutex");
        let speaker_version = speaker_route.map(|version| version.to_wire());
        let output = state.speaker_binding(local);
        let _ = volume.set_route(speaker_version.as_ref().zip(output.as_ref()));
        let _ = volume.refresh();
        // Volume keys for this phone as the exit (a forwarding focus computer's
        // or this phone's own control page); the readback is published below.
        for key in network.poll_exit_volume_keys() {
            if let Err(reason) = volume.key(key) {
                tracing::info!(reason = reason.number(), "exit volume key refused");
            }
        }
        if let Some(speaker) = &state.native[0] {
            speaker.set_playout_gain(volume.playout_gain());
        }
        network.set_volume_exit(self.volume_key_route(network, local, &volume));

        let mut results = Vec::new();
        for (peer, message) in network.poll_audio_config() {
            if !peers.iter().any(|(current, _)| *current == peer) {
                continue;
            }
            match message {
                Message::ConfigSetExitVolume(request) => {
                    results.push((peer, Message::ConfigExitVolumeResult(volume.handle(peer, request))));
                }
                Message::ConfigSetLocalOption(request) => {
                    let forwarding = request.option() == Ok(dto::LocalOption::ForwardingControls);
                    let controls = if forwarding {
                        &self.forward_controls
                    } else {
                        &self.controls
                    };
                    if let Ok(result) = controls.lock().expect("audio controls mutex").handle(
                        local,
                        peer,
                        request,
                        forwarding || capabilities.microphone_send,
                    ) {
                        results.push((peer, Message::ConfigLocalEditResult(result)));
                    }
                }
                _ => {}
            }
        }
        if let Some(input) = &state.native[1] {
            input.set_microphone_routes(if microphone_ready && state.running & 2 != 0 {
                routes
            } else {
                vec![]
            });
            let (gain, muted) = self.controls.lock().expect("microphone controls mutex").gain();
            input.set_microphone_gain(gain, muted);
            if let Some(stats) = input.take_aec_stats() {
                self.record_aec(stats);
            }
        }
        state.update_forward_readiness(
            forward_available && !audio.source.failed(),
            capabilities.speaker_send && audio.source.route(network).is_some(),
        );
        if let Some(NativeAudio::Forward(handle)) = &state.native[2] {
            handle.set_route(if state.running & 4 != 0 {
                audio.source.route(network)
            } else {
                None
            });
            let (gain, muted) = self.forward_controls.lock().expect("forward controls").gain();
            handle.set_gain(gain, muted);
        }
        log_case_a(&state, audio);
        let aec_deadline = state.native[1].as_ref().and_then(NativeAudio::service_aec);
        let facts_sent = network.edit_local_facts(|facts| {
            self.audio.lock().expect("audio configuration mutex").apply_facts(
                facts,
                self.controls.lock().expect("microphone controls mutex").preference()?,
            );
            state.apply_audio_facts(facts, local)?;
            let forward_controls = self.forward_controls.lock().expect("forward controls");
            if let Some(v2::RawValue::Array(preferences)) = facts.get_mut(&18) {
                // DeviceFacts preferences are strictly ordered by option number:
                // ForwardingControls (15) precedes MicrophoneControls (23).
                preferences.insert(0, forward_controls.preference()?.write());
            }
            if let Some(v2::RawValue::Map(resources)) = facts.get_mut(&16)
                && let Some(v2::RawValue::Map(speaker)) = resources.get_mut(&5)
                && state.enabled() & 4 != 0
            {
                let (gain, muted) = forward_controls.gain();
                speaker.insert(1, audio.source.session().write());
                speaker.insert(
                    2,
                    if state.running & 4 != 0 && audio.source.session().is_some() {
                        dto::SpeakerActivity::SourceForwarding
                    } else {
                        dto::SpeakerActivity::WaitingForExit
                    }
                    .write(),
                );
                speaker.insert(3, gain.write());
                speaker.insert(4, muted.write());
                speaker.insert(8, state.forward_target.map(oink_network::wire_identity::device).write());
            }
            facts.insert(17, volume.state().cloned().write());
            Ok(())
        });
        drop(volume);
        self.retire_volume_keys(state.running & 4 != 0);
        let fact_sequence = facts_sent.ok().and_then(|_| network.local_fact_sequence().ok());
        for (peer, mut message) in results {
            match &mut message {
                Message::ConfigLocalEditResult(result) => {
                    if publish_result(result, fact_sequence).is_err() {
                        continue;
                    }
                }
                Message::ConfigExitVolumeResult(result)
                    if fact_sequence.is_none()
                        && matches!(
                            result.outcome(),
                            Ok(dto::ControlOutcome::Applied | dto::ControlOutcome::NoChange)
                        ) =>
                {
                    let _ = result.edit(|raw| {
                        raw.insert(3, dto::ControlOutcome::Failed.write());
                        raw.insert(4, dto::ControlFailure::ApplyFailed.write());
                    });
                }
                _ => {}
            }
            let _ = network.send_audio_config_result(peer, message);
        }
        state.refresh_activation(network, &peers, now);
        // A demand change that leaves the snapshot alone (iOS captures
        // anyway) still updates the native input's AEC state in `publish`.
        if state.snapshot() != previous || demand_changed {
            self.publish(&mut state);
        }
        audio
            .sent
            .retain(|stream, _| peers.iter().any(|(_, current)| current == stream));
        if let Ok(message) = state.endpoint_message(network, &peers) {
            for (peer, stream) in peers {
                if audio.sent.get(&stream) != Some(&state.revision)
                    && network.send_audio_message(peer, stream, &message).is_ok()
                {
                    audio.sent.insert(stream, state.revision);
                }
            }
        }
        [
            state.idle.next_deadline(),
            audio.microphone.next_deadline(),
            state.activation_deadline(),
            aec_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) async fn shutdown(&self) {
        let mut changes = self.changed.subscribe();
        {
            let mut state = self.state.lock().expect("media state mutex");
            state.closed = true;
            state.cancel_activation(Instant::now());
            for kind in KINDS {
                state.close(kind, CloseReason::Manual, Instant::now());
            }
            self.publish(&mut state);
        }
        // Revocation is synchronous. Leave retained handles safe if the OS
        // already destroyed its service or a native stop callback is delayed.
        let _ = tokio::time::timeout(Duration::from_secs(5), changes.wait_for(|state| state.stop_mask == 0)).await;
    }
}

struct AudioRuntime {
    group: Option<GroupId>,
    speaker: Option<(u64, SpeakerTransport)>,
    microphone: MicrophoneTransport,
    source: crate::source_transport::SourceTransport,
    sent: HashMap<StreamId, u64>,
    /// The link socket is marked for audio ([`audio_marking`]).
    marked: bool,
    /// Rate-limited `oink_echo` lines of the running capture's processor.
    aec_log: oink_audio::echo::SummaryLog,
    /// Last logged (speaker generation, microphone generation, paired).
    aec_pairing: Option<(u64, u64, bool)>,
    /// Native callbacks / callbacks without host time since the last line:
    /// (render, render unknown, capture, capture unknown).
    host_times: [u64; 4],
}

impl AudioRuntime {
    fn new(local: Boot) -> Self {
        Self {
            group: None,
            speaker: None,
            microphone: MicrophoneTransport::default(),
            source: crate::source_transport::SourceTransport::new(local),
            sent: HashMap::new(),
            marked: false,
            aec_log: oink_audio::echo::SummaryLog::new(),
            aec_pairing: None,
            host_times: [0; 4],
        }
    }
}

/// The phone's `oink_echo` lines (behaviour-neutral): which native generations
/// the two directions run and whether they share one Case A control, the
/// processor windows, and how many native callbacks lacked a host time.
fn log_case_a(state: &State, audio: &mut AudioRuntime) {
    let generation = |index: usize| state.native[index].as_ref().map_or(0, NativeAudio::generation);
    let (speaker, microphone) = (generation(0), generation(1));
    let paired = match (&state.native[0], &state.native[1]) {
        (Some(output), Some(input)) => output.shares_case_a_control(input),
        _ => false,
    };
    if audio.aec_pairing != Some((speaker, microphone, paired)) {
        audio.aec_pairing = Some((speaker, microphone, paired));
        if speaker != 0 && microphone != 0 && !paired {
            tracing::warn!(target: oink_audio::echo::LOG_TARGET, event = "aec_pairing",
                output_generation = speaker, input_generation = microphone, paired,
                "speaker and microphone run in different native generations: no echo reference");
        } else {
            tracing::info!(target: oink_audio::echo::LOG_TARGET, event = "aec_pairing",
                output_generation = speaker, input_generation = microphone, paired,
                "echo cancellation native pairing");
        }
    }
    for (index, native) in state.native.iter().take(2).enumerate() {
        if let Some(native) = native {
            let (callbacks, unknown) = native.take_host_time_counts();
            audio.host_times[index * 2] += callbacks;
            audio.host_times[index * 2 + 1] += unknown;
        }
    }
    let Some(input) = &state.native[1] else {
        audio.aec_log = oink_audio::echo::SummaryLog::new();
        return;
    };
    let now = Instant::now();
    while let Some(summary) = input.take_aec_summary() {
        let lines = audio.aec_log.record(summary, now);
        if lines.has_summary() {
            let [render, render_unknown, capture, capture_unknown] = std::mem::take(&mut audio.host_times);
            tracing::info!(target: oink_audio::echo::LOG_TARGET, event = "aec_mobile",
                output_generation = speaker, input_generation = microphone, paired,
                render_callbacks = render, render_host_unknown = render_unknown,
                capture_callbacks = capture, capture_host_unknown = capture_unknown,
                "echo cancellation native timing");
        }
        lines.emit();
    }
}

/// The link socket's audio marking (iOS: voice service type, user 2026-11-11)
/// follows the roles, not the packets: marked while this phone is the speaker
/// exit or a receiver takes its microphone, so silence or a native stream
/// rebuild does not toggle it. Returns the new value when it changes.
fn audio_marking(marked: &mut bool, speaker_exit: bool, microphone_ready: bool) -> Option<bool> {
    let wanted = speaker_exit || microphone_ready;
    (*marked != wanted).then(|| {
        *marked = wanted;
        wanted
    })
}

impl State {
    fn speaker_binding(&self, local: Boot) -> Option<dto::ReceiverOutputBinding> {
        if !self.speaker_exit || self.running & 1 == 0 {
            return None;
        }
        dto::ReceiverOutputBinding::from_raw(
            [
                (1, boot(local).write()),
                (2, self.owners[0].write()),
                (3, v2::RawValue::from("mobile:default")),
            ]
            .into(),
        )
        .ok()
    }

    fn apply_audio_facts(&self, facts: &mut v2::RawMap, local: Boot) -> v2::Result<()> {
        let output = self.speaker_binding(local);
        let speaker = dto::SpeakerObservation::from_raw(
            [
                (
                    2,
                    if output.is_some() {
                        dto::SpeakerActivity::ExitReady
                    } else if self.failures[2] != MobileMediaFailure::None {
                        dto::SpeakerActivity::Failed
                    } else {
                        dto::SpeakerActivity::Unused
                    }
                    .write(),
                ),
                (5, output.write()),
                (7, output.is_some().write()),
            ]
            .into(),
        )?;
        let permission = match self.permission {
            MobileMicrophonePermission::NotDetermined => dto::MicrophonePermission::NotDetermined,
            MobileMicrophonePermission::Granted => dto::MicrophonePermission::Granted,
            MobileMicrophonePermission::Denied => dto::MicrophonePermission::Denied,
            MobileMicrophonePermission::Restricted => dto::MicrophonePermission::Restricted,
        };
        let microphone = dto::MicrophoneObservation::from_raw(
            [
                (1, permission.write()),
                (2, false.write()),
                (3, v2::RawValue::Array(vec![])),
                (5, false.write()),
                (6, (self.running & 2 != 0).write()),
            ]
            .into(),
        )?;
        let mut resources = facts
            .get(&16)
            .and_then(|value| value.as_map().ok())
            .cloned()
            .unwrap_or_default();
        resources.insert(5, speaker.write());
        resources.insert(8, microphone.write());
        facts.insert(16, v2::RawValue::Map(resources));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;
    use oink_protocol::v2::schema::FixedBytes;

    #[test]
    fn audio_marking_follows_the_roles_and_reports_only_changes() {
        let mut marked = false;
        // No role: nothing to send, not even the first time.
        assert_eq!(audio_marking(&mut marked, false, false), None);
        // Becoming the exit marks once.
        assert_eq!(audio_marking(&mut marked, true, false), Some(true));
        assert_eq!(audio_marking(&mut marked, true, false), None);
        // A receiver also takes the microphone: still marked, no resend.
        assert_eq!(audio_marking(&mut marked, true, true), None);
        // Exit lost, microphone still sent.
        assert_eq!(audio_marking(&mut marked, false, true), None);
        assert!(marked);
        // Last role ends (or a group switch / shutdown cleared both): unmark once.
        assert_eq!(audio_marking(&mut marked, false, false), Some(false));
        assert_eq!(audio_marking(&mut marked, false, false), None);
        // Microphone alone marks again.
        assert_eq!(audio_marking(&mut marked, false, true), Some(true));
        assert!(marked);
    }

    fn enable(state: &mut State, kind: MobileMediaKind, now: Instant) {
        let previous = state.enabled();
        state.local_enabled |= kind.bit();
        state.sync_intent(previous, CloseReason::Manual, now);
    }

    #[test]
    fn idle_retires_only_the_due_direction_and_waits_for_native_stop() {
        let mut state = State::default();
        let now = Instant::now();
        let speaker = MobileMediaKind::Speaker;
        let microphone = MobileMediaKind::Microphone;
        state.idle.set_timeout(microphone.direction(), IdleTimeout::Unlimited);
        enable(&mut state, speaker, now);
        enable(&mut state, microphone, now);
        let epoch = state.generation;
        state.owners[0] = epoch;
        state.running = speaker.bit();

        state.advance_idle(now + Duration::from_secs(299));
        assert_eq!(state.snapshot().running_mask, speaker.bit());
        state.advance_idle(now + Duration::from_secs(300));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.enabled_mask, microphone.bit());
        assert_eq!(snapshot.stop_mask, speaker.bit());
        assert_eq!(snapshot.running_mask, 0);
        assert_eq!(snapshot.start_mask, 0);
        assert_eq!(snapshot.owner_generations[0], epoch);
        assert_eq!(
            state.idle.snapshot(speaker.direction()).close_reason,
            Some(CloseReason::NoData)
        );
    }

    #[test]
    fn refused_commands_are_answered_with_their_reason() {
        let local = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [2; 16],
        };
        let result = endpoint_result(
            local,
            FixedBytes([5; 16]),
            dto::EndpointDirectionKind::SpeakerReceive,
            Err(Failure::Stale),
        )
        .unwrap();
        let Message::AudioEndpointResult(result) = result else {
            panic!()
        };
        assert_eq!(result.outcome(), Ok(dto::ControlOutcome::Refused));
        assert_eq!(result.reason(), Ok(Failure::Stale));
    }

    /// M22: a remote activation of one direction never fences the other.
    #[test]
    fn an_activation_belongs_to_its_own_direction() {
        let mut state = State::default();
        let now = Instant::now();
        let peer = Boot {
            device_id: DeviceId([3; 32]),
            nonce: [4; 16],
        };
        state.remote.push(MobileRemoteMediaOwner {
            caller: peer.device_id.to_hex(),
            boot_nonce: peer.nonce.to_vec(),
            request: vec![8; 16],
            resources: MobileMediaKind::Speaker.bit(),
        });
        enable(&mut state, MobileMediaKind::Microphone, now);
        assert_eq!(
            state.direction_activation(MobileMediaKind::Speaker),
            Some(FixedBytes([8; 16]))
        );
        assert_eq!(state.direction_activation(MobileMediaKind::Microphone), None);
    }

    /// M21 and the global audio configuration: once merged, an endpoint that
    /// is on while this phone does not hold its role lost it to a later choice
    /// (or to the group's later choice when it joined). It closes with Remote
    /// and the intent is cleared. Before merging nothing closes: the phone's
    /// own configuration is still waiting for the group.
    #[test]
    fn losing_the_role_closes_the_endpoint_with_remote_only_after_merging() {
        let mut state = State::default();
        let now = Instant::now();
        let speaker = MobileMediaKind::Speaker;
        enable(&mut state, speaker, now);
        let intents = [Some(state.idle.snapshot(speaker.direction()).generation), None];
        state.follow_roles([false, false], intents, now);
        assert_eq!(state.enabled(), speaker.bit(), "unjoined: waiting for connection");
        assert!(!state.snapshot().audio_joined);
        state.audio_joined = true;
        assert!(state.snapshot().audio_joined);
        state.follow_roles([true, false], intents, now);
        assert_eq!(state.enabled(), speaker.bit());
        state.follow_roles([false, false], intents, now);
        assert_eq!(state.enabled(), 0, "intent cleared");
        assert_eq!(
            state.idle.snapshot(speaker.direction()).close_reason,
            Some(CloseReason::Remote)
        );
        // An endpoint without intent closes nothing.
        state.follow_roles([false, false], [None, None], now);
        assert_eq!(state.enabled(), 0);
    }

    /// Endpoint reasons say why a direction is not running (review H6/M23).
    #[test]
    fn endpoint_directions_report_awaiting_demand_and_permission() {
        let mut state = State::default();
        let now = Instant::now();
        let microphone = MobileMediaKind::Microphone;
        assert_eq!(
            state.direction_view(microphone, true).unwrap().reason(),
            Ok(dto::EndpointCloseReason::PermissionRequired)
        );
        state.permission = MobileMicrophonePermission::Granted;
        enable(&mut state, microphone, now);
        let view = state.direction_view(microphone, true).unwrap();
        assert_eq!(view.phase(), Ok(dto::EndpointPhase::Starting));
        assert_eq!(view.reason(), Ok(dto::EndpointCloseReason::AwaitingDemand));
        // The mode itself is the global audio configuration: no `enabled` key.
        assert!(!view.raw().contains_key(&2));
    }

    /// Case A pairs the speaker and the microphone through one native
    /// generation. A direction that (re)starts beside the other running an
    /// older generation (Android: one direction failed and came back alone)
    /// restarts both together; directions already running apart restart too.
    #[test]
    fn case_a_directions_always_run_in_one_generation() {
        let now = Instant::now();
        let speaker = MobileMediaKind::Speaker;
        let microphone = MobileMediaKind::Microphone;
        let mut state = State {
            permission: MobileMicrophonePermission::Granted,
            ..State::default()
        };
        enable(&mut state, speaker, now);
        enable(&mut state, microphone, now);
        state.speaker_exit = true;
        let first = state.generation;
        state.owners[0] = first;
        state.running = speaker.bit();
        // A receiver takes the microphone after a revoke moved the generation
        // on (for example the microphone failed and was cleared again).
        state.microphone_ready = true;
        state.revoke(microphone.bit());
        assert_ne!(state.generation, first);
        assert_eq!(state.snapshot().start_mask, microphone.bit());
        state.pair_case_a();
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.stop_mask,
            speaker.bit(),
            "the speaker restarts with the microphone"
        );
        assert_eq!(snapshot.start_mask, 0, "nothing starts alone in the newer generation");
        // The native speaker stopped: both start, in the same generation.
        state.owners[0] = 0;
        state.stopping = 0;
        assert_eq!(state.snapshot().start_mask, DUPLEX_MASK);
        state.owners = [state.generation, state.generation, 0];
        state.running = DUPLEX_MASK;
        state.pair_case_a();
        assert_eq!(state.snapshot().stop_mask, 0, "paired directions keep running");
        // Running apart: both restart.
        state.owners[1] = state.generation + 100;
        state.pair_case_a();
        assert_eq!(state.snapshot().stop_mask, DUPLEX_MASK);
        // With AEC off the generations do not matter: nothing restarts.
        let mut off = State {
            echo_cancellation: false,
            owners: [3, 4, 0],
            running: DUPLEX_MASK,
            ..State::default()
        };
        off.pair_case_a();
        assert_eq!(off.snapshot().stop_mask, 0);
    }

    /// iOS (user 2026-11-12): an enabled microphone starts its native capture
    /// without a receiver; Android waits for the demand.
    #[test]
    fn ios_captures_an_enabled_microphone_without_demand() {
        let now = Instant::now();
        let microphone = MobileMediaKind::Microphone;
        let mut android = State {
            permission: MobileMicrophonePermission::Granted,
            ..State::default()
        };
        enable(&mut android, microphone, now);
        assert_eq!(android.snapshot().start_mask & microphone.bit(), 0);

        let mut ios = State {
            permission: MobileMicrophonePermission::Granted,
            capture_while_enabled: true,
            ..State::default()
        };
        assert_eq!(ios.snapshot().start_mask, 0, "off: nothing captures");
        enable(&mut ios, microphone, now);
        assert_eq!(ios.snapshot().start_mask, microphone.bit());
        ios.owners[1] = ios.generation;
        ios.running = microphone.bit();
        assert_eq!(ios.snapshot().start_mask, 0, "owned: no second start");
        // Capturing without a receiver is not sending: peers still see the
        // microphone waiting for a demand.
        let view = ios.direction_view(microphone, true).unwrap();
        assert_eq!(view.phase(), Ok(dto::EndpointPhase::Starting));
        assert_eq!(view.reason(), Ok(dto::EndpointCloseReason::AwaitingDemand));
        ios.microphone_ready = true;
        let view = ios.direction_view(microphone, true).unwrap();
        assert_eq!(view.phase(), Ok(dto::EndpointPhase::Running));
        // Turning it off stops the capture.
        ios.close(microphone, CloseReason::Manual, now);
        let snapshot = ios.snapshot();
        assert_eq!(snapshot.enabled_mask, 0);
        assert_eq!(snapshot.stop_mask, microphone.bit());
        assert_eq!(snapshot.start_mask, 0);
    }

    #[test]
    fn forwarding_keys_release_old_exit_on_stop_route_or_output_change() {
        let local = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        };
        let make = |target: u8, generation: u64| {
            let peer = Boot {
                device_id: DeviceId([target; 32]),
                nonce: [target; 16],
            };
            let output = dto::ReceiverOutputBinding::from_raw(
                [
                    (1, boot(peer).write()),
                    (2, generation.write()),
                    (3, v2::RawValue::from("default")),
                ]
                .into(),
            )
            .unwrap();
            let version = oink_audio::route::Version {
                stamp: 1,
                author: peer.device_id,
            }
            .to_wire();
            let session = dto::AudioSessionContext::from_raw(
                [
                    (1, boot(local).write()),
                    (2, 1u64.write()),
                    (3, output.write()),
                    (4, version.write()),
                ]
                .into(),
            )
            .unwrap();
            oink_routing::input::VolumeRoute::from_session(Arc::new(session)).unwrap()
        };
        let original = make(2, 1);
        for (current, forwarding) in [
            (Some(original.clone()), false),
            (Some(make(3, 1)), true),
            (Some(make(2, 2)), true),
            (None, true),
        ] {
            let mut held = HashMap::from([(0xe9, (original.clone(), 11)), (0xe2, (original.clone(), 12))]);
            let released = retire_volume_presses(&mut held, current.as_ref(), forwarding);
            assert_eq!(released.len(), 2);
            assert!(held.is_empty(), "late native ups cannot release a replacement press");
            assert!(
                released.iter().all(|(_, route, _)| *route == original),
                "ups retain the old exit context"
            );
        }
        let mut held = HashMap::from([(0xea, (original.clone(), 13))]);
        assert!(retire_volume_presses(&mut held, Some(&original), true).is_empty());
        assert_eq!(held.len(), 1);
    }

    #[test]
    fn exit_switch_keeps_native_forward_owner_until_real_stop() {
        let mut state = State::default();
        let now = Instant::now();
        enable(&mut state, MobileMediaKind::Forward, now);
        let generation = state.generation;
        state.owners[2] = generation;
        state.running = 4;
        // New usable exit, but Prepared is still in flight. Repeat to cover
        // slow and consecutive switches without a grace-period timeout.
        for _ in 0..3 {
            state.update_forward_readiness(true, false);
            assert_eq!(state.owners[2], generation);
            assert_eq!(state.running, 4);
            assert_eq!(state.stopping, 0, "no native stop/restore/pause request");
            assert_eq!(state.generation, generation);
            state.update_forward_readiness(true, true);
        }
        state.close(MobileMediaKind::Forward, CloseReason::Manual, now);
        assert_eq!(state.running & 4, 0);
        assert_eq!(state.stopping & 4, 4, "real stop retires native capture");
    }

    #[test]
    fn unavailable_exit_still_retires_native_forward_owner() {
        let mut state = State::default();
        let now = Instant::now();
        enable(&mut state, MobileMediaKind::Forward, now);
        state.owners[2] = state.generation;
        state.running = 4;
        state.update_forward_readiness(false, false);
        assert_eq!(state.running & 4, 0);
        assert_eq!(state.stopping & 4, 4);
    }

    #[test]
    fn forward_capture_waits_for_exit_and_preserves_exit_during_start() {
        let mut state = State::default();
        let now = Instant::now();
        enable(&mut state, MobileMediaKind::Forward, now);
        assert_eq!(state.snapshot().start_mask, 0, "no exit: no policy capture");
        state.forward_ready = true;
        assert_eq!(state.snapshot().start_mask, 4);
        state.forward_ready = false;
        state.speaker_exit = true;
        enable(&mut state, MobileMediaKind::Speaker, now);
        let generation = state.generation;
        state.owners[0] = generation;
        state.running = 1;
        assert_eq!(
            state.snapshot().start_mask,
            4,
            "exit stays alive until platform success"
        );
        state.close(MobileMediaKind::Forward, CloseReason::Fault, now);
        assert_eq!(state.running & 1, 1, "capture failure keeps the old exit");
        assert_eq!(state.enabled() & 1, 1);
    }
}
