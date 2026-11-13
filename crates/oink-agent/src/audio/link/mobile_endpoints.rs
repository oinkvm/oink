use super::*;
use crate::api::{ApiError, ApiEvent, ApiResult, DeviceKey};
use crate::device_cards::DecimalU64;
use crate::mobile_v2::{MobileV2Adapter, V2Identity};
use oink_foundation::session::Boot;
use oink_network::wire_identity::read_boot;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileAudioDirection {
    MicrophoneSend,
    SpeakerReceive,
    SpeakerSend,
}

impl MobileAudioDirection {
    fn wire(self) -> dto::EndpointDirectionKind {
        match self {
            Self::MicrophoneSend => dto::EndpointDirectionKind::MicrophoneSend,
            Self::SpeakerReceive => dto::EndpointDirectionKind::SpeakerReceive,
            Self::SpeakerSend => dto::EndpointDirectionKind::SpeakerSend,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileAudioTarget {
    pub boot: Boot,
    pub direction: MobileAudioDirection,
    pub generation: DecimalU64,
    pub activation_id: Option<[u8; 16]>,
}

/// A phone's endpoint runtime: support, phase and the reason it is not
/// running. Whether the phone is the exit or the sender is not here: it is
/// the global audio configuration (the device card's mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileEndpointState {
    pub supported: bool,
    /// The PC may open this endpoint: an Android phone (privileged mode or
    /// not, user 2026-11-12) on an admitted link (direct or through a relay
    /// station), while the endpoint is supported and the phone is local for
    /// that role in the global audio overview. Turning a phone back to local
    /// is the card's audio action (Clear(phone)).
    #[serde(default)]
    pub start_allowed: bool,
    pub phase: u32,
    pub reason: u32,
    /// The phone refused (or failed) this PC's last command for this
    /// direction: the `EndpointRefusal` number from `AudioEndpointResult`
    /// (M22). Cleared by a later accepted command or a new user action.
    #[serde(default)]
    pub refusal: Option<u32>,
    pub target: MobileAudioTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileAudioState {
    pub revision: DecimalU64,
    pub owner: Option<Boot>,
    pub microphone_send: MobileEndpointState,
    pub speaker_receive: MobileEndpointState,
    pub speaker_send: MobileEndpointState,
}

impl MobileAudioState {
    fn from_wire(state: dto::AudioEndpointState) -> v2::Result<Self> {
        let boot = read_boot(&state.boot()?)?;
        let endpoint = |view: dto::EndpointDirection, direction| -> v2::Result<MobileEndpointState> {
            Ok(MobileEndpointState {
                supported: view.supported()?,
                start_allowed: false,
                phase: view.phase()?.number(),
                reason: view.reason()?.number(),
                refusal: None,
                target: MobileAudioTarget {
                    boot,
                    direction,
                    generation: DecimalU64(view.generation()?),
                    // Each direction carries its own activation (M22).
                    activation_id: view.activation_id()?.map(|id| id.0),
                },
            })
        };
        Ok(Self {
            revision: DecimalU64(state.revision()?),
            owner: state.owner()?.as_ref().map(read_boot).transpose()?,
            microphone_send: endpoint(state.microphone_send()?, MobileAudioDirection::MicrophoneSend)?,
            speaker_receive: endpoint(state.speaker_receive()?, MobileAudioDirection::SpeakerReceive)?,
            speaker_send: endpoint(state.speaker_send()?, MobileAudioDirection::SpeakerSend)?,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum EndpointAction {
    Start,
    Release,
}

pub(super) type MobileAudioStates = Arc<Mutex<BTreeMap<DeviceId, MobileAudioState>>>;

/// A PC may start the endpoints of an Android phone's current Boot, in
/// privileged mode or not and also while the phone is locked (user
/// 2026-11-12): the App opens them from its running foreground service. iPhones
/// cannot start audio from the background, so they stay phone-only. The link
/// may be relayed: control and M1 datagrams cross a relay station like any
/// PC-to-PC audio.
pub(crate) fn remote_start_android(
    link: &oink_network::link::LinkEntry,
    members: &crate::config_module::MemberTable,
) -> bool {
    link.permits(oink_protocol::v2::registry::type_id::AudioEndpointRequest)
        && members.members.iter().any(|member| {
            member.device == link.peer
                && member.os.eq_ignore_ascii_case("android")
                && member.facts.as_ref().is_some_and(|facts| {
                    facts
                        .boot()
                        .ok()
                        .and_then(|boot| read_boot(&boot).ok())
                        .is_some_and(|boot| boot == link.peer_boot)
                })
        })
}

impl Handle {
    pub fn mobile_audio_state(
        &self,
        device: DeviceId,
        members: &crate::config_module::MemberTable,
    ) -> Option<MobileAudioState> {
        let links = self.links.links();
        let link = links.by_peer(device)?;
        let mut state = lock(&self.mobile_audio_states).get(&device).copied().filter(|state| {
            link.state == oink_network::link::LinkState::Ready && link.peer_boot == state.microphone_send.target.boot
        })?;
        let android = remote_start_android(link, members);
        let status = lock(&self.state);
        let holds = |resolution: oink_audio::route::Resolution| resolution.target() == Some(device);
        let roles = [holds(status.microphone_resolution), holds(status.resolution)];
        drop(status);
        for (endpoint, holder) in [&mut state.microphone_send, &mut state.speaker_receive]
            .into_iter()
            .zip(roles)
        {
            endpoint.start_allowed = android && endpoint.supported && !holder;
        }
        state.speaker_send.start_allowed = android
            && state.speaker_send.supported
            && ![
                dto::EndpointPhase::Starting.number(),
                dto::EndpointPhase::Running.number(),
            ]
            .contains(&state.speaker_send.phase);
        Some(state)
    }

    /// Success means the request was queued on the current authenticated stream.
    /// The following state update reports what the phone actually applied.
    pub fn control_mobile_audio(&self, action: EndpointAction, target: MobileAudioTarget) -> ApiResult<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let (reply, wait) = mpsc::sync_channel(1);
        self.commands
            .send(Command::MobileEndpoint(action, target, deadline, reply))
            .map_err(|_| unavailable())?;
        wait.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| unavailable())?
    }
}

fn unavailable() -> ApiError {
    ApiError::Unavailable {
        message: "audio control unavailable".into(),
    }
}

impl Owner {
    pub(super) fn record_mobile_audio_state(&mut self, peer: DeviceId, state: dto::AudioEndpointState) {
        let Ok(state) = MobileAudioState::from_wire(state) else {
            return;
        };
        let target = state.microphone_send.target.boot;
        if self.current_boot(peer) != Some(boot(target)) || peer == self.links.id() {
            return;
        }
        let mut state = state;
        let mut states = lock(&self.mobile_audio_states);
        if let Some(previous) = states.get(&peer) {
            if previous.microphone_send.target.boot == target && previous.revision.0 >= state.revision.0 {
                return;
            }
            if previous.microphone_send.target.boot == target {
                state.microphone_send.refusal = previous.microphone_send.refusal;
                state.speaker_receive.refusal = previous.speaker_receive.refusal;
                state.speaker_send.refusal = previous.speaker_send.refusal;
            }
        }
        states.insert(peer, state);
        drop(states);
        self.notify_mobile_audio_state(peer);
    }

    /// The phone's answer to one of this PC's endpoint commands (M22). Only a
    /// refusal or failure is kept, so the card can say why nothing changed.
    pub(super) fn record_mobile_audio_result(&mut self, peer: DeviceId, result: dto::AudioEndpointResult) {
        let (Ok(target), Ok(direction), Ok(outcome), Ok(reason)) = (
            result.target().and_then(|boot| read_boot(&boot)),
            result.direction(),
            result.outcome(),
            result.reason(),
        ) else {
            return;
        };
        if self.current_boot(peer) != Some(boot(target)) {
            return;
        }
        let refusal =
            matches!(outcome, dto::ControlOutcome::Refused | dto::ControlOutcome::Failed).then_some(reason.number());
        let mut states = lock(&self.mobile_audio_states);
        let Some(state) = states
            .get_mut(&peer)
            .filter(|state| state.microphone_send.target.boot == target)
        else {
            return;
        };
        let endpoint = match direction {
            dto::EndpointDirectionKind::MicrophoneSend => &mut state.microphone_send,
            dto::EndpointDirectionKind::SpeakerReceive => &mut state.speaker_receive,
            dto::EndpointDirectionKind::SpeakerSend => &mut state.speaker_send,
            _ => return,
        };
        if endpoint.refusal == refusal {
            return;
        }
        endpoint.refusal = refusal;
        drop(states);
        if let Some(reason) = refusal {
            tracing::warn!(reason, ?direction, "phone refused an audio endpoint command");
        }
        self.notify_mobile_audio_state(peer);
    }

    pub(super) fn forget_mobile_audio_state(&self, peer: DeviceId) {
        if lock(&self.mobile_audio_states).remove(&peer).is_some() {
            self.notify_mobile_audio_state(peer);
        }
    }

    fn notify_mobile_audio_state(&self, peer: DeviceId) {
        if let Some(node) = self.mobile_audio_node.upgrade() {
            Node { inner: node }.emit(ApiEvent::MobileAudioStateChanged(DeviceKey(peer)));
        }
    }

    pub(super) fn control_mobile_audio(
        &mut self,
        action: EndpointAction,
        target: MobileAudioTarget,
        deadline: Instant,
    ) -> ApiResult<()> {
        let peer = target.boot.device_id;
        let current = lock(&self.mobile_audio_states).get(&peer).copied();
        let endpoint = current.map(|state| match target.direction {
            MobileAudioDirection::MicrophoneSend => state.microphone_send,
            MobileAudioDirection::SpeakerReceive => state.speaker_receive,
            MobileAudioDirection::SpeakerSend => state.speaker_send,
        });
        let stale = || ApiError::InvalidState {
            message: "mobile audio endpoint changed".into(),
        };
        if Instant::now() >= deadline || self.current_boot(peer) != Some(boot(target.boot)) {
            return Err(stale());
        }
        let endpoint = endpoint
            .filter(|endpoint| endpoint.target == target)
            .ok_or_else(stale)?;
        // A new user action replaces the previous refusal of this direction.
        if let Some(state) = lock(&self.mobile_audio_states).get_mut(&peer) {
            match target.direction {
                MobileAudioDirection::MicrophoneSend => state.microphone_send.refusal = None,
                MobileAudioDirection::SpeakerReceive => state.speaker_receive.refusal = None,
                MobileAudioDirection::SpeakerSend => state.speaker_send.refusal = None,
            }
        }
        let local = self.links.boot();
        let request_id = crate::api::request_context::new_client();
        let direction = target.direction.wire();
        let adapter = MobileV2Adapter::new(V2Identity {
            device: self.links.id(),
            boot: local,
            group: self.group.applied().group,
        });
        let role = match target.direction {
            MobileAudioDirection::MicrophoneSend => Role::Microphone,
            MobileAudioDirection::SpeakerReceive | MobileAudioDirection::SpeakerSend => Role::Speaker,
        };
        let holder = if target.direction == MobileAudioDirection::SpeakerSend {
            self.audio.mode(peer, role) == Some(Mode::Shared)
        } else {
            self.audio.holder(role) == Some(peer)
        };
        let message = match action {
            EndpointAction::Start if endpoint.supported && !holder => {
                let links = self.links.links();
                let members = self.group.members();
                if !links
                    .by_peer(peer)
                    .is_some_and(|link| remote_start_android(link, &members))
                {
                    return Err(ApiError::Unsupported {
                        message: "remote audio start needs an Android phone".into(),
                    });
                }
                let id = v2::schema::FixedBytes(request_id);
                dto::AudioEndpointRequest::from_raw(
                    [
                        (1, id.write()),
                        (2, boot(local).write()),
                        (3, boot(target.boot).write()),
                        (4, direction.write()),
                        (5, id.write()),
                        (6, boot(local).write()),
                    ]
                    .into(),
                )
                .map(Message::AudioEndpointRequest)
                .map_err(crate::mobile_v2::AdapterError::Protocol)
            }
            EndpointAction::Release if holder && current.and_then(|state| state.owner) == Some(local) => adapter
                .release_endpoint(
                    request_id,
                    target.boot,
                    direction,
                    target.generation.0,
                    target.activation_id,
                )
                .map(|request| request.message),
            _ => return Err(stale()),
        }
        .map_err(|error| ApiError::InvalidArgument {
            message: error.to_string(),
        })?;
        self.streams
            .send(peer, &message, Instant::now())
            .map_err(|error| ApiError::Network {
                message: format!("mobile audio control failed: {error:?}"),
            })
    }
}
