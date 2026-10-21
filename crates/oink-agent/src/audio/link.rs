//! Audio's ordinary owner. The native source and sink callbacks own all M1 traffic.
use super::*;
use crate::config::{
    AgentConfig,
    owner::{ClaimRecord, GroupClaims},
};
use crate::{
    config_module::{ConfigChange, ConfigModule},
    node::Node,
    streams::{
        StreamModule,
        audio::{AudioStreams, ForwardTable, MediaReader},
    },
};
use oink_audio::claims::{Claims, Record, Resolution, Stamp};
use oink_foundation::GroupId;
use oink_network::link::{LinkLayer, ModuleEvent, ModulePort};
use oink_network::wire_identity::{boot, read_device};
mod control;
use std::sync::atomic::{AtomicBool, AtomicU64};

#[derive(Clone)]
pub(crate) struct Handle {
    links: LinkLayer,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    state: Arc<Mutex<Status>>,
    volume: Arc<oink_foundation::snapshot::Snapshot<Option<Arc<dto::AudioSessionContext>>>>,
    keep_local: Arc<AtomicBool>,
}
enum Command {
    VolumeKey(oink_foundation::input::KeyCode),
    SetOption(
        dto::Boot,
        GroupId,
        dto::LocalOption,
        u64,
        RawValue,
        tokio::sync::oneshot::Sender<Result<OwnerOptionApplied, dto::ControlFailure>>,
    ),
    SetClaim(
        dto::Boot,
        GroupId,
        bool,
        u64,
        tokio::sync::oneshot::Sender<Result<OwnerOptionApplied, dto::ControlFailure>>,
    ),
    ExitVolume(
        dto::Boot,
        GroupId,
        dto::SetExitVolume,
        tokio::sync::oneshot::Sender<Result<dto::ExitVolumeResult, dto::ControlFailure>>,
    ),
    ExitKey(
        oink_audio::volume::KeyEvent,
        tokio::sync::oneshot::Sender<Result<bool, dto::ControlFailure>>,
    ),
    Configure(crate::api::PreferencePatch<Settings>, SyncSender<Result<(), String>>),
    Refresh,
    RefreshPermission,
    PrepareUpdate(SyncSender<Result<(), String>>),
    Stop,
    #[cfg(all(test, debug_assertions))]
    HoldPeriodicReports(tokio::sync::oneshot::Sender<()>),
}
impl Handle {
    #[cfg(all(test, debug_assertions))]
    pub async fn hold_periodic_reports(&self) {
        let (reply, completed) = tokio::sync::oneshot::channel();
        self.commands.send(Command::HoldPeriodicReports(reply)).unwrap();
        completed.await.unwrap();
    }
    pub fn volume_key(&self, code: oink_foundation::input::KeyCode) {
        let _ = self.commands.send(Command::VolumeKey(code));
    }
    pub async fn set_option(
        &self,
        requester: dto::Boot,
        group: GroupId,
        option: dto::LocalOption,
        expected: u64,
        patch: RawValue,
    ) -> Result<OwnerOptionApplied, dto::ControlFailure> {
        let (reply, wait) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::SetOption(requester, group, option, expected, patch, reply))
            .map_err(|_| dto::ControlFailure::ApplyFailed)?;
        wait.await.map_err(|_| dto::ControlFailure::ApplyFailed)?
    }
    pub async fn set_claim(
        &self,
        requester: dto::Boot,
        group: GroupId,
        claimed: bool,
        expected: u64,
    ) -> Result<OwnerOptionApplied, dto::ControlFailure> {
        let (reply, wait) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::SetClaim(requester, group, claimed, expected, reply))
            .map_err(|_| dto::ControlFailure::ApplyFailed)?;
        wait.await.map_err(|_| dto::ControlFailure::ApplyFailed)?
    }
    pub async fn set_exit_volume(
        &self,
        requester: dto::Boot,
        group: GroupId,
        request: dto::SetExitVolume,
    ) -> Result<dto::ExitVolumeResult, dto::ControlFailure> {
        let (reply, wait) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::ExitVolume(requester, group, request, reply))
            .map_err(|_| dto::ControlFailure::ApplyFailed)?;
        wait.await.map_err(|_| dto::ControlFailure::ApplyFailed)?
    }
    pub fn exit_volume_key(
        &self,
        event: oink_audio::volume::KeyEvent,
    ) -> tokio::sync::oneshot::Receiver<Result<bool, dto::ControlFailure>> {
        let (reply, received) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(Command::ExitKey(event, reply));
        received
    }
    pub fn status(&self) -> Status {
        let mut status = lock(&self.state).clone();
        status.audio_activity = self.links.audio_active();
        status
    }
    pub fn configure(&self, settings: crate::api::PreferencePatch<Settings>) -> Result<(), String> {
        let (reply, wait) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Configure(settings, reply))
            .map_err(|_| "audio stopped".to_string())?;
        wait.recv_timeout(Duration::from_secs(15))
            .map_err(|_| "audio configuration timed out".to_string())?
    }
    pub fn refresh_permission(&self) {
        let _ = self.commands.send(Command::RefreshPermission);
    }
    pub fn refresh_devices(&self) -> Result<(), String> {
        self.commands
            .send(Command::Refresh)
            .map_err(|_| "audio stopped".to_string())
    }
    pub fn prepare_update(&self, deadline: Instant) -> Result<(), String> {
        let (reply, wait) = mpsc::sync_channel(1);
        self.commands
            .send(Command::PrepareUpdate(reply))
            .map_err(|_| "audio owner unavailable")?;
        wait.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "audio cleanup timed out")?
    }
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
    pub fn volume_context_snapshot(
        &self,
    ) -> Arc<oink_foundation::snapshot::Snapshot<Option<Arc<dto::AudioSessionContext>>>> {
        self.volume.clone()
    }
    pub fn keep_local_snapshot(&self) -> Arc<AtomicBool> {
        self.keep_local.clone()
    }
}

pub(crate) fn start_link_runtime(
    node: Node,
    port: ModulePort,
) -> anyhow::Result<(Handle, tokio::task::JoinHandle<()>)> {
    let links = node.links()?;
    let group = node.link_config()?;
    let settings = lock(&node.inner.config).settings.audio.clone();
    let directory = node.data_dir().join("audio");
    let native_services = node.native_services_enabled();
    let stop = node.stop_receiver();
    let state = Arc::new(Mutex::new(Status {
        settings: settings.clone(),
        ..Status::default()
    }));
    let volume = Arc::new(oink_foundation::snapshot::Snapshot::default());
    let keep_local = Arc::new(AtomicBool::new(settings.keep_local_playback));
    let (commands, receive) = tokio::sync::mpsc::unbounded_channel();
    let handle = Handle {
        links: links.clone(),
        commands,
        state: state.clone(),
        volume: volume.clone(),
        keep_local: keep_local.clone(),
    };
    let (finished, wait) = tokio::sync::oneshot::channel();
    let store = node.inner.config.clone();
    let gate = node.inner.owner_gate.clone();
    let weak = Arc::downgrade(&node.inner);
    let worker = thread::Builder::new()
        .name("oink-audio-control".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("audio control runtime");
            runtime.block_on(async move {
                let mut owner = Owner::new(
                    OwnerContext {
                        links,
                        group,
                        store,
                        gate,
                    },
                    settings,
                    directory,
                    state,
                    volume,
                    native_services,
                );
                keep_local.store(owner.settings.keep_local_playback, Ordering::Release);
                owner.keep_local = keep_local;
                let quality_node = weak.clone();
                owner.on_quality = Some(Arc::new(move |at_ms, observations, final_bucket| {
                    if let Some(inner) = quality_node.upgrade() {
                        let node = Node { inner };
                        let view = {
                            let mut history = lock(&node.inner.audio_quality);
                            let observations = observations.into_iter().map(|o| (node.device_name(o.device), o));
                            if final_bucket {
                                history.flush(at_ms, observations);
                            } else {
                                history.record(at_ms, observations);
                            }
                            history.view(at_ms)
                        };
                        node.emit(crate::api::ApiEvent::AudioQuality(view));
                    }
                }));
                let statistics_node = weak.clone();
                owner.on_statistics = Some(Arc::new(move |statistics| {
                    if let Some(inner) = statistics_node.upgrade() {
                        Node { inner }.emit(crate::api::ApiEvent::AudioStats(statistics));
                    }
                }));
                owner.on_status = Some(Arc::new(move |status| {
                    if let Some(inner) = weak.upgrade() {
                        let node = Node { inner };
                        node.emit(crate::api::ApiEvent::Audio(status));
                    }
                }));
                owner.run(port, receive, stop).await;
                drop(owner); // native capture and mute are released before reporting completion
            });
            let _ = finished.send(());
        })?;
    let task = tokio::spawn(async move {
        let _ = wait.await;
        let _ = worker.join();
    });
    Ok((handle, task))
}

