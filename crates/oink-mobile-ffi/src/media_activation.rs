//! A PC opening this Android phone's speaker exit or microphone (remote audio intent).
//!
//! Any admitted PC may do it, in privileged mode or not, also while the phone is
//! locked (user 2026-11-12); the App opens the resources from its running
//! foreground service without a window.
use super::*;
use oink_network::link::{LinkId, LinkState};
use std::{collections::HashSet, sync::Weak};
use tokio::sync::mpsc;

const ACTIVATION_TTL: Duration = Duration::from_secs(15);
const MAX_REQUESTS_PER_BOOT: usize = 1024;

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum AndroidActivationProgress {
    WaitingForUnlock,
    PermissionRequired,
    Starting,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidActivationRequest {
    pub owner: MobileRemoteMediaOwner,
    pub ttl_ms: u64,
}

#[uniffi::export(callback_interface)]
pub trait AndroidControlListener: Send + Sync {
    fn activate(&self, request: AndroidActivationRequest);
    fn release(&self, owner: MobileRemoteMediaOwner);
}

enum Callback {
    Activate(AndroidActivationRequest),
    Release(MobileRemoteMediaOwner),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Pending,
    Claimed,
    Active,
}

struct Ticket {
    peer: Boot,
    link: LinkId,
    owner: MobileRemoteMediaOwner,
    kind: MobileMediaKind,
    deadline: Instant,
    stage: Stage,
    permission_required: bool,
}

#[derive(Default)]
pub(super) struct Activation {
    listener: Option<mpsc::UnboundedSender<Callback>>,
    helper: Option<[u8; 16]>,
    resources: u8,
    ticket: Option<Ticket>,
    // Never launch the same activation twice within this mobile Boot,
    // including after a failed launch, an expiry, or a direct reconnect.
    seen: HashSet<(Boot, [u8; 16])>,
    /// Final answers to accepted requests (activated, expired, declined or
    /// failed), sent to the requesting PC by the media service (pause audit 2).
    pub(super) results: Vec<(Boot, dto::Id16, dto::EndpointDirectionKind, EndpointOutcome)>,
}

impl Ticket {
    fn finish(&self, outcome: EndpointOutcome) -> (Boot, dto::Id16, dto::EndpointDirectionKind, EndpointOutcome) {
        let direction = match self.kind {
            MobileMediaKind::Speaker => dto::EndpointDirectionKind::SpeakerReceive,
            MobileMediaKind::Microphone => dto::EndpointDirectionKind::MicrophoneSend,
            MobileMediaKind::Forward => dto::EndpointDirectionKind::SpeakerSend,
        };
        let request = oink_protocol::v2::schema::FixedBytes(self.owner.request.clone().try_into().unwrap_or([0; 16]));
        (self.peer, request, direction, outcome)
    }
}

#[derive(uniffi::Object)]
pub struct AndroidControls {
    media: Weak<MobileMediaSession>,
    listener: mpsc::UnboundedSender<Callback>,
    task: tokio::task::AbortHandle,
}

impl std::fmt::Debug for AndroidControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AndroidControls").finish_non_exhaustive()
    }
}

impl AndroidControls {
    fn edit<R>(&self, edit: impl FnOnce(&MobileMediaSession, &mut State) -> R) -> Option<R> {
        let media = self.media.upgrade()?;
        let mut state = media.state.lock().ok()?;
        if state.closed
            || !state
                .activation
                .listener
                .as_ref()
                .is_some_and(|current| current.same_channel(&self.listener))
        {
            return None;
        }
        let result = edit(&media, &mut state);
        media.publish(&mut state);
        media.timer_changed.notify_one();
        Some(result)
    }
}

#[uniffi::export]
impl AndroidControls {
    pub fn set_direct_resources(&self, resources: u8) {
        self.edit(|_, state| {
            state.activation.resources = resources & AUDIO_MASK;
            if state
                .activation
                .ticket
                .as_ref()
                .is_some_and(|ticket| ticket.kind.bit() & resources == 0)
            {
                state.cancel_activation(Instant::now());
            }
        });
    }

    pub fn publish_helper(
        &self,
        incarnation: Vec<u8>,
        ready: bool,
        _read: bool,
        _write: bool,
        _display_generation: u64,
    ) {
        let running = self.edit(|_, state| {
            let Ok(incarnation) = <[u8; 16]>::try_from(incarnation) else {
                return None;
            };
            if !ready && state.activation.helper != Some(incarnation) {
                return None;
            }
            let next = ready.then_some(incarnation);
            state.activation.helper = next;
            Some(next.is_some())
        });
        // The helper running is "privileged mode on": publish it in DeviceFacts
        // (key 32) so every peer draws the crowned Android badge. Remote audio
        // does not depend on it. Outside the media lock; the facts owner has
        // its own.
        if let (Some(Some(running)), Some(media)) = (running, self.media.upgrade())
            && let Err(error) = media.network.set_android_privileged(running)
        {
            tracing::warn!(%error, running, "android privileged fact not published");
        }
    }

    /// Recheck before opening the resources.
    pub fn is_pending(&self, owner: MobileRemoteMediaOwner) -> bool {
        self.edit(|media, state| {
            let now = Instant::now();
            state.refresh_activation(&media.network, &media.network.audio_peers(), now);
            state
                .activation
                .ticket
                .as_ref()
                .is_some_and(|ticket| ticket.owner == owner && ticket.stage == Stage::Pending)
        })
        .unwrap_or(false)
    }

    /// Reserve this activation's single attempt before starting the foreground service.
    pub fn accept(&self, request: AndroidActivationRequest) -> bool {
        self.edit(|media, state| {
            let now = Instant::now();
            state.refresh_activation(&media.network, &media.network.audio_peers(), now);
            let Some(ticket) = state
                .activation
                .ticket
                .as_mut()
                .filter(|ticket| ticket.owner == request.owner && ticket.stage == Stage::Pending)
            else {
                return false;
            };
            ticket.stage = Stage::Claimed;
            true
        })
        .unwrap_or(false)
    }

    /// Commit intent only after Android has granted permission and started the FGS.
    pub fn activate(&self, owner: MobileRemoteMediaOwner) -> bool {
        self.edit(|media, state| {
            let now = Instant::now();
            state.refresh_activation(&media.network, &media.network.audio_peers(), now);
            let Some(ticket) = state
                .activation
                .ticket
                .as_mut()
                .filter(|ticket| ticket.owner == owner && ticket.stage == Stage::Claimed)
            else {
                return false;
            };
            let kind = ticket.kind;
            if kind == MobileMediaKind::Microphone && state.permission != MobileMicrophonePermission::Granted {
                return false;
            }
            ticket.stage = Stage::Active;
            let result = ticket.finish(Ok(dto::ControlOutcome::Applied));
            state.activation.results.push(result);
            if kind == MobileMediaKind::Speaker {
                state.close(MobileMediaKind::Forward, CloseReason::Manual, now);
            }
            let previous = state.enabled();
            state.failures[kind.index()] = MobileMediaFailure::None;
            state.remote.push(owner);
            state.sync_intent(previous, CloseReason::Manual, now);
            true
        })
        .unwrap_or(false)
    }

    pub fn report_progress(&self, owner: MobileRemoteMediaOwner, progress: AndroidActivationProgress) {
        self.edit(|_, state| {
            let Some(ticket) = state
                .activation
                .ticket
                .as_mut()
                .filter(|ticket| ticket.owner == owner && ticket.stage != Stage::Active)
            else {
                return;
            };
            let permission = matches!(progress, AndroidActivationProgress::PermissionRequired);
            if ticket.permission_required != permission {
                ticket.permission_required = permission;
                state.revision += 1;
            }
        });
    }