type QualityCallback = Arc<dyn Fn(u64, Vec<crate::audio_quality::Observation>, bool) + Send + Sync>;

struct Receiver {
    quality_callback: Option<QualityCallback>,
    peer: DeviceId,
    stream: oink_network::link::StreamId,
    format: Negotiated,
    source: Option<oink_audio::sink::Source>,
    // The admitted M0/M1 receiver outlives its native playback/DSP port.
    idle_stats: dsp::Stats,
    reader: Arc<MediaReader>,
    backlog: std::collections::VecDeque<oink_audio::sink::Packet>,
    dropped_at_start: u64,
    observed_sleep: bool,
    observed_burst: u64,
    events: timing::ReceiveEvents,
}
impl Receiver {
    fn quality_observation(&self, now: Instant, retiring: bool) -> crate::audio_quality::Observation {
        crate::audio_quality::Observation {
            device: self.peer,
            buffer_ms: (!retiring && !self.reader.bursts.asleep(now))
                .then(|| self.source.as_ref().map_or(0., |s| s.stats().buffered_ms)),
            bytes: self.reader.payload_bytes(),
            glitches: self
                .reader
                .dropouts
                .snapshot((oink_audio::diagnostics::monotonic_us(now).max(0) / 1000) as u64),
            session: self.reader.quality_id,
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        if self.reader.payload_bytes() > 0
            && let Some(callback) = &self.quality_callback
        {
            let now = Instant::now();
            // Flush the final partial counter bucket before losing this session.
            callback(
                (oink_audio::diagnostics::monotonic_us(now).max(0) / 1000) as u64,
                vec![self.quality_observation(now, true)],
                true,
            );
        }
    }
}
struct StartingPlayback {
    result: mpsc::Receiver<Result<Box<dyn PlaybackStream>, String>>,
    at: Instant,
}
const EXIT_SWITCH_LEASE: Duration = Duration::from_millis(1500);

struct OwnerContext {
    links: LinkLayer,
    group: ConfigModule,
    store: Arc<Mutex<AgentConfig>>,
    gate: Arc<Mutex<()>>,
}
type PublishedObservations = (
    dto::CapturePermission,
    Vec<dto::AudioOutputFact>,
    dto::SpeakerObservation,
    Option<dto::ResourceFault>,
    Option<dto::ExitVolumeState>,
);

struct Owner {
    update_suspended: bool,
    update_cleanup: Option<(Option<u64>, Result<(), String>)>,
    update_activity: Option<oink_foundation::maintenance::Lease>,
    links: LinkLayer,
    group: ConfigModule,
    streams: AudioStreams,
    platform: Arc<dyn AudioPlatform>,
    discovery: DeviceDiscovery,
    permission_observer: super::discovery::PermissionObserver,
    permission_checked: Instant,
    _platform_wake: Arc<dyn Fn() + Send + Sync>,
    settings: Settings,
    state: Arc<Mutex<Status>>,
    on_status: Option<Arc<dyn Fn(Status) + Send + Sync>>,
    on_statistics: Option<Arc<dyn Fn(crate::api::AudioStatistics) + Send + Sync>>,
    published_state: Option<Status>,
    quality_at: Instant,
    on_quality: Option<QualityCallback>,
    volume: Arc<oink_foundation::snapshot::Snapshot<Option<Arc<dto::AudioSessionContext>>>>,
    keep_local: Arc<AtomicBool>,
    store: Arc<Mutex<AgentConfig>>,
    gate: Arc<Mutex<()>>,
    active_group: Option<GroupId>,
    claims: Claims,
    resolution: Resolution,
    source_session: oink_audio::session::Source,
    receive_sessions: BTreeMap<DeviceId, oink_audio::session::Receiver>,
    offers: BTreeMap<DeviceId, dto::SourceRouteOffer>,
    exit_volume: oink_audio::volume::Owner,
    bound_output: Option<String>,
    volume_published: Option<dto::ExitVolumeState>,
    published_observations: Option<PublishedObservations>,
    paused: bool,
    capture: Option<Box<dyn CaptureStream>>,
    encoded: Option<oink_audio::source::Encoded>,
    selected: Option<Negotiated>,
    needs_generation: bool,
    exit_switch_deadline: Option<Instant>,
    provisional: bool,
    verified: bool,
    playback: Option<Box<dyn PlaybackStream>>,
    playback_idle: bool,
    draining: bool,
    starting_playback: Option<StartingPlayback>,
    observed_source_sleep: bool,
    observed_source_burst: u64,
    receivers: BTreeMap<DeviceId, Receiver>,
    prepared: BTreeMap<DeviceId, Negotiated>,
    forwarding_pause: oink_audio::media_pause::ForwardingPause,
    power: Option<oink_audio::power::Monitor>,
    power_state: oink_audio::power::State,
    permission: CapturePermission,
    error: Option<String>,
    remote_problem: Option<Problem>,
    restart_reason: Option<String>,
    source_failed: bool,
    mute_cleanup_error: Option<String>,
    playback_failed: bool,
    retry_default: Option<Option<String>>,
    device_revision: Option<u64>,
    service_generation: Option<u64>,
    awaiting_output_snapshot: bool,
    output_invalidated_at: Option<Instant>,
    restart_playback: bool,
    output_latencies: Vec<OutputLatency>,
    devices: Vec<DeviceOutputs>,
    sent: Arc<AtomicU64>,
    capture_sent_base: u64,
    dropped: Arc<AtomicU64>,
    started: Instant,
    peer_states: BTreeMap<DeviceId, (DeviceAudioState, Instant)>,
    supported: BTreeSet<DeviceId>,
    report_at: Instant,
    report_pending: bool,
    reported_state: Option<DeviceAudioState>,
    cadence: Cadence,
    capture_observations: u64,
    capture_discarded_samples: u64,
    capture_last: Option<oink_audio::source::Observation>,
    capture_levels: Option<oink_audio::CaptureLevels>,
    mute_recovery: Option<oink_audio::recovery::RecoveryReport>,
    forwarding_off_reason: Option<oink_audio::ForwardingOffReason>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop_source();
        self.receivers.clear();
        self.playback = None;
        self.starting_playback = None;
        self.playback_idle = false;
        self.draining = false;
    }
}
fn startup_recovery_error(directory: &std::path::Path, error: Option<String>) -> Option<String> {
    // The machine-wide lock can belong to a different Oink data directory.
    // That alone is not evidence that THIS source left a mute behind. Inspect
    // only our private record; unreadable/locked own state remains conservative.
    // No endpoint query or device mutation occurs on this fallback path.
    error.filter(|_| {
        oink_audio::recovery::Journal::open(directory)
            .map(|journal| journal.has_pending_restore())
            .unwrap_or(true)
    })
}
impl Owner {
    fn new(
        context: OwnerContext,
        settings: Settings,
        directory: std::path::PathBuf,
        state: Arc<Mutex<Status>>,
        volume: Arc<oink_foundation::snapshot::Snapshot<Option<Arc<dto::AudioSessionContext>>>>,
        native_services: bool,
    ) -> Self {
        #[cfg(any(windows, target_os = "macos"))]
        let (recovered, recovery_error) = if !native_services {
            (None, None)
        } else {
            match oink_audio::recover(&directory) {
                Ok(report) => (Some(report), None),
                Err(error) => {
                    tracing::warn!(%error, "source mute startup recovery could not be confirmed");
                    (None, Some(error))
                }
            }
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let (recovered, recovery_error): (Option<oink_audio::recovery::RecoveryReport>, Option<String>) = (None, None);
        let recovery_error = startup_recovery_error(&directory, recovery_error);
        let platform: Arc<dyn AudioPlatform> = Arc::new(SystemAudio {
            #[cfg(any(windows, target_os = "macos"))]
            recovery_dir: directory.clone(),
        });
        #[cfg(any(test, debug_assertions))]
        let platform: Arc<dyn AudioPlatform> = if native_services {
            platform
        } else {
            // Explicit fake-platform startup retains the real worker and
            // transports without querying devices or recovering native mute.
            Arc::new(UnitTestAudio)
        };
        let mut owner = Self::with_platform(context, settings, state, volume, platform);
        owner.mute_cleanup_error = recovery_error;
        if recovered
            .as_ref()
            .is_some_and(|report| report.outcome == oink_audio::recovery::RecoveryOutcome::EndpointUnavailable)
        {
            owner.mute_cleanup_error = Some("saved muted source output unavailable; recovery journal retained".into());
        }
        owner.mute_recovery = recovered;
        if native_services {
            owner.power = oink_audio::power::Monitor::new().ok();
        }
        owner.power_state = owner.power.as_ref().map(|power| power.state()).unwrap_or_default();
        owner
    }
    fn with_platform(
        context: OwnerContext,
        settings: Settings,
        state: Arc<Mutex<Status>>,
        volume: Arc<oink_foundation::snapshot::Snapshot<Option<Arc<dto::AudioSessionContext>>>>,
        platform: Arc<dyn AudioPlatform>,
    ) -> Self {
        let OwnerContext {
            links,
            group,
            store,
            gate,
        } = context;
        let forwarding_off_reason = settings.forwarding_off_reason;
        let local_boot = boot(links.boot());
        let applied = group.applied();
        let streams = AudioStreams::new(links.clone(), group.clone());
        let wake = streams.wake();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || wake.notify_one());
        let mut discovery = DeviceDiscovery::with_wake(platform.clone(), wake.clone());
        discovery.request();
        let mut permission_observer = super::discovery::PermissionObserver::new(platform.clone(), wake.clone());
        permission_observer.request();
        let permission = if platform.permission_requires_signal() {
            CapturePermission::Unknown
        } else {
            CapturePermission::NotRequired
        };
        Self {
            update_suspended: false,
            update_cleanup: None,
            update_activity: None,
            links,
            group,
            streams,
            discovery,
            permission_observer,
            permission_checked: Instant::now(),
            _platform_wake: oink_audio::device_notifications::subscribe(wake.clone()),
            keep_local: Arc::new(AtomicBool::new(settings.keep_local_playback)),
            settings,
            state,
            on_status: None,
            on_statistics: None,
            quality_at: Instant::now(),
            on_quality: None,
            published_state: None,
            volume,
            store,
            gate,
            active_group: None,
            claims: Claims::default(),
            resolution: Resolution::Unknown,
            source_session: oink_audio::session::Source::new(local_boot.clone()),
            receive_sessions: BTreeMap::new(),
            offers: BTreeMap::new(),
            exit_volume: oink_audio::volume::Owner::new(local_boot),
            bound_output: None,
            volume_published: None,
            published_observations: None,
            paused: applied.paused,
            capture: None,
            encoded: None,
            selected: None,
            needs_generation: false,
            exit_switch_deadline: None,
            provisional: false,
            verified: false,
            playback: None,
            playback_idle: false,
            draining: false,
            starting_playback: None,
            observed_source_sleep: false,
            observed_source_burst: 0,
            receivers: BTreeMap::new(),
            prepared: BTreeMap::new(),
            forwarding_pause: {
                let pause = platform.forwarding_pause();
                pause.set_pause_allowed(false);
                pause
            },
            power: None,
            power_state: Default::default(),
            permission,
            error: None,
            remote_problem: None,
            restart_reason: None,
            source_failed: false,
            mute_cleanup_error: None,
            playback_failed: false,
            retry_default: None,
            device_revision: platform.device_revision(),
            service_generation: platform.service_generation(),
            awaiting_output_snapshot: false,
            output_invalidated_at: None,
            restart_playback: false,
            output_latencies: Vec::new(),
            platform,
            devices: Vec::new(),
            sent: Arc::new(AtomicU64::new(0)),
            capture_sent_base: 0,
            dropped: Arc::new(AtomicU64::new(0)),
            started: Instant::now(),
            peer_states: BTreeMap::new(),
            supported: BTreeSet::new(),
            report_at: Instant::now(),
            report_pending: false,
            reported_state: None,
            cadence: Cadence::default(),
            capture_observations: 0,
            capture_discarded_samples: 0,
            capture_last: None,
            capture_levels: None,
            mute_recovery: None,
            forwarding_off_reason,
        }
    }
    async fn run(
        &mut self,
        mut port: ModulePort,
        mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut changes = self.group.subscribe();
        let wake = self.streams.wake();
        self.apply();
        loop {
            // Active streams keep their 20 ms health/statistics deadline. Idle
            // control sleeps until a real notification or the one-second report.
            let maintenance = if self.capture.is_some() || self.playback.is_some() {
                Instant::now() + Duration::from_millis(20)
            } else {
                self.report_at + Duration::from_secs(1)
            };
            let deadline = self.streams.deadline().map_or(maintenance, |at| at.min(maintenance));
            let deadline = self.exit_switch_deadline.map_or(deadline, |at| at.min(deadline));
            tokio::select! {
                _ = stop.changed() => break,
                _ = wake.notified() => {},
                _ = tokio::time::sleep_until(deadline.into()) => {},
                change = changes.next() => match change {
                    Some(ConfigChange::State { .. }) => self.apply(),
                    Some(ConfigChange::Facts { device, capabilities_changed }) => { self.streams.facts_changed(device, capabilities_changed); self.apply(); self.prepare_peer(device); },
                    Some(_) => {}, None => break,
                },
                event = port.next() => match event {
                    Some(ModuleEvent::LinkReady { link, peer }) => {
                        self.streams.link_ready(link, peer);
                        self.streams.enable_state_reports(peer);
                        self.prepare_peer(peer);
                    },
                    Some(ModuleEvent::LinkLost { link, peer, reason }) => self.link_lost(link, peer, reason),
                    Some(ModuleEvent::Inbound(stream)) => self.streams.inbound(stream),
                    Some(ModuleEvent::InboundLow(op)) => self.streams.inbound_low(*op),
                    Some(ModuleEvent::StreamClosed { stream, reason }) => self.streams.stream_closed(stream, reason),
                    Some(ModuleEvent::PathChanged { .. } | ModuleEvent::ProtocolViolation { .. }) => {}, None => break,
                },
                command = commands.recv() => match command {
                    Some(Command::Stop) | None => break,
                    Some(Command::PrepareUpdate(reply)) => { let result = self.suspend_for_update(); let _ = reply.send(result); },
                    Some(Command::RefreshPermission) => self.permission_observer.request(),
                    Some(Command::Refresh) => { self.platform.invalidate_devices(); self.discovery.changed(); },
                    Some(Command::Configure(settings, reply)) => self.configure(settings, reply),
                    Some(Command::VolumeKey(code)) => self.volume_key(code),
                    Some(Command::SetOption(requester, group, option, expected, patch, reply)) => { let result = self.set_option(&requester, group, option, expected, patch); let _ = reply.send(result); },
                    Some(Command::SetClaim(requester, group, claimed, expected, reply)) => { let result = self.set_claim(&requester, group, claimed, expected); let _ = reply.send(result); },
                    Some(Command::ExitVolume(requester, group, request, reply)) => { let result = self.set_exit_volume(&requester, group, request); let _ = reply.send(result); },
                    Some(Command::ExitKey(event, reply)) => { let result = self.exit_volume_key(event); let _ = reply.send(result); },
                    #[cfg(all(test, debug_assertions))]
                    Some(Command::HoldPeriodicReports(reply)) => {
                        // The process watchdog expires long before this epoch.
                        // Tests can observe causal publication without a clock tick.
                        self.report_at = Instant::now() + Duration::from_secs(3600);
                        let _ = reply.send(());
                    },
                },
            }
            let messages = self.streams.poll(Instant::now());
            for (peer, stream) in self.streams.take_closed() {
                if stream.dir == oink_network::link::StreamDir::In && self.resolution.owner() == Some(peer) {
                    self.selected = None;
                }
            }
            for (peer, message) in messages {
                self.message(peer, message);
            }
            self.tick();
        }
        self.streams.set_forwarding(false, 1.0);
        self.stop_source();
    }
    fn link_lost(&mut self, link: oink_network::link::LinkId, peer: DeviceId, reason: oink_network::link::LinkLost) {
        self.streams.link_lost(link, peer, reason);
        self.receivers.remove(&peer);
        self.prepared.remove(&peer);
        self.receive_sessions.remove(&peer);
        self.offers.remove(&peer);
        self.peer_states.remove(&peer);
        self.supported.remove(&peer);
        if self.resolution.owner() == Some(peer) {
            self.selected = None;
            self.stop_source();
        }
    }
    fn release_capture(&mut self) -> Option<String> {
        let mut cleanup = None;
        self.streams.set_forwarding(false, 1.0);
        self.needs_generation |= self.capture.is_some() && !self.provisional;
        if let Some(mut capture) = self.capture.take() {
            capture.set_forwarding_pause(None);
            // Joining already happens on Drop. Inspect its lease restoration
            // result on EVERY lifecycle path before discarding the handle.
            if let Some(error) = capture.finish() {
                self.mute_cleanup_error = Some(error.clone());
                cleanup = Some(error);
            }
        }
        self.encoded = None;
        self.capture_levels = None;
        self.capture_sent_base = self.sent.load(Ordering::Relaxed);
        self.provisional = false;
        cleanup
    }
    fn stop_source(&mut self) {
        let _ = self.stop_source_checked();
    }
    fn stop_source_checked(&mut self) -> Option<String> {
        self.exit_switch_deadline = None;
        self.streams.set_forwarding(false, 1.0);
        self.forwarding_pause.stop();
        let cleanup = self.release_capture();
        self.forwarding_pause = self.platform.forwarding_pause();
        self.forwarding_pause
            .set_pause_allowed(matches!(self.resolution, Resolution::None { .. }));
        self.verified = false;
        cleanup
    }
    fn fail_source(&mut self, error: String) {
        self.stop_source();
        self.error = Some(error);
        self.source_failed = true;
    }
    fn suspend_for_update(&mut self) -> Result<(), String> {
        let generation = oink_foundation::maintenance::process().generation();
        if let Some((current, result)) = &self.update_cleanup
            && *current == generation
        {
            return result.clone();
        }
        // Historical diagnostics still prevent unsafe capture reuse, but only
        // this attempt's capture restoration can refuse an explicit Install.
        // Tick and PrepareUpdate can reach the same attempt in either order.
        self.update_suspended = true;
        self.streams.pause_changed(true);
        let result = self.stop_source_checked().map_or(Ok(()), Err);
        self.retire_sessions();
        self.update_activity = None;
        self.update_cleanup = Some((generation, result.clone()));
        self.publish(self.phase());
        result
    }
    fn tick(&mut self) {
        let gate = self.gate.clone();
        let _guard = lock(&gate);
        if oink_foundation::maintenance::process().closed() {
            let _ = self.suspend_for_update();
            return;
        }
        if self.update_suspended {
            self.update_suspended = false;
            self.update_cleanup = None;
            let _ = self.apply_locked();
            for peer in self
                .links
                .links()
                .links
                .iter()
                .map(|link| link.peer)
                .collect::<Vec<_>>()
            {
                self.prepare_peer(peer);
            }
        }
        if self.update_activity.is_none() {
            self.update_activity =
                oink_foundation::maintenance::process().enter(oink_foundation::maintenance::ActivityKind::Audio);
            if self.update_activity.is_none() {
                return;
            }
        }
        self.tick_running();
        // Keep the live-stream lease even when native playback is sleeping.
        // Explicit Install interrupts it through suspend_for_update.
        if self.capture.is_none()
            && self.playback.is_none()
            && self.starting_playback.is_none()
            && self.receivers.is_empty()
            && self.mute_cleanup_error.is_none()
        {
            self.update_activity = None;
        }
    }
    fn tick_running(&mut self) {
        self.cadence.observe(Instant::now());
        self.observe_service_restart();
        let automatic_off = self.capture.as_ref().and_then(|capture| {
            if self.settings.keep_local_playback {
                return None;
            }
            // Keep the established external-controls takeover reason. A
            // pending default notification must not reclassify that action.
            // Default changes still stop forwarding before generic fault
            // handling, which would otherwise only suspend the switch.
            if capture.local_controls_changed() {
                Some(oink_audio::ForwardingOffReason::LocalVolumeChanged)
            } else if capture.default_output_changed() {
                Some(oink_audio::ForwardingOffReason::DefaultOutputChanged)
            } else {
                None
            }
        });
        if let Some(reason) = automatic_off {
            self.mute_cleanup_error = self.capture.as_mut().and_then(|capture| capture.finish());
            self.stop_source();
            self.settings.forwarding_enabled = false;
            self.source_failed = false;
            self.error = self.mute_cleanup_error.clone();
            self.forwarding_off_reason = Some(reason);
            self.report_pending = true;
            let mut settings = self.settings.clone();
            settings.forwarding_enabled = false;
            settings.forwarding_off_reason = Some(reason);
            if let Err(reason) = self.install_settings_locked(&mut settings) {
                self.error = Some(format!("audio settings failure {}", reason.number()));
            }
        }
        if let Some(power) = &self.power {
            let state = power.state();
            if state != self.power_state {
                self.stop_source();
                self.receivers.clear();
                self.playback = None;
                self.starting_playback = None;
                self.playback_idle = false;
                self.draining = false;
                self.power_state = state;
                let _ = self.apply_locked();
                if let Some(power) = &self.power {
                    power.acknowledge(state.epoch);
                }
            }
        }
        // Authorization can be revoked while idle or forwarding. Keep this
        // prompt-free observer alive after a grant; output discovery is separate.
        if self.platform.permission_requires_signal() && self.permission_checked.elapsed() >= Duration::from_secs(3) {
            self.permission_checked = Instant::now();
            self.permission_observer.request();
        }
        if let Some(permission) = self.permission_observer.poll() {
            self.accept_permission(permission);
        }
        let revision = self.platform.device_revision();
        if revision != self.device_revision {
            self.device_revision = revision;
            self.discovery.changed();
            if let Some(previous) = &self.retry_default
                && self.settings.capture_scope == CaptureScope::DefaultOutput
                && let Some(current) = self.platform.default_output_id()
                && previous.as_ref() != Some(&current)
            {
                self.retry_default = None;
                self.source_failed = false;
                self.error = None;
            }
        }
        if let Some(snapshot) = self.discovery.poll() {
            self.accept_devices(snapshot);
        }
        let _ = self.refresh_exit_binding(false);
        let _ = self.exit_volume.refresh(false);
        self.refresh_volume_context();
        let _ = self.publish_observations();
        if self.power_state.sleeping {
            self.publish(AudioPhase::SoundSleep);
            return;
        }
        if self.resolution.owner() == Some(self.links.id()) {
            self.tick_exit();
        } else {
            self.tick_source();
        }
        self.streams.set_media_wake(self.playback.is_none() || self.draining);
        self.publish(self.phase());
        // API readers and the UI event emitted by set_audio_settings run as soon
        // as the reply arrives. Publish before acknowledging, including failures.
    }
    fn fail_playback(&mut self, error: String) {
        self.error = Some(error.clone());
        self.playback_failed = true;
        self.receivers.clear();
        self.playback = None;
        self.starting_playback = None;
        self.playback_idle = false;
        self.draining = false;
        for (peer, session) in std::mem::take(&mut self.receive_sessions) {
            let fault = dto::AudioRouteFault::from_raw(
                [
                    (1, session.prepared().session().write()),
                    (
                        3,
                        Problem::new(dto::UserFacingCode::DeviceUnavailable).to_wire().write(),
                    ),
                ]
                .into(),
            )
            .expect("typed session fault");
            let _ = self
                .streams
                .send(peer, &Message::AudioRouteFault(fault), Instant::now());
            self.streams.forget_preparation(peer);
        }
        self.prepared.clear();
    }
    fn observe_service_restart(&mut self) {
        let generation = self.platform.service_generation();
        if generation == self.service_generation {
            return;
        }
        let had_service = self.service_generation.is_some();
        self.service_generation = generation;
        // HAL can invalidate an output before publishing its restart notification.
        // Preserve the accepted one-second correlation; hotplug alone cannot retry.
        let eligible = self
            .playback
            .as_ref()
            .is_some_and(|playback| playback.error().is_none() || playback.device_invalidated())
            || self
                .output_invalidated_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
            || self.restart_playback
            || self.playback_idle;
        if had_service && self.resolution.owner() == Some(self.links.id()) {
            self.exit_volume.retire();
            self.awaiting_output_snapshot = true;
        }
        if had_service && eligible && self.resolution.owner() == Some(self.links.id()) {
            self.fail_playback("audio service restarted; waiting for selected output".into());
            self.restart_playback = true;
            self.exit_volume.retire();
        }
        self.discovery.changed();
    }
    fn accept_permission(&mut self, preflight: CapturePreflight) {
        let permission = if !self.platform.permission_requires_signal() {
            CapturePermission::NotRequired
        } else {
            match preflight {
                CapturePreflight::Authorized => CapturePermission::Authorized,
                CapturePreflight::Denied => CapturePermission::Denied,
                CapturePreflight::NotDetermined => CapturePermission::Unknown,
                CapturePreflight::Unavailable(_) => CapturePermission::Unknown,
            }
        };
        // Only a confirmed grant retires a permission failure. Device, mute and
        // transport failures keep their existing explicit recovery behavior.
        if permission == CapturePermission::Authorized
            && self.source_failed
            && self
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("capture_permission_"))
        {
            self.source_failed = false;
            self.error = None;
            self.verified = false;
        }
        if !matches!(
            permission,
            CapturePermission::Authorized | CapturePermission::NotRequired
        ) && self.capture.is_some()
        {
            // A lost/unknown authorization must not retain a muted capture or
            // reuse its previous verification. Playback never requires capture
            // authorization, so leave the exit and its receivers alone.
            self.fail_source(if permission == CapturePermission::Denied {
                "capture_permission_denied: system audio recording permission denied".into()
            } else {
                "capture_permission_required: confirm System Audio Recording in Permissions".into()
            });
        }
        self.permission = permission;
    }
    fn accept_devices(&mut self, result: Result<DeviceSnapshot, String>) {
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        if snapshot.service_generation != self.platform.service_generation() {
            return;
        }
        self.awaiting_output_snapshot = false;
        let before = self.devices.iter().find(|device| device.device == self.links.id());
        let had_snapshot = before.is_some();
        let old_default = before
            .and_then(|device| device.outputs.iter().find(|output| output.is_default))
            .map(|output| output.id.clone());
        let new_default = snapshot
            .outputs
            .iter()
            .find(|output| output.is_default)
            .map(|output| output.id.clone());
        let selected_present = self
            .bound_output
            .as_ref()
            .is_some_and(|selected| snapshot.outputs.iter().any(|output| &output.id == selected));
        // Output metadata can finish after a newer permission observation.
        // Only the permission observer/source preflight owns authorization.
        self.output_latencies = snapshot.latencies;
        self.devices = vec![DeviceOutputs {
            device: self.links.id(),
            outputs: snapshot.outputs,
            capture: snapshot.capture,
        }];
        if (self.playback.is_some() || self.playback_idle || self.starting_playback.is_some()) && !selected_present {
            self.fail_playback("selected output disconnected".into());
            self.output_invalidated_at = Some(Instant::now());
        }
        // Only a fresh snapshot after the service notification can resolve the
        // stable output UID. A failed open remains terminal until explicit retry.
        if self.restart_playback && selected_present {
            self.restart_playback = false;
            self.playback_failed = false;
        }
        if self.platform.device_revision().is_some()
            && self.platform.supports_default_following()
            && self.settings.keep_local_playback
            && had_snapshot
            && old_default != new_default
            && self.capture.is_some()
            && self.settings.capture_scope == CaptureScope::DefaultOutput
            && !self
                .capture
                .as_ref()
                .and_then(|capture| capture.info())
                .and_then(|info| info.physical_output_id)
                .is_some_and(|uid| new_default.as_deref() == Some(format!("coreaudio:{uid}").as_str()))
        {
            self.restart_reason = Some(format!(
                "default output notification: {old_default:?} -> {new_default:?}"
            ));
            // This is the same internal replacement as the native health
            // restart. Preserve the owner's logical ticket and player state;
            // only the old physical capture/queued media are retired.
            let _ = self.release_capture();
        }
    }
    fn tick_exit(&mut self) {
        if self.bound_output.is_none() || self.restart_playback || self.playback_failed {
            return;
        }
        // Initial preparation opens the selected device once, then the ordinary
        // idle drain closes it if no source is sounding. playback_idle retains
        // that validation: later Prepared handshakes never require a live stream.
        if self.playback.is_none() && !self.playback_idle && self.starting_playback.is_none() && !self.paused {
            match self.platform.playback(self.bound_output.as_deref().unwrap()) {
                Ok(playback) => {
                    self.playback = Some(playback);
                    self.error = None;
                }
                Err(error) => {
                    self.fail_playback(error);
                    return;
                }
            }
        }
        if let Some(error) = self.playback.as_ref().and_then(|playback| playback.error()) {
            if self
                .playback
                .as_ref()
                .is_some_and(|playback| playback.device_invalidated())
            {
                self.output_invalidated_at = Some(Instant::now());
            }
            self.fail_playback(error);
            return;
        }
        let peers: Vec<_> = self.links.links().links.iter().map(|link| link.peer).collect();
        for peer in peers {
            self.prepare_peer(peer);
        }
        let table = self.streams.forward_table();
        let current = table.load();
        self.receivers.retain(|peer, receiver| {
            current.from.iter().any(|source| {
                source.peer == *peer
                    && source.stream_id == receiver.format.generation
                    && source.format == receiver.format.format
                    && source.packets.follows() == receiver.stream
            })
        });
        for source in &current.from {
            self.receivers.entry(source.peer).or_insert_with(|| {
                let links = self.links.clone();
                Receiver {
                    quality_callback: self.on_quality.clone(),
                    peer: source.peer,
                    stream: source.packets.follows(),
                    format: Negotiated {
                        generation: source.stream_id,
                        format: source.format,
                    },
                    source: None,
                    idle_stats: dsp::Stats::default(),
                    reader: Arc::new(MediaReader::new(
                        table.clone(),
                        source.peer,
                        Some(Arc::new(move || links.audio_activity())),
                    )),
                    backlog: std::collections::VecDeque::new(),
                    dropped_at_start: source.packets.dropped(),
                    observed_sleep: false,
                    observed_burst: 0,
                    events: timing::ReceiveEvents::default(),
                }
            });
        }
        if self.draining {
            // The ordinary owner already holds admission while the finite tail
            // drains. A new burst can cancel shutdown on the still-open device;
            // retain its first packet before handing the queue back to RT.
            for receiver in self.receivers.values_mut() {
                if !receiver.reader.take_control() {
                    continue;
                }
                if let Some(packet) = receiver.reader.read().1 {
                    if let Some(source) = &mut receiver.source {
                        if !source.push_packet(packet) {
                            self.fail_playback("audio drain wake receiver capacity exceeded".into());
                            return;
                        }
                    } else {
                        receiver.backlog.push_back(packet);
                    }
                }
            }
        }
        if self.playback.is_none() {
            // With RT joined, this is the sole reader, including throughout native
            // open/start. Keep onset packets and their original network arrival times.
            for (peer, receiver) in &mut self.receivers {
                for _ in 0..64 {
                    let (_, packet) = receiver.reader.read();
                    let Some(packet) = packet else {
                        break;
                    };
                    if receiver.backlog.len() == 64 {
                        self.fail_playback("audio wake backlog capacity exceeded".into());
                        return;
                    }
                    receiver.backlog.push_back(packet);
                }
                if current
                    .from
                    .iter()
                    .find(|source| source.peer == *peer)
                    .is_some_and(|source| source.packets.dropped() != receiver.dropped_at_start)
                {
                    self.fail_playback("audio wake datagram queue overflow".into());
                    return;
                }
            }
            if self.starting_playback.is_none()
                && !self.paused
                && self.receivers.values().any(|receiver| !receiver.backlog.is_empty())
            {
                let platform = self.platform.clone();
                let output = self.bound_output.as_ref().unwrap().clone();
                let wake = self.streams.wake();
                let (send, result) = mpsc::sync_channel(1);
                let at = Instant::now();
                if let Err(error) = thread::Builder::new().name("oink-audio-wake".into()).spawn(move || {
                    let _ = send.send(platform.playback(&output));
                    wake.notify_one();
                }) {
                    self.fail_playback(format!("start audio wake worker: {error}"));
                    return;
                }
                self.starting_playback = Some(StartingPlayback { result, at });
            }
            if let Some(starting) = &self.starting_playback {
                match starting.result.try_recv() {
                    Ok(Ok(playback)) => {
                        let startup_ms = starting.at.elapsed().as_secs_f64() * 1000.0;
                        for (peer, receiver) in &self.receivers {
                            tracing::info!(
                                ?peer,
                                burst = receiver.reader.bursts.generation(),
                                startup_ms,
                                "audio output woke"
                            );
                        }
                        self.playback = Some(playback);
                        self.starting_playback = None;
                        self.playback_idle = false;
                    }
                    Ok(Err(error)) => {
                        self.fail_playback(error);
                        return;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.fail_playback("audio wake worker stopped".into());
                        return;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
        }
        if let Some(playback) = &mut self.playback {
            for receiver in self.receivers.values_mut() {
                if receiver.source.is_some() {
                    continue;
                }
                match playback.realtime_source(dsp_config(receiver.format.format)) {
                    Ok(Some(mut port)) => {
                        port.set_dropout_counters(receiver.reader.dropouts.clone());
                        while let Some(packet) = receiver.backlog.pop_front() {
                            if !port.push_packet(packet) {
                                self.fail_playback("audio wake receiver capacity exceeded".into());
                                return;
                            }
                        }
                        let reader = receiver.reader.clone();
                        receiver.reader.give_realtime();
                        port.set_packet_reader(Arc::new(move || reader.realtime()));
                        receiver.source = Some(port);
                    }
                    _ => {
                        self.error = Some("realtime audio receiver unavailable".into());
                    }
                }
            }
        }
        let now = Instant::now();
        for (peer, receiver) in &mut self.receivers {
            let sleeping = receiver.reader.bursts.asleep(now);
            let burst = receiver.reader.bursts.generation();
            if sleeping != receiver.observed_sleep || burst != receiver.observed_burst {
                tracing::info!(?peer, sleeping, burst, "audio sink source silence transition");
                receiver.observed_sleep = sleeping;
                receiver.observed_burst = burst;
            }
        }
        // Freeze admission before closing the output; an in-flight read must finish
        // first. If that read woke a source, leave the current receiver/device intact.
        let all_asleep = self
            .receivers
            .values()
            .all(|receiver| receiver.reader.bursts.asleep(now));
        if self.playback.is_some() && all_asleep && !self.paused {
            let controlled = self.receivers.values().all(|receiver| receiver.reader.take_control());
            if controlled
                && self
                    .receivers
                    .values()
                    .all(|receiver| receiver.reader.bursts.asleep(Instant::now()))
            {
                if !self.draining {
                    // Include the frozen-reader drain interval in startup overflow
                    // accounting; an onset queued here belongs to the next owner.
                    for source in &current.from {
                        if let Some(receiver) = self.receivers.get_mut(&source.peer) {
                            receiver.dropped_at_start = source.packets.dropped();
                        }
                    }
                    self.playback.as_ref().unwrap().begin_idle_drain();
                    self.draining = true;
                } else if self.playback.as_ref().unwrap().idle_drained() {
                    for receiver in self.receivers.values_mut() {
                        if let Some(source) = &mut receiver.source {
                            source.poll();
                            receiver.idle_stats = dsp::Stats {
                                queued_frames: 0,
                                buffered_ms: 0.0,
                                packet_queue_ms: 0.0,
                                decoded_queue_ms: 0.0,
                                ..source.stats()
                            };
                        }
                        receiver.source = None;
                    }
                    self.playback = None; // joins feeder and native stream threads, closes device
                    self.playback_idle = true;
                    self.draining = false;
                    tracing::info!("audio output sleeping; all sources silent");
                    self.streams.wake().notify_one();
                }
            } else {
                self.draining = false;
                self.playback.as_ref().unwrap().cancel_idle_drain();
                for receiver in self.receivers.values() {
                    receiver.reader.give_realtime();
                }
            }
        } else if self.draining {
            self.draining = false;
            if let Some(playback) = &self.playback {
                playback.cancel_idle_drain();
            }
            for receiver in self.receivers.values() {
                receiver.reader.give_realtime();
            }
        }
    }
    fn tick_source(&mut self) {
        if self.paused || !self.resolution.available() || !self.settings.forwarding_enabled {
            self.stop_source();
            return;
        }
        if self.exit_switch_deadline.is_some_and(|at| Instant::now() >= at) {
            self.fail_source("audio exit switch readiness timed out".into());
            return;
        }
        let switching = self.exit_switch_deadline.is_some();
        let codec_ready =
            !switching || (self.selected.is_some() && self.encoded.as_ref().is_none_or(|output| output.reconfigured()));
        let table = self.streams.forward_table();
        // Publish transport readiness before deciding whether capture can run. The
        // capture callback remains disabled during unmuted permission verification.
        self.streams.set_forwarding(
            !self.provisional && codec_ready && !self.source_failed,
            oink_audio::forward_volume::amplitude(self.settings.forwarding_volume_percent),
        );
        if switching && !table.load().to.is_empty() {
            self.exit_switch_deadline = None;
        }
        if table.load().to.is_empty() && !self.provisional && !switching {
            // M0 may be reopening independently while the link stays ready. Restore
            // local sound. Player pause remains reserved for an explicit no-exit
            // barrier; a control-stream fault preserves the selected exit.
            let _ = self.release_capture();
            return;
        }
        if self.platform.capture_cleanup_failed() {
            self.fail_source("capture cleanup failed; local mute release unconfirmed".into());
            return;
        }
        if self.source_failed {
            return;
        }
        if let Some(reason) = self.capture.as_ref().and_then(|capture| capture.restart_reason()) {
            self.restart_reason = Some(reason);
            let _ = self.release_capture();
        }
        if let Some(error) = self.capture.as_ref().and_then(|capture| capture.error()) {
            self.fail_source(error);
            return;
        }
        if self.capture.is_none() {
            if self.needs_generation {
                self.needs_generation = false;
                self.source_session.retire();
                self.selected = None;
                if let Some(peer) = self.resolution.owner() {
                    self.prepare_peer(peer);
                }
                return;
            }
            let Some(selected) = self.selected else {
                return;
            };
            let verify = false;
            if self.platform.permission_requires_signal() && !self.verified {
                match self.platform.capture_permission_preflight() {
                    CapturePreflight::Authorized => {
                        self.permission = CapturePermission::Authorized;
                    }
                    CapturePreflight::Denied => {
                        self.permission = CapturePermission::Denied;
                        self.fail_source("capture_permission_denied: system audio recording permission denied".into());
                        return;
                    }
                    CapturePreflight::NotDetermined | CapturePreflight::Unavailable(_) => {
                        // Startup/automatic source recovery must never request TCC.
                        // Grant creates the tap explicitly; a confirmed preflight
                        // then re-arms the real source through accept_permission.
                        self.fail_source(
                            "capture_permission_required: grant System Audio Recording in Permissions".into(),
                        );
                        return;
                    }
                }
            }
            let opened = oink_audio::source::channel(
                dsp_config(selected.format),
                self.settings.forwarding_volume_percent,
                verify,
            )
            .and_then(|(encoder, output)| {
                let writer = ForwardTable::source_writer(table.clone(), self.started);
                let links = self.links.clone();
                let pause = self.forwarding_pause.clone();
                let sent = self.sent.clone();
                let dropped = self.dropped.clone();
                output.set_muted(self.settings.forwarding_muted);
                output.set_packet_writer(Arc::new(move |packet| {
                    let accepted = pause
                        .submit(|| if writer(packet) { Ok(()) } else { Err(()) })
                        .is_some_and(|result| result.is_ok());
                    if accepted {
                        if !packet.silent {
                            links.audio_activity();
                        }
                        sent.fetch_add(1, Ordering::Relaxed);
                    } else {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    accepted
                }));
                self.platform
                    .capture_encoded(
                        encoder,
                        !self.settings.keep_local_playback && !verify,
                        self.settings.capture_scope,
                    )
                    .map(|capture| (capture, output))
            });
            match opened {
                Ok((capture, output)) => {
                    // Muted startup recovers the retained OLD endpoint
                    // journal before taking a new lease. Unmuted startup does
                    // not inspect it, so keep-local cannot prove old recovery.
                    if !self.settings.keep_local_playback {
                        self.mute_cleanup_error = None;
                    }
                    capture.set_forwarding_pause(Some(self.forwarding_pause.clone()));
                    self.capture = Some(capture);
                    self.encoded = Some(output);
                    self.provisional = verify;
                    self.verified = false;
                    self.streams.set_forwarding(
                        !verify,
                        oink_audio::forward_volume::amplitude(self.settings.forwarding_volume_percent),
                    );
                }
                Err(error) => {
                    if error.starts_with("capture_mute_release_unconfirmed:") {
                        self.mute_cleanup_error = Some(error.clone());
                    }
                    let denied = error.starts_with("capture_permission_denied:");
                    if denied {
                        self.permission = CapturePermission::Denied;
                    }
                    self.fail_source(error);
                    if !denied && !self.platform.capture_cleanup_failed() && self.platform.supports_default_following()
                    {
                        self.retry_default = Some(self.platform.default_output_id());
                    }
                }
            }
        }
        if let Some(output) = &self.encoded {
            let sleeping = output.sleeping();
            let burst = output.burst();
            if sleeping != self.observed_source_sleep || burst != self.observed_source_burst {
                tracing::info!(sleeping, burst, "audio source silence transition");
                self.observed_source_sleep = sleeping;
                self.observed_source_burst = burst;
            }
            if let Some(error) = output.error() {
                self.fail_source(error.into());
                return;
            }
            if output.has_signal() {
                self.permission = CapturePermission::Authorized;
                if self.provisional {
                    let _ = self.release_capture();
                    self.verified = true;
                }
            }
        }
    }
    fn publish(&mut self, phase: AudioPhase) {
        self.publish_at(phase, Instant::now());
    }

    fn publish_at(&mut self, phase: AudioPhase, now: Instant) {
        self.keep_local
            .store(self.settings.keep_local_playback, Ordering::Release);
        let cleanup_failed = self.platform.capture_cleanup_failed() || self.mute_cleanup_error.is_some();
        let phase = if cleanup_failed {
            self.error = self
                .mute_cleanup_error
                .clone()
                .or_else(|| Some("capture cleanup failed; local mute release unconfirmed".into()));
            AudioPhase::Failed
        } else {
            phase
        };
        let settings = self.settings.clone();
        for (peer, receiver) in &mut self.receivers {
            let Some(source) = &mut receiver.source else {
                continue;
            };
            source.poll();
            while let Some(outcome) = source.outcome() {
                receiver.events.packet(
                    *peer,
                    outcome.sequence,
                    outcome.network_read,
                    outcome.arrived,
                    outcome.inserted,
                    outcome.source_emit,
                    outcome.accepted,
                    outcome.late,
                );
            }
        }
        if now.saturating_duration_since(self.quality_at) >= Duration::from_millis(250) {
            self.quality_at +=
                Duration::from_millis((now.saturating_duration_since(self.quality_at).as_millis() / 250) as u64 * 250);
            if let Some(on_quality) = &self.on_quality {
                let observations = self
                    .receivers
                    .iter()
                    .filter(|(_, r)| r.reader.payload_bytes() > 0)
                    .map(|(_, r)| r.quality_observation(now, false))
                    .collect();
                on_quality(
                    (oink_audio::diagnostics::monotonic_us(now).max(0) / 1000) as u64,
                    observations,
                    false,
                );
            }
        }
        if let Some(encoded) = &mut self.encoded {
            while let Some(observation) = encoded.observation() {
                self.capture_observations += 1;
                self.capture_discarded_samples += observation.discarded_samples as u64;
                self.capture_last = Some(observation);
            }
            if let Some(levels) = encoded.levels() {
                if self
                    .capture_levels
                    .is_none_or(|last| last.window_sequence != levels.window_sequence)
                {
                    report::capture_levels(self.links.id(), levels);
                }
                self.capture_levels = Some(levels);
            }
        }
        let sources: Vec<_> = self
            .receivers
            .iter()
            .map(|(peer, receiver)| {
                (
                    *peer,
                    receiver
                        .source
                        .as_ref()
                        .map_or(receiver.idle_stats, |source| source.stats()),
                )
            })
            .collect();
        if lock(&self.state).phase != phase {
            tracing::info!(
                phase = phase.number(),
                claim_counter = self.resolution.stamp().map(|s| s.counter),
                capture_running = self.capture.is_some(),
                source_muted = self.capture.as_ref().is_some_and(|capture| capture.source_muted()),
                playback_running = self.playback.is_some(),
                playback_dormant = self.playback_idle,
                prepared_peers = self.prepared.len(),
                connected_sources = sources.len(),
                "audio lifecycle state changed"
            );
        }
        let state = DeviceAudioState {
            phase,
            problem: self.problem(),
            permission: self.permission,
            source_muted: self.capture.as_ref().is_some_and(|capture| capture.source_muted()),
            forwarding_enabled: self.settings.forwarding_enabled,
            forwarding_off_reason: self.forwarding_off_reason,
            forwarding_muted: self.settings.forwarding_muted,
            forwarding_volume_percent: self.settings.forwarding_volume_percent,
            mute_release_unconfirmed: cleanup_failed,
            sent: self.sent.load(Ordering::Relaxed),
            send_dropped: self.dropped.load(Ordering::Relaxed),
            outputs: self
                .devices
                .iter()
                .filter(|device| device.device == self.links.id())
                .flat_map(|device| device.outputs.iter())
                .filter(|output| oink_audio::validate_receive_output(Some(&output.id)).is_ok())
                .take(32)
                .map(|output| oink_audio::OutputLatency {
                    output: output.id.clone(),
                    reported_latency_ms: self
                        .output_latencies
                        .iter()
                        .find(|latency| latency.output == output.id)
                        .and_then(|latency| latency.reported_latency_ms)
                        .filter(|value| value.is_finite() && *value >= 0.0),
                })
                .collect(),
            receivers: sources
                .iter()
                .map(|(peer, stats)| ReceiverReport {
                    source: *peer,
                    received: stats.received,
                    concealed: stats.lost,
                    late: stats.late,
                    buffered_ms: stats.buffered_ms,
                })
                .collect(),
        };
        let periodic_report = now.saturating_duration_since(self.report_at) >= Duration::from_secs(1);
        if periodic_report {
            let worker_gaps = self.cadence.gaps.take();
            if self.capture.is_some() || self.playback.is_some() {
                let playback = self.playback.as_ref().and_then(|playback| playback.take_metrics());
                let capture_realtime = self.encoded.as_ref().map(|encoded| encoded.take_metrics());
                let capture_observations = std::mem::take(&mut self.capture_observations);
                let discarded_samples = std::mem::take(&mut self.capture_discarded_samples);
                let capture_last = self.capture_last.take();
                let sent = self.sent.load(Ordering::Relaxed);
                let send_dropped = self.dropped.load(Ordering::Relaxed);
                let at = oink_audio::diagnostics::monotonic_us(Instant::now());
                report::submit(move || {
                    tracing::debug!(
                        ?worker_gaps,
                        ?playback,
                        ?capture_realtime,
                        ?capture_last,
                        capture_observations,
                        discarded_samples,
                        sent,
                        send_dropped,
                        monotonic_us = at,
                        "audio pipeline timing"
                    );
                });
                for (peer, receiver) in &mut self.receivers {
                    let report_active = receiver.events.summary(*peer);
                    let peer = *peer;
                    let Some(source) = &mut receiver.source else {
                        continue;
                    };
                    let rejections = source.take_rejections();
                    let stats = source.stats();
                    if report_active
                        && stats.buffered_ms.is_finite()
                        && stats.buffered_ms >= 0.0
                        && let Some(p) = crate::telemetry::producer().as_ref()
                        && let Some(generation) = p.generation()
                    {
                        crate::telemetry::projections::distribution_value(
                            p,
                            generation,
                            oink_telemetry::Metric::AudioBuffered {
                                stage: oink_telemetry::AudioStage::Total,
                            },
                            (stats.buffered_ms * 1000.0) as u64,
                            None,
                        );
                        for latency in &self.output_latencies {
                            if let Some(ms) = latency.reported_latency_ms.filter(|ms| ms.is_finite() && *ms >= 0.0) {
                                crate::telemetry::projections::distribution_value(
                                    p,
                                    generation,
                                    oink_telemetry::Metric::AudioReportedOutputLatency,
                                    (ms * 1000.0) as u64,
                                    None,
                                );
                            }
                        }
                    }
                    let stream = receiver.stream;
                    report::submit(move || {
                        tracing::debug!(?peer, ?rejections, ?stats, monotonic_us = at, "audio receiver timing");
                        // Debug structs are intentionally excluded from bundles.
                        // Retain one scalar sample/second so a later save can
                        // locate concealment against collection/pacing phases.
                        tracing::debug!(target: "oink_audio_diagnostics", kind="audio_receiver_sample",
                            link_id=stream.link.0, stream_id=stream.tag.ordinal,
                            sample_mono_us=at, received=stats.received, late=stats.late,
                            concealed=stats.lost, empty_queue_plc_frames=stats.empty_queue_plc_frames,
                            gap_plc_frames=stats.gap_plc_frames, decode_errors=stats.decode_errors,
                            buffered_ms=stats.buffered_ms, arrival_jitter_ms=stats.arrival_jitter_ms,
                            "audio receiver counters");
                    });
                }
            }
            self.report_at = now;
        }
        let semantic_state = crate::event_delivery::device_audio_state(&state);
        if periodic_report || self.report_pending || self.reported_state.as_ref() != Some(&semantic_state) {
            for peer in self.supported.iter().copied() {
                let _ = self.streams.send(
                    peer,
                    &Message::AudioState(state.to_wire().expect("bounded audio state")),
                    Instant::now(),
                );
            }
            self.reported_state = Some(semantic_state);
            self.report_pending = false;
        }
        let mut device_states = vec![DeviceState {
            device: self.links.id(),
            state,
            age_ms: 0,
        }];
        device_states.extend(self.peer_states.iter().map(|(peer, (state, at))| DeviceState {
            device: *peer,
            state: state.clone(),
            age_ms: now.saturating_duration_since(*at).as_millis() as u64,
        }));
        let mut devices = self.devices.clone();
        for device in &mut devices {
            // Keep the cached backend/output metadata independent of consent.
            // Derive this diagnostic label from the current permission on every
            // publication, including changes that do not enumerate outputs.
            let suffix = match self.permission {
                CapturePermission::Authorized | CapturePermission::NotRequired => "",
                CapturePermission::Denied => "_permission_denied",
                CapturePermission::Unknown => "_permission_unknown",
                CapturePermission::NotRequested => "_permission_required",
            };
            device.capture.push_str(suffix);
        }
        devices.extend(
            self.group
                .members()
                .members
                .iter()
                .filter(|member| !member.is_self)
                .filter_map(|member| {
                    let facts = member.facts.as_ref()?;
                    if self.current_boot(member.device).as_ref() != facts.boot().ok().as_ref() {
                        return None;
                    }
                    let capabilities = facts.capabilities().ok()?;
                    Some(DeviceOutputs {
                        device: member.device,
                        outputs: capabilities
                            .audio_outputs()
                            .ok()?
                            .0
                            .into_iter()
                            .filter_map(|output| {
                                Some(OutputDevice {
                                    id: output.id().ok()?.0,
                                    name: output.name().ok()?.0,
                                    is_default: output.is_default().ok()?,
                                })
                            })
                            .collect(),
                        capture: format!("{:?}", capabilities.capture_permission().ok()?),
                    })
                }),
        );
        *lock(&self.state) = Status {
            resolution: self.resolution,
            audio_activity: self.links.audio_active(),
            suspension: if cleanup_failed {
                Some(oink_audio::SuspensionReason::MuteReleaseUnconfirmed)
            } else if self.resolution.owner().is_none() || !self.settings.forwarding_enabled {
                None
            } else if self.power_state.sleeping {
                Some(oink_audio::SuspensionReason::Sleeping)
            } else if self.paused {
                Some(oink_audio::SuspensionReason::GroupPaused)
            } else if self.source_failed || self.playback_failed {
                Some(oink_audio::SuspensionReason::DeviceUnavailable)
            } else if self.capture.is_some() || self.playback.is_some() || self.playback_idle {
                None
            } else {
                Some(oink_audio::SuspensionReason::Preparing)
            },
            settings,
            forwarding_off_reason: if self.settings.forwarding_enabled {
                None
            } else {
                self.forwarding_off_reason
            },
            devices,
            device_states,
            capture_permission: self.permission,
            phase,
            problem: self.problem(),
            source_muted: self.capture.as_ref().is_some_and(|capture| capture.source_muted()),
            mute_release_unconfirmed: cleanup_failed,
            mute_recovery: self.mute_recovery.clone(),
            capture_info: self.capture.as_ref().and_then(|capture| capture.info()),
            capture_levels: self.capture_levels,
            capture_restart_reason: self
                .restart_reason
                .as_ref()
                .map(|_| Problem::new(dto::UserFacingCode::DeviceUnavailable)),
            playback_info: self.playback.as_ref().and_then(|playback| playback.info()),
            sent: self.sent.load(Ordering::Relaxed),
            send_dropped: self.dropped.load(Ordering::Relaxed),
            sources,
            network_rtt_ms: self
                .resolution
                .owner()
                .and_then(|peer| self.links.links().by_peer(peer).and_then(|link| link.rtt))
                .map(|rtt| rtt.as_secs_f64() * 1000.0),
            ..Status::default()
        };
        // Install the authoritative snapshot before notifying subscribers. Local
        // edits arrive here after persistence/DSP application; peer reports after
        // one M0 hop. Quality sampling remains on its existing cadence.
        let status = lock(&self.state).clone();
        let semantic = crate::event_delivery::audio_state(&status);
        let changed = self.published_state.as_ref() != Some(&semantic);
        self.published_state = Some(semantic);
        if changed && let Some(on_status) = &self.on_status {
            on_status(status.clone());
        }
        // M0 arrivals may be staggered across peers. Only this owner's report
        // cadence publishes measurements; arrivals cannot create extra samples.
        if periodic_report && let Some(on_statistics) = &self.on_statistics {
            on_statistics((&status).into());
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests;