    pub fn finish(&self, owner: MobileRemoteMediaOwner, failure: MobileMediaFailure) {
        self.edit(|_, state| {
            let Some(ticket) = state.activation.ticket.as_ref().filter(|ticket| ticket.owner == owner) else {
                return;
            };
            let kind = ticket.kind;
            if ticket.stage != Stage::Active {
                // The user turned the request down, or Android could not start it.
                let reason = match failure {
                    MobileMediaFailure::None => dto::EndpointRefusal::Declined,
                    MobileMediaFailure::PermissionRequired => dto::EndpointRefusal::PermissionRequired,
                    _ => dto::EndpointRefusal::Failed,
                };
                let result = ticket.finish(Err(reason));
                state.activation.results.push(result);
            }
            state.cancel_activation(Instant::now());
            if failure != MobileMediaFailure::None && state.enabled() & kind.bit() == 0 {
                state.failures[kind.index()] = failure;
            }
        });
    }

    pub fn shutdown(&self) {
        self.edit(|_, state| {
            state.cancel_activation(Instant::now());
            state.activation.listener = None;
            state.activation.helper = None;
            state.activation.resources = 0;
        });
        self.task.abort();
    }
}

impl Drop for AndroidControls {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl MobileMediaSession {
    pub(crate) fn bind_android_controls(
        self: &Arc<Self>,
        listener: Box<dyn AndroidControlListener>,
    ) -> Arc<AndroidControls> {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let task = self.runtime.spawn(async move {
            while let Some(callback) = receiver.recv().await {
                match callback {
                    Callback::Activate(request) => listener.activate(request),
                    Callback::Release(owner) => listener.release(owner),
                }
            }
        });
        if cfg!(target_os = "android") && self.allowed {
            let mut state = self.state.lock().expect("media state mutex");
            state.cancel_activation(Instant::now());
            state.activation.listener = Some(sender.clone());
            state.activation.helper = None;
            state.activation.resources = 0;
            self.publish(&mut state);
            self.timer_changed.notify_one();
        }
        Arc::new(AndroidControls {
            media: Arc::downgrade(self),
            listener: sender,
            task: task.abort_handle(),
        })
    }
}

/// The activation's caller is still the admitted peer on that same link,
/// direct or through a PC relay station, and the link can carry endpoint
/// control.
fn admitted_peer(network: &MobileNetwork, peer: Boot, link: LinkId) -> bool {
    network.links().by_peer(peer.device_id).is_some_and(|current| {
        current.peer_boot == peer
            && current.link == link
            && current.state == LinkState::Ready
            && current.permits(oink_protocol::v2::registry::type_id::AudioEndpointRequest)
    })
}

/// Endpoints this phone has now (platform facts), as kind bits.
fn supported(network: &MobileNetwork) -> u8 {
    let facts = network.platform_facts();
    u8::from(facts.speaker_receive) | (u8::from(facts.microphone_send) << 1) | (u8::from(facts.speaker_send) << 2)
}

impl State {
    pub(super) fn activation_deadline(&self) -> Option<Instant> {
        self.activation
            .ticket
            .as_ref()
            .filter(|ticket| ticket.stage != Stage::Active)
            .map(|ticket| ticket.deadline)
    }

    /// The remote activation that owns `kind`: an active remote owner of that
    /// direction, else a pending ticket for it. Never the other direction's.
    pub(super) fn direction_activation(&self, kind: MobileMediaKind) -> Option<dto::Id16> {
        let request = self
            .remote
            .iter()
            .find(|owner| owner.resources & kind.bit() != 0)
            .map(|owner| owner.request.clone())
            .or_else(|| {
                self.activation
                    .ticket
                    .as_ref()
                    .filter(|ticket| ticket.kind == kind)
                    .map(|ticket| ticket.owner.request.clone())
            })?;
        Some(oink_protocol::v2::schema::FixedBytes(request.try_into().ok()?))
    }

    pub(super) fn activation_owner(&self) -> Option<&MobileRemoteMediaOwner> {
        self.activation.ticket.as_ref().map(|ticket| &ticket.owner)
    }

    pub(super) fn activation_pending(&self, kind: MobileMediaKind) -> Option<bool> {
        self.activation
            .ticket
            .as_ref()
            .filter(|ticket| ticket.stage != Stage::Active && ticket.kind == kind)
            .map(|ticket| ticket.permission_required)
    }

    pub(super) fn cancel_activation(&mut self, now: Instant) {
        let Some(ticket) = self.activation.ticket.take() else {
            return;
        };
        let previous = self.enabled();
        self.remote.retain(|owner| !owner.same_request(&ticket.owner));
        self.sync_intent(previous, CloseReason::Remote, now);
        self.revision += 1;
        if let Some(listener) = &self.activation.listener {
            let _ = listener.send(Callback::Release(ticket.owner));
        }
    }

    fn expire_activation(&mut self, now: Instant) {
        if self.activation_deadline().is_some_and(|deadline| now >= deadline) {
            if let Some(ticket) = &self.activation.ticket {
                let result = ticket.finish(Err(dto::EndpointRefusal::Expired));
                self.activation.results.push(result);
            }
            self.cancel_activation(now);
        }
    }

    pub(super) fn refresh_activation(&mut self, network: &MobileNetwork, peers: &[(Boot, StreamId)], now: Instant) {
        self.expire_activation(now);
        let Some(ticket) = &self.activation.ticket else {
            return;
        };
        if self.closed
            || self.activation.resources & supported(network) & ticket.kind.bit() == 0
            || !peers
                .iter()
                .any(|(peer, route)| *peer == ticket.peer && route.link == ticket.link)
            || !admitted_peer(network, ticket.peer, ticket.link)
            || (ticket.stage == Stage::Active && !self.remote.iter().any(|owner| owner.same_request(&ticket.owner)))
        {
            // A still pending request whose PC is reachable gets its answer.
            if ticket.stage != Stage::Active && peers.iter().any(|(peer, _)| *peer == ticket.peer) {
                let result = ticket.finish(Err(dto::EndpointRefusal::Unavailable));
                self.activation.results.push(result);
            }
            self.cancel_activation(now);
        }
    }

    /// The outcome answers the request with `AudioEndpointResult` (M22).
    pub(super) fn activation_message(
        &mut self,
        network: &MobileNetwork,
        peer: Boot,
        route: StreamId,
        message: &Message,
        now: Instant,
    ) -> EndpointOutcome {
        if self.closed {
            return Err(Failure::Unavailable);
        }
        if !admitted_peer(network, peer, route.link) {
            return Err(Failure::Unsupported);
        }
        match message {
            Message::AudioEndpointRequest(request) => {
                if !network
                    .authenticated_advert(peer.device_id)
                    .is_some_and(|advert| advert.device_kind == dto::DeviceKind::Desktop)
                {
                    return Err(Failure::Unsupported);
                }
                self.request_activation(network.boot(), peer, route.link, supported(network), request, now)
            }
            Message::AudioEndpointRelease(release) => {
                self.release_activation(network.boot(), peer, route.link, release, now)
            }
            _ => Err(Failure::Invalid),
        }
    }

    fn request_activation(
        &mut self,
        local: Boot,
        peer: Boot,
        link: LinkId,
        supported: u8,
        request: &dto::AudioEndpointRequest,
        now: Instant,
    ) -> EndpointOutcome {
        // One remote activation at a time; a second direction is refused
        // explicitly instead of being dropped (M22).
        if self.activation.ticket.is_some() || !self.remote.is_empty() {
            return Err(Failure::Busy);
        }
        let Some(listener) = &self.activation.listener else {
            return Err(Failure::Unsupported);
        };
        let (Ok(caller), Ok(owner), Ok(target), Ok(request_id), Ok(Some(activation)), Ok(direction)) = (
            request.caller().and_then(|boot| read_boot(&boot)),
            request.owner().and_then(|boot| read_boot(&boot)),
            request.target().and_then(|boot| read_boot(&boot)),
            request.request_id(),
            request.activation_id(),
            request.direction(),
        ) else {
            return Err(Failure::Invalid);
        };
        if caller != peer || owner != peer || target != local {
            return Err(Failure::Stale);
        }
        if request_id != activation {
            return Err(Failure::Invalid);
        }
        let kind = match direction {
            dto::EndpointDirectionKind::SpeakerReceive => MobileMediaKind::Speaker,
            dto::EndpointDirectionKind::MicrophoneSend => MobileMediaKind::Microphone,
            dto::EndpointDirectionKind::SpeakerSend => MobileMediaKind::Forward,
            _ => return Err(Failure::Unsupported),
        };
        let resources = kind.bit();
        if supported & self.activation.resources & resources == 0 {
            return Err(Failure::Unsupported);
        }
        if self.activation.seen.len() >= MAX_REQUESTS_PER_BOOT {
            return Err(Failure::Busy);
        }
        if !self.activation.seen.insert((peer, request_id.0)) {
            return Err(Failure::Duplicate);
        }
        let owner = MobileRemoteMediaOwner {
            caller: peer.device_id.to_hex(),
            boot_nonce: peer.nonce.to_vec(),
            request: request_id.0.to_vec(),
            resources,
        };
        let request = AndroidActivationRequest {
            owner: owner.clone(),
            ttl_ms: ACTIVATION_TTL.as_millis() as u64,
        };
        if listener.send(Callback::Activate(request)).is_ok() {
            self.activation.ticket = Some(Ticket {
                peer,
                link,
                owner,
                kind,
                deadline: now + ACTIVATION_TTL,
                stage: Stage::Pending,
                permission_required: false,
            });
            self.revision += 1;
            Ok(dto::ControlOutcome::Applied)
        } else {
            Err(Failure::Failed)
        }
    }

    fn release_activation(
        &mut self,
        local: Boot,
        peer: Boot,
        link: LinkId,
        release: &dto::AudioEndpointRelease,
        now: Instant,
    ) -> EndpointOutcome {
        let Some(ticket) = &self.activation.ticket else {
            return Ok(dto::ControlOutcome::NoChange);
        };
        let (Ok(caller), Ok(target), Ok(Some(activation)), Ok(generation), Ok(direction)) = (
            release.caller().and_then(|boot| read_boot(&boot)),
            release.target().and_then(|boot| read_boot(&boot)),
            release.activation_id(),
            release.generation(),
            release.direction(),
        ) else {
            return Err(Failure::Invalid);
        };
        let expected = match ticket.kind {
            MobileMediaKind::Speaker => dto::EndpointDirectionKind::SpeakerReceive,
            MobileMediaKind::Microphone => dto::EndpointDirectionKind::MicrophoneSend,
            MobileMediaKind::Forward => dto::EndpointDirectionKind::SpeakerSend,
        };
        if caller == peer
            && ticket.peer == peer
            && ticket.link == link
            && target == local
            && ticket.owner.request == activation.0
            && direction == expected
            && generation == self.idle.snapshot(ticket.kind.direction()).generation
        {
            self.cancel_activation(now);
            Ok(dto::ControlOutcome::Applied)
        } else {
            Err(Failure::Stale)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;
    use oink_protocol::v2::schema::FixedBytes;

    fn endpoint(device: u8, nonce: u8) -> Boot {
        Boot {
            device_id: DeviceId([device; 32]),
            nonce: [nonce; 16],
        }
    }

    fn request(local: Boot, caller: Boot, owner: Boot, activation: u8) -> dto::AudioEndpointRequest {
        dto::AudioEndpointRequest::from_raw(
            [
                (1, FixedBytes([4; 16]).write()),
                (2, boot(caller).write()),
                (3, boot(local).write()),
                (4, dto::EndpointDirectionKind::SpeakerReceive.write()),
                (5, FixedBytes([activation; 16]).write()),
                (6, boot(owner).write()),
            ]
            .into(),
        )
        .unwrap()
    }

    fn release(local: Boot, caller: Boot, generation: u64) -> dto::AudioEndpointRelease {
        dto::AudioEndpointRelease::from_raw(
            [
                (1, FixedBytes([5; 16]).write()),
                (2, boot(caller).write()),
                (3, boot(local).write()),
                (4, dto::EndpointDirectionKind::SpeakerReceive.write()),
                (5, generation.write()),
                (6, FixedBytes([4; 16]).write()),
            ]
            .into(),
        )
        .unwrap()
    }

    /// An Android App bound to the activation boundary. No privileged helper:
    /// remote audio does not need one (user 2026-11-12).
    fn bound() -> (State, mpsc::UnboundedReceiver<Callback>) {
        let mut state = State::default();
        let (tx, rx) = mpsc::unbounded_channel();
        state.activation.listener = Some(tx);
        state.activation.resources = AUDIO_MASK;
        (state, rx)
    }

    #[test]
    fn activation_requires_exact_boots_fence_and_supported_resource() {
        let (mut state, mut callback) = bound();
        let local = endpoint(1, 1);
        let peer = endpoint(2, 2);
        let other = endpoint(2, 3);
        let now = Instant::now();
        let link = LinkId(1);
        let speaker = MobileMediaKind::Speaker.bit();
        for invalid in [
            request(local, other, peer, 4),
            request(local, peer, other, 4),
            request(other, peer, peer, 4),
            request(local, peer, peer, 5),
        ] {
            let _ = state.request_activation(local, peer, link, speaker, &invalid, now);
        }
        let valid = request(local, peer, peer, 4);
        let _ = state.request_activation(local, peer, link, 0, &valid, now);
        assert!(callback.try_recv().is_err());
        let _ = state.request_activation(local, peer, link, speaker, &valid, now);
        let Callback::Activate(ticket) = callback.try_recv().unwrap() else {
            panic!("activate");
        };
        assert_eq!(ticket.ttl_ms, ACTIVATION_TTL.as_millis() as u64);
        assert_eq!(state.activation_deadline(), Some(now + ACTIVATION_TTL));
        assert_eq!(state.enabled(), 0);
        state.expire_activation(now + ACTIVATION_TTL - Duration::from_millis(1));
        assert!(callback.try_recv().is_err());
        state.expire_activation(now + ACTIVATION_TTL);
        assert!(matches!(callback.try_recv().unwrap(), Callback::Release(owner) if owner == ticket.owner));
        let _ = state.request_activation(local, peer, LinkId(2), speaker, &valid, now);
        assert!(callback.try_recv().is_err(), "a replay cannot relaunch after reconnect");
    }

    #[test]
    fn release_requires_caller_link_and_generation_and_preserves_local_intent() {
        let (mut state, mut callback) = bound();
        let local = endpoint(1, 1);
        let peer = endpoint(2, 2);
        let now = Instant::now();
        let link = LinkId(1);
        let kind = MobileMediaKind::Speaker;
        state.local_enabled = kind.bit();
        state.sync_intent(0, CloseReason::Manual, now);
        let _ = state.request_activation(local, peer, link, kind.bit(), &request(local, peer, peer, 4), now);
        let Callback::Activate(activation) = callback.try_recv().unwrap() else {
            panic!("activate");
        };
        state.activation.ticket.as_mut().unwrap().stage = Stage::Active;
        state.remote.push(activation.owner);
        let generation = state.idle.snapshot(kind.direction()).generation;
        let _ = state.release_activation(local, peer, link, &release(local, peer, generation + 1), now);
        let _ = state.release_activation(local, peer, LinkId(2), &release(local, peer, generation), now);
        let _ = state.release_activation(
            local,
            endpoint(3, 3),
            link,
            &release(local, endpoint(3, 3), generation),
            now,
        );
        assert_eq!(state.remote.len(), 1);
        let _ = state.release_activation(local, peer, link, &release(local, peer, generation), now);
        assert!(state.remote.is_empty());
        assert_eq!(state.enabled(), kind.bit());
        assert_eq!(state.idle.snapshot(kind.direction()).generation, generation);
        assert!(matches!(callback.try_recv().unwrap(), Callback::Release(_)));
    }

    /// M22: requests are answered; a second direction while one activation is
    /// in progress is refused as Busy instead of silently dropped.
    #[test]
    fn requests_are_answered_with_their_outcome() {
        let (mut state, _callback) = bound();
        let local = endpoint(1, 1);
        let peer = endpoint(2, 2);
        let now = Instant::now();
        let link = LinkId(1);
        let valid = request(local, peer, peer, 4);
        assert_eq!(
            state.request_activation(local, peer, link, AUDIO_MASK, &valid, now),
            Ok(dto::ControlOutcome::Applied)
        );
        let mut microphone = request(local, peer, peer, 7).into_raw();
        microphone.insert(1, FixedBytes([7; 16]).write());
        microphone.insert(4, dto::EndpointDirectionKind::MicrophoneSend.write());
        let microphone = dto::AudioEndpointRequest::from_raw(microphone).unwrap();
        assert_eq!(
            state.request_activation(local, peer, link, AUDIO_MASK, &microphone, now),
            Err(Failure::Busy)
        );
        let (mut fresh, _callback) = bound();
        assert_eq!(
            fresh.request_activation(local, peer, link, 0, &valid, now),
            Err(Failure::Unsupported)
        );
        assert_eq!(
            fresh.request_activation(local, endpoint(2, 3), link, AUDIO_MASK, &valid, now),
            Err(Failure::Stale)
        );
        // Without an App bound (iOS, or the Android App not running) nothing opens.
        let mut unbound = State::default();
        assert_eq!(
            unbound.request_activation(local, peer, link, AUDIO_MASK, &valid, now),
            Err(Failure::Unsupported)
        );
    }

    /// Pause audit 2: an accepted request the user never confirms is answered
    /// Expired, and the same request id is refused as a duplicate.
    #[test]
    fn an_unconfirmed_activation_is_answered_expired_and_replays_are_duplicates() {
        let (mut state, _callback) = bound();
        let local = endpoint(1, 1);
        let peer = endpoint(2, 2);
        let now = Instant::now();
        let link = LinkId(1);
        let valid = request(local, peer, peer, 4);
        assert_eq!(
            state.request_activation(local, peer, link, AUDIO_MASK, &valid, now),
            Ok(dto::ControlOutcome::Applied)
        );
        assert!(
            state.activation.results.is_empty(),
            "accepted requests are answered when they settle"
        );
        state.expire_activation(now + ACTIVATION_TTL);
        let results = std::mem::take(&mut state.activation.results);
        assert_eq!(
            results,
            vec![(
                peer,
                FixedBytes([4; 16]),
                dto::EndpointDirectionKind::SpeakerReceive,
                Err(Failure::Expired)
            )]
        );
        assert_eq!(
            state.request_activation(local, peer, link, AUDIO_MASK, &valid, now),
            Err(Failure::Duplicate)
        );
    }

    /// User 2026-11-12: a PC opens the phone's endpoints in non-privileged
    /// mode too; the helper going away does not cancel a running activation.
    #[test]
    fn activation_needs_no_privileged_helper() {
        let (mut state, mut callback) = bound();
        assert_eq!(state.activation.helper, None);
        let local = endpoint(1, 1);
        let peer = endpoint(2, 2);
        let now = Instant::now();
        assert_eq!(
            state.request_activation(local, peer, LinkId(1), AUDIO_MASK, &request(local, peer, peer, 4), now),
            Ok(dto::ControlOutcome::Applied)
        );
        assert!(matches!(callback.try_recv(), Ok(Callback::Activate(_))));
        assert!(state.activation.ticket.is_some());
    }

    #[test]
    fn android_forward_request_uses_the_source_resource_without_a_helper_token() {
        let (mut state, mut callback) = bound();
        assert!(state.activation.helper.is_none());
        let local = endpoint(1, 1);
        let peer = endpoint(2, 2);
        let mut raw = request(local, peer, peer, 4).into_raw();
        raw.insert(4, dto::EndpointDirectionKind::SpeakerSend.write());
        let request = dto::AudioEndpointRequest::from_raw(raw).unwrap();
        assert_eq!(
            state.request_activation(local, peer, LinkId(1), 4, &request, Instant::now()),
            Ok(dto::ControlOutcome::Applied)
        );
        let Callback::Activate(request) = callback.try_recv().unwrap() else {
            panic!("source activation");
        };
        assert_eq!(request.owner.resources, 4);
        assert_eq!(state.activation.ticket.as_ref().unwrap().kind, MobileMediaKind::Forward);
    }
}
