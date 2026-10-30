use super::*;
mod cursor;
use crate::streams::input::{Adapter, RouterAdapter, wire};
use crate::threads::{MainCommand, inject::InjectItem, notices::Notices};
use oink_foundation::handoff_timing::{self, Event as TimingEvent, Stage as TimingStage, Token as TimingToken};
use oink_foundation::input::{CaptureMode, CaptureTime, CapturedEvent, Disposition, InjectEvent};
use oink_input::capture_loop::{CaptureLoop, Wake};
use oink_network::link::{PostError, SchedulerStatus};
use oink_routing::{Output, Router, RouterConfig, StatusEvent};
use oink_routing::{Packet as Message, control::ControlMsg, dnd::DndMsg, input::InputMsg};
use std::{
    collections::{BTreeSet, HashMap},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeDispatch {
    Queued = 0,
    Callback = 1,
    ClipCheck = 2,
}
#[derive(Default)]
struct Pair {
    first_motion_epoch: Option<u64>,
    admission: Option<wire::PeerAdmission>,
    outbound: Option<(SendStream, Option<DatagramSend>)>,
    inbound: Option<(RecvStream, Option<DatagramRecv>)>,
    opening: usize,
    attached: bool,
    releasing: bool,
    os: Option<Os>,
}
struct Motion {
    at: Instant,
    crossing: Option<(TimingToken, Instant)>,
    peer: DeviceId,
    stream: StreamId,
    sequence: u64,
    evidence: Option<MotionEvidence>,
    dx: f64,
    dy: f64,
    time: Option<CaptureTime>,
}
struct MotionEvidence {
    first: u64,
    count: u64,
    origin: Option<DeviceId>,
}
struct PendingOwnerApply {
    state: Arc<std::sync::atomic::AtomicU8>,
    safety_sequence: Option<u64>,
    emergency_sequence: u64,
    local_emergency_sequence: u64,
    reply: super::OwnerApplyReply,
}
pub(super) struct Driver {
    backlog_sequence: u64,
    backlog_check: Instant,
    resume_hotkey: Option<oink_input::capture_loop::ResumeHotkey>,
    input_safety: oink_input::session_gate::InputSafety,
    owner_acks: Vec<PendingOwnerApply>,
    capture_fault: Option<oink_protocol::v1::dto::ResourceFaultCode>,
    display_fault: Option<oink_protocol::v1::dto::ResourceFaultCode>,
    display_failure_since: Option<Instant>,
    update_hold: Option<(u64, Instant)>,
    update_committed: bool,
    local: DeviceId,
    adapter: Adapter,
    pairs: HashMap<DeviceId, Pair>,
    read_peers: Vec<DeviceId>,
    declarations: HashMap<Option<DeviceId>, super::drag::Declaration>,
    snapshots: InputSnapshots,
    layout: Arc<RouterLayout>,
    online: BTreeSet<DeviceId>,
    settings: Arc<RoutingSettings>,
    environment: Arc<Environment>,
    capture_detector: ScopedCaptureDetector,
    capture_diagnostics: crate::diagnostics::capture::State,
    clip_poll_at: Instant,
    clip_available: bool,
    clip_diagnostics: crate::diagnostics::cursor_clip::State,
    cursor: cursor::State,
    relative_capture: bool,
    queued_cursor_constraints: bool,
    keyboard_remote: bool,
    source_modes_revision: u64,
    inject: InjectHandle,
    main: crate::threads::MainSender,
    shared: Arc<Shared>,
    notices: Arc<Notices>,
    done: std::sync::mpsc::Sender<()>,
    paused: bool,
    emergency_sequence: u64,
    local_emergency_sequence: u64,
    session_locked: Option<bool>,
    capture_initialized: bool,
    overlay: Option<u64>,
    projection_until: Option<Instant>,
    overlay_escape: Option<(u64, Instant)>,
    controllable: bool,
    stopping: bool,
    start: Instant,
    now: Instant,
    budget: super::rate_budget::RateBudget,
    motion: Option<Motion>,
    scheduler: SchedulerStatus,
    read_back_at: Instant,
    deadline: Option<Instant>,
    startup_cursor: Option<oink_foundation::Point>,
    trail: super::trail::Trail,
    trail_pointer_sent: Option<(
        oink_foundation::Point,
        oink_foundation::Point,
        Option<oink_foundation::Rect>,
    )>,
    trail_suppressed_sent: Option<bool>,
    status_dirty: bool,
    application_park: Option<(
        oink_routing::application_focus::ParkRequest,
        oink_input::application_focus::Permit,
    )>,
    input_time: Option<CaptureTime>,
    input_report: Option<crate::telemetry::Stamp>,
    jitter_sources: HashMap<(DeviceId, DeviceId, bool), (oink_input::jitter::Estimator, oink_input::jitter::Estimator)>,
}
impl Driver {
    fn check_backlog(&mut self, now: Instant) {
        self.backlog_check = now + Duration::from_millis(100);
        let triggered = self
            .shared
            .backlog_requested
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            | self.inject.take_backlog_trigger();
        if self.paused {
            return;
        }
        let mut oldest = self.inject.oldest_pending().map(|at| (at, "injection"));
        let mut include = |at: Option<Instant>, queue| {
            if let Some(at) = at
                && oldest.is_none_or(|(before, _)| at < before)
            {
                oldest = Some((at, queue));
            }
        };
        include(self.motion.as_ref().map(|motion| motion.at), "capture coalescer");
        include(self.cursor.pending.as_ref().map(|p| p.at), "target cursor readback");
        for pair in self.pairs.values() {
            if let Some((stream, motion)) = &pair.outbound {
                include(stream.oldest_pending(), "capture reliable send");
                include(
                    motion.as_ref().and_then(DatagramSend::oldest_pending),
                    "capture datagram send",
                );
            }
            if let Some((stream, motion)) = &pair.inbound {
                include(stream.oldest_pending(), "reliable receive");
                include(
                    motion.as_ref().and_then(DatagramRecv::oldest_pending),
                    "datagram receive",
                );
            }
        }
        if !triggered && oldest.is_none_or(|(at, _)| now.saturating_duration_since(at) <= Duration::from_secs(5)) {
            return;
        }
        tracing::error!(
            queue = oldest.map(|(_, queue)| queue),
            age_ms = oldest.map(|(at, _)| now.saturating_duration_since(at).as_millis() as u64),
            "input backlog exceeded 5 seconds; discarding all queued input and pausing this device"
        );
        self.backlog_sequence += 1;
        self.emergency_sequence += 1; // Fence older resumes without originating a group emergency.
        self.inject.backend().set_input_paused(true);
        self.inject.discard_pending();
        self.command(InputCommand::Paused(true));
        for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
            self.fault(peer, Which::Inbound, false, StreamCode::Cancelled);
            self.fault(peer, Which::Outbound, false, StreamCode::Cancelled);
        }
        self.publish();
    }
    #[allow(clippy::too_many_arguments)] // One native owner is assembled from the frozen startup dependencies.
    pub(super) fn new(
        boot: oink_foundation::session::Boot,
        snapshots: InputSnapshots,
        inject: InjectHandle,
        main: crate::threads::MainSender,
        shared: Arc<Shared>,
        notices: Arc<Notices>,
        done: std::sync::mpsc::Sender<()>,
    ) -> Self {
        let local = boot.device_id;
        let layout = snapshots.layout.load();
        let settings = snapshots.settings.load();
        let environment = snapshots.environment.load();
        let os = if cfg!(target_os = "macos") {
            Os::MacOS
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Other
        };
        let mut config = RouterConfig::new(boot, "", os);
        config.capture_reports_key_repeat = snapshots.capture_reports_key_repeat;
        let online = BTreeSet::from([local]);
        let mut router = Router::new(config, Arc::new(layout.topology.with_online(online.clone())));
        router.set_local_input_transform(snapshots.local_input_transform.clone());
        router.enable_system_clip_recheck(os == Os::Windows);
        router.set_volume_notifications_probe(Arc::new(oink_audio::device_notifications::recent_windows_notifications));
        router.set_settings((*settings).clone());
        let native = inject.backend();
        #[cfg(debug_assertions)]
        if let Some(clock) = native.application_focus_test_clock() {
            router.set_application_focus_test_clock(clock);
        }
        router.set_application_focus_enabled(layout.application_focus_enabled, 0);
        router.set_application_focus_participation(
            boot,
            native.application_focus_supported(),
            (settings.mouse_mode, settings.keyboard_mode),
            0,
        );
        router.set_local_feel(native.feel_profile());
        let trail = super::trail::Trail::new();
        trail.duration(settings.cursor_trail_duration_ms);
        let _ = main.send(MainCommand::TrailDuration(settings.cursor_trail_duration_ms));
        router.set_trail_probe(trail.probe());
        if let Some(trace) = &inject.trace {
            router.set_diagnostics(trace.clone());
        }
        let startup_cursor = native.cursor_position().ok();
        let now = Instant::now();
        let mut budget = super::rate_budget::RateBudget::default();
        budget.reset(settings.event_frequency, now);
        let startup_hold = oink_foundation::maintenance::process().generation();
        if startup_hold.is_some() {
            router.set_input_suspended(true);
        }
        Self {
            backlog_sequence: 0,
            backlog_check: now,
            resume_hotkey: None,
            input_safety: Default::default(),
            owner_acks: Vec::new(),
            capture_fault: None,
            display_fault: None,
            display_failure_since: None,
            update_hold: startup_hold.map(|generation| (generation, now + Duration::from_secs(600))),
            update_committed: startup_hold.is_some(),
            local,
            adapter: Adapter::new(router),
            pairs: HashMap::new(),
            read_peers: Vec::with_capacity(16),
            declarations: HashMap::new(),
            snapshots,
            layout,
            online,
            settings,
            environment,
            capture_detector: Default::default(),
            capture_diagnostics: Default::default(),
            clip_poll_at: now,
            clip_available: true,
            clip_diagnostics: Default::default(),
            cursor: Default::default(),
            relative_capture: false,
            queued_cursor_constraints: false,
            keyboard_remote: false,
            source_modes_revision: 1,
            inject,
            main,
            shared,
            notices,
            done,
            paused: false,
            emergency_sequence: 0,
            local_emergency_sequence: 0,
            session_locked: None,
            capture_initialized: false,
            overlay: None,
            projection_until: None,
            overlay_escape: None,
            controllable: true,
            stopping: false,
            start: now,
            now,
            budget,
            motion: None,
            scheduler: SchedulerStatus::default(),
            read_back_at: now,
            deadline: None,
            startup_cursor,
            trail,
            trail_pointer_sent: None,
            trail_suppressed_sent: None,
            status_dirty: true,
            application_park: None,
            input_time: None,
            input_report: None,
            jitter_sources: HashMap::new(),
        }
    }
    fn micros(&self) -> u64 {
        self.now.saturating_duration_since(self.start).as_micros() as u64
    }
    fn refresh(&mut self) {
        self.adapter
            .router_mut()
            .set_touchpad_contacts(self.inject.backend().touchpad_contacts());
        let layout = self.snapshots.layout.load();
        if !Arc::ptr_eq(&layout, &self.layout) {
            let focus = self.adapter.router().focus().holder;
            // Input claims can arrive before the ordinary config owner publishes
            // reconnect availability. Repeated absence in its snapshots is not
            // a removal: recalling here would broadcast a newer local claim and
            // pull the whole group's pointer to this otherwise idle member.
            if focus != self.local && self.layout.devices.contains(&focus) && !layout.devices.contains(&focus) {
                self.adapter.router_mut().return_pointer_home();
            }
            // Periodic facts may republish an identical snapshot. Reinstalling it
            // would cancel an in-progress gap crossing and edge resistance.
            let changed = layout.devices != self.layout.devices
                || layout.layout != self.layout.layout
                || layout.owners != self.layout.owners;
            let now = self.micros();
            self.adapter
                .router_mut()
                .set_application_focus_enabled(layout.application_focus_enabled, now);
            self.layout = layout;
            self.sync_layout(changed);
        }
        let settings = self.snapshots.settings.load();
        if !Arc::ptr_eq(&settings, &self.settings) {
            if *settings != *self.settings {
                if settings.mouse_mode != self.settings.mouse_mode
                    || settings.keyboard_mode != self.settings.keyboard_mode
                {
                    self.source_modes_revision += 1;
                }
                if settings.mouse_mode != self.settings.mouse_mode {
                    self.flush_motion_for(crate::diagnostics::handoff::FlushReason::FrequencyChanged, None);
                }
                if self.budget.frequency() != settings.event_frequency {
                    self.flush_motion_for(crate::diagnostics::handoff::FlushReason::FrequencyChanged, None);
                    self.budget.reset(settings.event_frequency, self.now);
                }
                if settings.cursor_trail_duration_ms != self.settings.cursor_trail_duration_ms {
                    self.trail.duration(settings.cursor_trail_duration_ms);
                    let _ = self
                        .main
                        .send(MainCommand::TrailDuration(settings.cursor_trail_duration_ms));
                }
                let disabling_keyboard = settings.keyboard_mode == oink_foundation::input::InputMode::Disabled
                    && !self.paused
                    && !self.adapter.router().keyboard_disabled();
                let held = if disabling_keyboard {
                    self.inject.backend().held_session_keys()
                } else {
                    Vec::new()
                };
                self.adapter.router_mut().set_settings((*settings).clone());
                if disabling_keyboard {
                    self.adapter.router_mut().release_session_keys(&held);
                }
                let boot = self.adapter.router().config().boot;
                let capable = self.inject.backend().application_focus_supported();
                let now = self.micros();
                self.adapter.router_mut().set_application_focus_participation(
                    boot,
                    capable,
                    (settings.mouse_mode, settings.keyboard_mode),
                    now,
                );
            }
            self.settings = settings;
            let relative = self.adapter.router().game_mode().active
                && !self.adapter.router().has_pointer_focus()
                && !self.adapter.router().paused()
                && !self.adapter.router().mouse_local();
            if self.relative_capture != relative {
                self.relative_capture = relative;
                self.inject
                    .push_in_epoch(self.input_safety, InjectItem::SetRelativeCapture(relative));
            }
        }
        let environment = self.snapshots.environment.load();
        if !Arc::ptr_eq(&environment, &self.environment) {
            if environment.game_detected != self.environment.game_detected {
                self.adapter
                    .router_mut()
                    .set_pointer_captured(environment.game_detected);
            }
            if environment.displays_asleep && !self.environment.displays_asleep {
                self.adapter.router_mut().on_displays_asleep();
            }
            if environment.system_sleeping != self.environment.system_sleeping {
                self.adapter
                    .router_mut()
                    .set_input_suspended(self.paused || environment.system_sleeping);
                self.adapter
                    .router_mut()
                    .set_paused(self.paused || environment.system_sleeping || !self.controllable);
            }
            self.environment = environment;
        }
    }
    fn sync_layout(&mut self, force: bool) {
        let mut online = BTreeSet::from([self.local]);
        online.extend(self.pairs.iter().filter_map(|(peer, pair)| {
            (pair.attached
                && self.layout.devices.contains(peer)
                && self
                    .adapter
                    .receive_state(*peer)
                    .is_some_and(|s| s.controllable == Some(true)))
            .then_some(*peer)
        }));
        if force || online != self.online {
            let topology = self.layout.topology.with_online(online.clone());
            if let Some(trace) = &self.inject.trace {
                trace.topology(
                    &topology,
                    self.layout.group,
                    self.layout.layout.clone(),
                    self.layout.owners.clone(),
                    self.layout.version.clone(),
                    Some(online.clone()),
                );
            }
            self.adapter.router_mut().set_layout(Arc::new(topology));
            self.online = online;
        }
    }
    fn observe_display_fault(&mut self, fault: oink_protocol::v1::dto::ResourceFaultCode) {
        self.display_fault = Some(fault);
        if fault == oink_protocol::v1::dto::ResourceFaultCode::None {
            self.display_failure_since = None;
        } else {
            self.display_failure_since.get_or_insert(self.now);
        }
    }
    fn display_enumeration_fault(&self) -> Option<oink_protocol::v1::dto::ResourceFaultCode> {
        // Display reconfiguration can briefly yield an error or an empty list.
        // Preserve immediate resource facts for routing; only the local notice
        // requires three continuous seconds of failed enumeration.
        self.display_failure_since
            .filter(|since| self.now.saturating_duration_since(*since) >= Duration::from_secs(3))
            .and(self.display_fault)
    }
    fn resource_faults(&self) -> super::ResourceFaults {
        use oink_protocol::v1::dto::ResourceFaultCode;
        let native = self.inject.resource_faults();
        super::ResourceFaults {
            screen: super::faults::combine(native.screen, self.display_fault),
            mouse: super::faults::combine(native.mouse, self.capture_fault),
            keyboard: if self.environment.secure_input {
                Some(ResourceFaultCode::SecureInput)
            } else {
                self.capture_fault
            },
        }
    }
    fn owner_fence(
        &mut self,
        reply: super::OwnerApplyReply,
        failures: Arc<std::sync::atomic::AtomicU64>,
        expect_released: bool,
        safety_sequence: Option<u64>,
    ) {
        self.outputs(None);
        self.publish();
        let state = Arc::new(std::sync::atomic::AtomicU8::new(0));
        self.inject.push_in_epoch(
            self.input_safety,
            InjectItem::OwnerFence {
                state: state.clone(),
                failures,
                expect_released,
            },
        );
        self.owner_acks.push(PendingOwnerApply {
            state,
            safety_sequence,
            emergency_sequence: self.emergency_sequence,
            local_emergency_sequence: self.local_emergency_sequence,
            reply,
        });
    }
    fn reply_to_owners(&mut self) {
        self.owner_acks.retain(|pending| {
            let state = pending.state.load(Ordering::Acquire);
            if state == 0 {
                return true;
            }
            let result = match state {
                _ if pending
                    .safety_sequence
                    .is_some_and(|sequence| sequence != self.emergency_sequence) =>
                {
                    Err(oink_protocol::v1::dto::ControlFailure::StaleRevision)
                }
                2 => Err(oink_protocol::v1::dto::ControlFailure::ApplyFailed),
                _ => Ok(super::OwnerInputApplied {
                    cleanup_pending: state == 3,
                    emergency_sequence: pending.emergency_sequence,
                    local_emergency_sequence: pending.local_emergency_sequence,
                }),
            };
            let _ = pending.reply.try_send(result);
            false
        });
    }
    fn publish(&mut self) {
        let status = self.shared.status.load();
        let router = self.adapter.router();
        if !self.status_dirty
            // Remote commits and timer transitions can change application focus
            // without a native command or an ordinary routing Status event.
            && status.application_focus.as_ref() == router.application_focus()
            && status.update_held_input == router.update_input_held()
            && status.update_remote_control == router.update_remote_control(self.micros())
            && status.update_locked == (self.session_locked != Some(false) || self.environment.secure_input)
            && status.update_suspended == (self.environment.system_sleeping || self.environment.displays_asleep)
            && status.pointer_focus == router.has_pointer_focus()
            && status.dnd_active == (router.dnd_state_name() != "idle")
            && status.dnd_host_pending == router.dnd_host_pending()
            && status.trail_pinned == router.trail_pinned()
            && status.paused == router.paused()
            && status.emergency_sequence == self.emergency_sequence
            && status.injection_detached == self.inject.detached()
            && status.locked_device == router.locked_device()
            && status.screen_local_input == router.screen_local_input()
            && status.source_modes_revision == self.source_modes_revision
            && status.mouse_mode == router.config().settings.mouse_mode
            && status.keyboard_mode == router.config().settings.keyboard_mode
            && status.resource_faults == self.resource_faults()
            && status.display_enumeration_fault == self.display_enumeration_fault()
        {
            return;
        }
        self.status_dirty = false;
        let opening = self.adapter.opening_state(self.controllable).to_vec();
        if *self.shared.opening.load() != opening {
            self.shared.opening.publish(opening);
        }
        let mut peers: Vec<_> = self
            .pairs
            .iter()
            .map(|(peer, pair)| PeerInput {
                peer: *peer,
                inbound: pair.inbound.as_ref().map(|p| p.0.id()),
                outbound: pair.outbound.as_ref().map(|p| p.0.id()),
                attached: pair.attached,
                opening_received: pair.opening,
                releasing: pair.releasing,
                online: pair.attached
                    && self
                        .adapter
                        .receive_state(*peer)
                        .is_some_and(|s| s.controllable == Some(true)),
                touchpad_contacts: self.adapter.receive_state(*peer).and_then(|s| s.touchpad_contacts),
                feel: self.adapter.receive_state(*peer).and_then(|s| s.feel.clone()),
            })
            .collect();
        peers.sort_by_key(|peer| peer.peer);
        let status = InputStatus {
            application_focus: router.application_focus().cloned(),
            update_held_input: router.update_input_held(),
            update_remote_control: router.update_remote_control(self.micros()),
            update_locked: self.session_locked != Some(false) || self.environment.secure_input,
            update_suspended: self.environment.system_sleeping || self.environment.displays_asleep,
            focus: Some(self.adapter.router().focus().holder),
            focus_info: Some(self.adapter.router().focus()),
            group_focus: self.adapter.router().group_focus_status(),
            game_mode: self.adapter.router().game_mode().active,
            keyboard_disabled: self.adapter.router().keyboard_disabled(),
            caps_lock_switches_input_source: self.inject.backend().caps_lock_switches_input_source(),
            resource_faults: self.resource_faults(),
            display_enumeration_fault: self.display_enumeration_fault(),
            source_modes_revision: self.source_modes_revision,
            mouse_mode: self.adapter.router().config().settings.mouse_mode,
            keyboard_mode: self.adapter.router().config().settings.keyboard_mode,
            screen_local_input: self.adapter.router().screen_local_input(),
            game_status: Some(self.adapter.router().game_mode()),
            pointer_focus: self.adapter.router().has_pointer_focus(),
            dnd_active: self.adapter.router().dnd_state_name() != "idle",
            dnd_host_pending: self.adapter.router().dnd_host_pending(),
            trail_pinned: self.adapter.router().trail_pinned(),
            paused: self.adapter.router().paused(),
            emergency_sequence: self.emergency_sequence,
            local_emergency_sequence: self.local_emergency_sequence,
            locked_device: self.adapter.router().locked_device(),
            backlog_sequence: self.backlog_sequence,
            injection_detached: self.inject.detached(),
            helper_priority: self.inject.backend().helper_priority(),
            peers,
        };
        if *self.shared.status.load() != status {
            self.shared.status.publish(status);
            self.shared.status_changed.notify_one();
            let _ = self.main.send(MainCommand::InputStatusChanged);
        }
    }
    fn detach(&mut self, peer: DeviceId) {
        // Relayed origins share an upstream stream. Reset just that stream's
        // baselines; an unrelated peer disappearing must not hide a live stall.
        self.jitter_sources.retain(|(upstream, _, _), _| *upstream != peer);
        let Some(pair) = self.pairs.get_mut(&peer) else {
            return;
        };
        if !pair.attached {
            return;
        }
        pair.attached = false;
        pair.releasing = true;
        if let Some(trace) = &self.inject.trace {
            trace.pair(peer, false);
        }
        self.status_dirty = true;
        self.sync_layout(false);
        self.publish(); // Clear online before the router emits Cancel and the native releases.
        self.declarations.remove(&Some(peer));
        self.adapter.detach(peer);
        self.outputs(Some(peer));
        self.inject
            .push_in_epoch(self.input_safety, InjectItem::ReleaseAll { peer });
    }
    fn attach(&mut self, peer: DeviceId) {
        if self.update_hold.is_some() || self.inject.suspended() {
            return;
        }
        let pair = &self.pairs[&peer];
        if self.paused || pair.releasing || pair.attached || pair.outbound.is_none() || pair.inbound.is_none() {
            return;
        }
        let Some(admission) = pair.admission.as_ref() else {
            return;
        };
        let Some(state) = self
            .adapter
            .receive_state(peer)
            .filter(|s| s.opening_read(admission))
            .cloned()
        else {
            return;
        };
        self.adapter.attach(admission, &state, pair.os.unwrap_or(Os::Other));
        if self.adapter.attached(peer) {
            self.pairs.get_mut(&peer).unwrap().attached = true;
            if let Some(trace) = &self.inject.trace {
                trace.pair(peer, true);
            }
            self.status_dirty = true;
            self.sync_layout(false);
            self.outputs(None);
        }
    }
    fn fault(&mut self, peer: DeviceId, which: Which, full: bool, code: StreamCode) {
        self.status_dirty = true;
        if code == StreamCode::Cancelled
            && let Some(pair) = self.pairs.get(&peer)
        {
            // The safety fence deliberately ends a native session. Mark the
            // handle before dropping it: the network owner may see the closed
            // queue before InputStreams receives our notice. Both event orders
            // must tell the peer this was cancellation, not a failed opening.
            match which {
                Which::Outbound => {
                    if let Some((stream, _)) = &pair.outbound {
                        stream.cancel();
                    }
                }
                Which::Inbound => {
                    if let Some((stream, _)) = &pair.inbound {
                        stream.cancel();
                    }
                }
            }
        }
        let stream = match which {
            Which::Outbound => self
                .pairs
                .get_mut(&peer)
                .and_then(|p| p.outbound.take())
                .map(|p| p.0.id()),
            Which::Inbound => self
                .pairs
                .get_mut(&peer)
                .and_then(|p| p.inbound.take())
                .map(|p| p.0.id()),
        };
        let Some(stream) = stream else {
            return;
        };
        if which == Which::Outbound && self.motion.as_ref().is_some_and(|m| m.peer == peer) {
            self.motion = None;
        }
        self.detach(peer);
        if which == Which::Inbound {
            self.adapter.clear_received(peer);
        }
        self.notices.post(InputNotice::Fault {
            peer,
            which,
            full,
            code,
            stream,
        });
    }
    fn queued(&mut self, queued: super::drag::Queued) {
        if self.stopping {
            return;
        }
        if matches!(
            queued.command,
            InputCommand::DndEvent(oink_routing::DndEvent::SourceExported { .. })
        ) {
            self.declarations.remove(&None);
        }
        let owner = match &queued.command {
            InputCommand::DndItems { peer, stream, .. } => {
                if !self.current_inbound(*peer, *stream) {
                    return;
                }
                Some(*peer)
            }
            _ => None,
        };
        match queued.declaration {
            Ok(Some(declaration)) => {
                self.declarations.insert(owner, declaration);
            }
            Ok(None) => {}
            Err(_) => {
                match &queued.command {
                    InputCommand::DndItems { peer, notice, .. } => {
                        self.adapter
                            .router_mut()
                            .on_dnd_attachment_refused(*peer, notice.key, notice.revision);
                    }
                    _ => self.adapter.router_mut().cancel_dnd(),
                }
                self.outputs(owner);
                return;
            }
        }
        self.command(queued.command);
    }
    fn command(&mut self, command: InputCommand) {
        if self.stopping {
            return;
        }
        self.status_dirty |= !matches!(
            command,
            InputCommand::PointerSample { .. }
                | InputCommand::EdgeResistance(_)
                | InputCommand::VolumeExit(..)
                | InputCommand::EventFrequency(_)
                | InputCommand::SendMessage { .. }
        );
        match command {
            InputCommand::ApplicationFocusObserved { claim, observation } => {
                let now = self.micros();
                self.adapter
                    .router_mut()
                    .application_focus_observed(&claim, observation, now);
            }
            InputCommand::ApplicationFocusApplied { request, result } => {
                let now = self.micros();
                self.adapter
                    .router_mut()
                    .application_focus_park_completed(&request, result, now);
            }
            InputCommand::ReceiveEmergency { reply } => {
                let failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
                self.inject
                    .push_in_epoch(self.input_safety, InjectItem::OwnerBegin(failures.clone()));
                // Received C0 events invalidate older resumes without originating a new group event.
                if !self.adapter.router().keyboard_disabled() && !self.adapter.router().mouse_disabled() {
                    self.emergency_sequence = self
                        .emergency_sequence
                        .checked_add(1)
                        .expect("emergency sequence exhausted");
                }
                self.adapter.router_mut().release_input_disables();
                self.command(InputCommand::Paused(true));
                self.owner_fence(reply, failures, true, None);
            }
            InputCommand::TriggerEmergency { reply } => {
                let failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
                self.inject
                    .push_in_epoch(self.input_safety, InjectItem::OwnerBegin(failures.clone()));
                self.adapter.trigger_emergency();
                self.owner_fence(reply, failures, true, None);
            }
            InputCommand::ApplyOwnerPreferences {
                expected_emergency_sequence,
                mut routing,
                input_transform,
                accept_contacts,
                reply,
            } => {
                let disabling =
                    [routing.keyboard_mode, routing.mouse_mode].contains(&oink_foundation::input::InputMode::Disabled);
                if disabling && expected_emergency_sequence != self.emergency_sequence {
                    let _ = reply.try_send(Err(oink_protocol::v1::dto::ControlFailure::StaleRevision));
                    return;
                }
                let failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
                self.inject
                    .push_in_epoch(self.input_safety, InjectItem::OwnerBegin(failures.clone()));
                routing.accept_contacts = accept_contacts;
                self.snapshots.settings.publish(routing);
                self.refresh();
                self.adapter.router_mut().set_local_input_transform(input_transform);
                self.owner_fence(reply, failures, false, disabling.then_some(expected_emergency_sequence));
            }
            InputCommand::ApplyOwnerPause {
                paused,
                expected_emergency_sequence,
                reply,
            } => {
                if !paused && expected_emergency_sequence != self.emergency_sequence {
                    let _ = reply.try_send(Err(oink_protocol::v1::dto::ControlFailure::StaleRevision));
                    return;
                }
                let failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
                self.inject
                    .push_in_epoch(self.input_safety, InjectItem::OwnerBegin(failures.clone()));
                self.command(InputCommand::Paused(paused));
                self.owner_fence(
                    reply,
                    failures,
                    paused,
                    (!paused).then_some(expected_emergency_sequence),
                );
            }
            InputCommand::PrepareUpdate { generation, reply } => {
                use oink_update::barrier::Blocker;
                // Explicit Install interrupts held/remote input, native drag,
                // locked/unknown sessions and sleep; those are not idle gates.
                // The injection fence below still proves releases were applied.
                if self.update_hold.is_some_and(|(old, _)| old != generation) {
                    let _ = reply.send(Err(vec![Blocker::OwnerMissing]));
                    return;
                }
                self.update_hold = Some((generation, Instant::now() + Duration::from_secs(30)));
                // Reuse the ordinary pause teardown out of band so a pending
                // native drag/source continuation cannot hold the release fence.
                self.inject.backend().set_input_paused(true);
                self.inject.suspend();
                for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
                    self.detach(peer);
                }
                self.adapter.router_mut().set_input_suspended(true);
                self.motion = None;
                self.outputs(None);
                let fence = Arc::new(std::sync::atomic::AtomicU8::new(0));
                self.inject
                    .push_in_epoch(self.input_safety, InjectItem::UpdateFence(fence.clone()));
                let _ = reply.send(Ok(fence));
            }
            InputCommand::CommitUpdate { generation } => {
                if self.update_hold.is_some_and(|(old, _)| old == generation) {
                    self.update_committed = true;
                }
            }
            InputCommand::AbortUpdate { generation } => {
                if self.update_hold.is_some_and(|(old, _)| old == generation) {
                    self.update_hold = None;
                    self.update_committed = false;
                    self.inject.backend().set_input_paused(self.paused);
                    self.adapter
                        .router_mut()
                        .set_input_suspended(self.paused || self.environment.system_sleeping);
                    self.adapter
                        .router_mut()
                        .set_paused(self.paused || self.environment.system_sleeping || !self.controllable);
                    for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
                        self.attach(peer);
                    }
                }
            }
            InputCommand::Overlay(id) => self.overlay = id,
            InputCommand::IdentifyScreens(id) => {
                if !self.paused && self.update_hold.is_none() {
                    for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
                        if self.adapter.attached(peer) {
                            self.send(peer, Message::Control(ControlMsg::IdentifyScreens { id }));
                        }
                    }
                }
            }
            InputCommand::Paused(paused) => {
                if paused {
                    self.adapter.router_mut().release_input_disables();
                }
                self.capture_initialized = true;
                if cfg!(windows) && self.paused != paused {
                    self.capture_detector = Default::default();
                }
                self.paused = paused;
                self.inject
                    .backend()
                    .set_input_paused(paused || self.update_hold.is_some());
                if paused {
                    self.inject.suspend();
                }
                if paused {
                    self.overlay_escape = None;
                    self.declarations.clear();
                    self.trail.pause();
                    for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
                        self.detach(peer);
                        self.pairs.get_mut(&peer).unwrap().outbound = None;
                    }
                    self.motion = None;
                }
                self.adapter
                    .router_mut()
                    .set_input_suspended(paused || self.environment.system_sleeping);
                self.adapter
                    .router_mut()
                    .set_paused(paused || self.environment.system_sleeping || !self.controllable);
            }
            InputCommand::Controllable(controllable) => {
                if !controllable {
                    self.capture_fault = None;
                }
                if self.controllable != controllable {
                    self.notices.post(InputNotice::Controllable(controllable));
                }
                self.controllable = controllable;
                self.adapter
                    .router_mut()
                    .set_paused(self.paused || self.environment.system_sleeping || !controllable);
            }
            InputCommand::FocusLock(locked) => self.adapter.router_mut().set_locked(locked),
            InputCommand::ReleaseOwnedFocusLock { reply } => {
                let failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
                self.inject
                    .push_in_epoch(self.input_safety, InjectItem::OwnerBegin(failures.clone()));
                self.adapter.router_mut().release_owned_focus_lock();
                self.owner_fence(reply, failures, false, None);
            }
            InputCommand::LockFocusTo { target, reply } => {
                let result = self.adapter.router_mut().lock_focus_to(target);
                let _ = reply.try_send(result);
            }
            InputCommand::MovePointerTo(target) => {
                let _ = self.adapter.router_mut().move_pointer_to(target);
            }
            InputCommand::ReturnPointerHome => self.adapter.router_mut().return_pointer_home(),
            InputCommand::PeerAdmission(admission) => {
                let peer = admission.boot.device_id;
                let changed = self
                    .pairs
                    .get(&peer)
                    .and_then(|pair| pair.admission.as_ref())
                    .is_some_and(|old| old.boot != admission.boot || old.link != admission.link);
                if changed {
                    self.detach(peer);
                    if let Some(pair) = self.pairs.get_mut(&peer) {
                        pair.outbound = None;
                        pair.inbound = None;
                        pair.opening = 0;
                    }
                    self.adapter.clear_received(peer);
                }
                self.adapter.router_mut().admit_boot(admission.boot);
                let now = self.micros();
                self.adapter.router_mut().set_application_focus_capability(
                    admission.boot,
                    admission.application_focus(),
                    now,
                );
                self.pairs.entry(peer).or_default().admission = Some(admission);
            }
            InputCommand::RetirePeer(boot) => {
                let peer = boot.device_id;
                if self
                    .pairs
                    .get(&peer)
                    .and_then(|pair| pair.admission.as_ref())
                    .is_some_and(|old| old.boot == boot)
                {
                    self.detach(peer);
                    if let Some(pair) = self.pairs.get_mut(&peer) {
                        pair.outbound = None;
                        pair.inbound = None;
                        pair.admission = None;
                        pair.opening = 0;
                    }
                    self.adapter.clear_received(peer);
                }
                self.adapter.router_mut().retire_boot(boot);
            }
            InputCommand::KeyboardDisabled(disabled) => {
                let enabling = disabled && !self.paused && !self.adapter.router().keyboard_disabled();
                let held = if enabling {
                    self.inject.backend().held_session_keys()
                } else {
                    Vec::new()
                };
                self.adapter.router_mut().set_keyboard_disabled(disabled);
                if enabling {
                    self.adapter.router_mut().release_session_keys(&held);
                }
            }
            InputCommand::SessionLocked(locked) => {
                if self.session_locked != locked {
                    self.cursor_invalidate();
                }
                self.session_locked = locked;
                self.adapter.router_mut().keyboard_session_locked(locked);
            }
            InputCommand::GameMode(mode) => self.adapter.router_mut().set_game_mode(mode),
            InputCommand::ProjectionGesture(active) => {
                self.projection_until = active.then_some(self.now + Duration::from_secs(2));
                self.adapter.router_mut().set_projection_gesture(active);
            }
            InputCommand::TrailPinned(pinned) => self.adapter.router_mut().set_trail_pinned(pinned),
            InputCommand::LocalFeel(feel) => self.adapter.router_mut().set_local_feel(feel),
            InputCommand::LocalDisplays(displays) => {
                self.observe_display_fault(if displays.is_empty() {
                    oink_protocol::v1::dto::ResourceFaultCode::DeviceUnavailable
                } else {
                    oink_protocol::v1::dto::ResourceFaultCode::None
                });
                if self.adapter.router().local_displays() != displays {
                    if let Some(trace) = &self.inject.trace {
                        trace.displays(self.local, &displays);
                    }
                    self.cursor_invalidate();
                    self.trail.displays(displays.clone());
                    self.adapter.router_mut().set_local_displays(displays);
                }
                if let Some(cursor) = self.startup_cursor.take() {
                    self.adapter.router_mut().set_cursor(cursor);
                }
            }
            InputCommand::LocalDisplayFailure => {
                self.observe_display_fault(oink_protocol::v1::dto::ResourceFaultCode::BackendFailed);
            }
            InputCommand::PeerFacts {
                application_modes,
                boot,
                os,
                displays,
                transform,
            } => {
                if self
                    .pairs
                    .get(&boot.device_id)
                    .and_then(|pair| pair.admission.as_ref())
                    .is_some_and(|admission| admission.boot == boot)
                {
                    // Attachment replays this cache after the input opening completes.
                    // Facts may arrive first, so retain their OS for that later attach.
                    self.pairs.get_mut(&boot.device_id).unwrap().os = Some(os);
                    let now = self.micros();
                    self.adapter.router_mut().set_peer_os(boot.device_id, os, now);
                    self.adapter.router_mut().set_peer_displays(boot.device_id, displays);
                    self.adapter.router_mut().set_peer_input_transform(boot, transform);
                    let capable = self
                        .pairs
                        .get(&boot.device_id)
                        .and_then(|p| p.admission.as_ref())
                        .is_some_and(|p| p.application_focus());
                    self.adapter.router_mut().set_application_focus_participation(
                        boot,
                        capable,
                        application_modes,
                        now,
                    );
                }
            }
            InputCommand::DiagnosticsSnapshot => {
                if let Some(trace) = &self.inject.trace {
                    trace.topology(
                        &self.layout.topology.with_online(self.online.clone()),
                        self.layout.group,
                        self.layout.layout.clone(),
                        self.layout.owners.clone(),
                        self.layout.version.clone(),
                        Some(self.online.clone()),
                    );
                }
            }
            InputCommand::EdgeResistance(regions) => self.adapter.router_mut().set_edge_resistance_regions(regions),
            InputCommand::PointerSample {
                scope,
                application,
                sample,
                hidden,
            } => self.pointer_sample(scope, application, sample, hidden),
            InputCommand::VolumeExit(exit, keep) => {
                self.adapter.router_mut().set_volume_exit_probe(Arc::new(move || {
                    exit.load()
                        .as_ref()
                        .as_ref()
                        .cloned()
                        .and_then(oink_routing::input::VolumeRoute::from_session)
                }));
                self.adapter
                    .router_mut()
                    .set_volume_keep_local_probe(Arc::new(move || keep.load(Ordering::Acquire)));
            }
            InputCommand::Hotkeys(hotkeys) => {
                self.resume_hotkey = hotkeys
                    .iter()
                    .find(|binding| binding.action == "toggle_sharing")
                    .map(|binding| {
                        use oink_routing::Mods;
                        let chord = binding.chord;
                        oink_input::capture_loop::ResumeHotkey {
                            key: chord.key,
                            control: chord.mods.contains(Mods::CTRL),
                            alt: chord.mods.contains(Mods::ALT),
                            shift: chord.mods.contains(Mods::SHIFT),
                            meta: chord.mods.contains(Mods::META),
                        }
                    });
                self.adapter.router_mut().set_hotkeys(hotkeys);
            }
            InputCommand::EventFrequency(frequency) => {
                if self.budget.frequency() != frequency {
                    self.flush_motion_for(crate::diagnostics::handoff::FlushReason::FrequencyChanged, None);
                    self.budget.reset(frequency, self.now);
                }
            }
            InputCommand::Outbound { peer, input, motion } => {
                if self
                    .pairs
                    .get(&peer)
                    .and_then(|pair| pair.admission.as_ref())
                    .is_none_or(|admission| admission.link != input.id().link)
                {
                    return;
                }
                self.detach(peer);
                self.pairs.entry(peer).or_default().outbound = Some((input, motion));
                self.adapter
                    .router_mut()
                    .set_touchpad_contacts(self.inject.backend().touchpad_contacts());
                for mut message in self.adapter.opening_state(self.controllable) {
                    let admission = self.pairs[&peer].admission.as_ref().expect("admitted writer");
                    if let Message::Control(ControlMsg::InputCapability { touchpad_contacts, .. }) = &mut message
                        && !admission.contacts()
                    {
                        *touchpad_contacts = None;
                    }
                    if wire::supported(&message, admission) {
                        self.post(peer, message, false);
                    }
                }
                self.attach(peer);
            }
            InputCommand::OutboundGone { peer } => {
                if let Some(pair) = self.pairs.get_mut(&peer) {
                    pair.outbound = None;
                }
                self.motion = self.motion.take().filter(|m| m.peer != peer);
                self.detach(peer);
            }
            InputCommand::Inbound {
                peer,
                mut input,
                motion,
                peer_os,
            } => {
                if self
                    .pairs
                    .get(&peer)
                    .and_then(|pair| pair.admission.as_ref())
                    .is_none_or(|admission| admission.link != input.id().link)
                {
                    return;
                }
                self.detach(peer);
                self.adapter.clear_received(peer);
                let wake = self.shared.wake.get().expect("native wake installed");
                input.set_wake(wake.clone());
                if let Some(motion) = &motion {
                    motion.set_wake(wake.clone());
                }
                let pair = self.pairs.entry(peer).or_default();
                pair.inbound = Some((input, motion));
                pair.opening = 0;
                pair.os = Some(peer_os);
            }
            InputCommand::InboundGone { peer, stream } => {
                if self
                    .pairs
                    .get(&peer)
                    .and_then(|p| p.inbound.as_ref())
                    .is_some_and(|p| p.0.id() == stream)
                {
                    self.pairs.get_mut(&peer).unwrap().inbound = None;
                    self.detach(peer);
                    self.adapter.clear_received(peer);
                }
            }

            #[cfg(debug_assertions)]
            InputCommand::DndWatchdogSnapshotForTest(reply) => {
                let snapshot = format!(
                    "{}\ncursor_read_pending={} capture_motion_pending={} declarations={} injection_oldest={:?}",
                    self.adapter.router().dnd_watchdog_snapshot_for_test(),
                    self.cursor.pending.is_some(),
                    self.motion.is_some(),
                    self.declarations.len(),
                    self.inject.oldest_pending().map(|at| at.elapsed()),
                );
                let _ = reply.try_send(snapshot);
            }
            InputCommand::DndContentDemand {
                peer,
                offer,
                holder,
                commit,
                diagnostic,
                reply,
            } => {
                let trace =
                    oink_foundation::drag_trace::Trace::new(oink_foundation::drag_trace::stage::AUTHORIZE, diagnostic);
                let router = self.adapter.router_mut();
                let accepted = trace.scope(|| {
                    if commit {
                        router.on_dnd_content_demand(peer, offer, holder)
                    } else {
                        router.on_dnd_namespace_demand(peer, offer, holder)
                    }
                });
                trace.result(if accepted { 0 } else { -1 });
                let _ = reply.send(accepted);
            }
            InputCommand::DndDeliveryReceipt {
                peer,
                delivery,
                success,
                reply,
            } => {
                let _ = reply.send(
                    self.adapter
                        .router_mut()
                        .on_dnd_delivery_receipt(peer, delivery, success),
                );
            }
            InputCommand::DndPrepared(command) => {
                // A release/leave/pause may overtake metadata readiness. Never
                // synthesize a late Down for that retired target.
                if let oink_routing::DndCmd::Begin {
                    ref grant, id, epoch, ..
                } = command
                {
                    if grant.begin_current() {
                        self.dispatch_dnd(command);
                    } else {
                        // Up may arrive after the worker cached its result but
                        // before this command. Retire readers on the ordinary
                        // owner and report failure even though native Begin
                        // never ran; otherwise source and cache remain hosted.
                        self.dispatch_dnd(oink_routing::DndCmd::Cancel {
                            id,
                            epoch,
                            physical: false,
                        });
                        self.command(InputCommand::DndEvent(DndEvent::Ended {
                            id,
                            epoch,
                            dropped: false,
                        }));
                    }
                }
            }
            InputCommand::DndEvent(event) => {
                self.refresh_system_cursor_clip();
                if let Some(trace) = &self.inject.trace {
                    trace.dnd_event(&event);
                }
                if matches!(
                    event,
                    oink_routing::DndEvent::Ended { .. }
                        | oink_routing::DndEvent::HostLeft { ok: false, .. }
                        | oink_routing::DndEvent::SourceHeld { ok: false, .. }
                ) {
                    self.declarations.remove(&None);
                }
                self.adapter.router_mut().on_dnd_event(event);
            }
            InputCommand::DndAvailable(available) => self.adapter.router_mut().set_dnd_available(available),
            InputCommand::DndItems {
                peer,
                stream,
                notice,
                incomplete,
            } => {
                let now = self.micros();
                let accepted = self.current_inbound(peer, stream);
                if let Some(trace) = &self.inject.trace {
                    trace.dnd_items(
                        peer,
                        notice.key.offer.clip,
                        notice.state.holder().map_or(0, |h| h.epoch),
                        Some(&notice.items),
                        accepted,
                    );
                }
                if accepted {
                    self.adapter.router_mut().on_dnd_notice(peer, notice, incomplete, now);
                }
            }
            InputCommand::DndRefused {
                peer,
                stream,
                key,
                revision,
            } => {
                if self.current_inbound(peer, stream) || self.current_outbound(peer, stream) {
                    self.adapter.router_mut().on_dnd_attachment_refused(peer, key, revision);
                }
            }
            InputCommand::PeerMessage { peer, stream, message } => {
                if self.current_inbound(peer, stream) {
                    if let Some(trace) = &self.inject.trace {
                        trace.dnd_message(peer, false, &message);
                    }
                    if clip_decision(&message) {
                        self.refresh_system_cursor_clip();
                    }
                    self.adapter.inbound(peer, message);
                }
            }
            InputCommand::SendMessage { peer, stream, message } => {
                if self.current_outbound(peer, stream) {
                    self.flush_motion_for(crate::diagnostics::handoff::FlushReason::AttachmentBarrier, Some(peer));
                    self.post_wire(peer, message);
                }
            }
            InputCommand::Stop => {
                if !self.stopping {
                    self.stopping = true;
                    self.declarations.clear();
                    for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
                        self.detach(peer);
                    }
                    self.outputs(None);
                    self.inject.stop();
                }
            }
        }
        if self.update_hold.is_some() {
            self.adapter.router_mut().set_input_suspended(true);
        }
        self.outputs(None);
    }
    fn current_inbound(&self, peer: DeviceId, stream: StreamId) -> bool {
        self.pairs
            .get(&peer)
            .is_some_and(|pair| pair.attached && pair.inbound.as_ref().is_some_and(|p| p.0.id() == stream))
    }
    fn current_outbound(&self, peer: DeviceId, stream: StreamId) -> bool {
        self.pairs
            .get(&peer)
            .is_some_and(|pair| pair.attached && pair.outbound.as_ref().is_some_and(|p| p.0.id() == stream))
    }
    fn pointer_sample(
        &mut self,
        scope: Option<(u64, DeviceId)>,
        application: Option<crate::diagnostics::capture::Application>,
        sample: Option<oink_input::pointer_capture::CaptureSample>,
        hidden: bool,
    ) {
        let now = self.micros();
        let router = self.adapter.router_mut();
        let focus = router.focus();
        // The observer and native cursor owner run independently. An old
        // source-freeze sample must not acquire the returning holder's epoch.
        if scope != Some((focus.epoch, focus.holder)) {
            return;
        }
        let eligible = router.has_pointer_focus() && !router.paused();
        let capture_eligible = eligible && !router.dnd_pointer_active();
        let suppressed = router.has_pointer_focus() && hidden;
        self.trail.suppress(suppressed);
        if self.trail_suppressed_sent != Some(suppressed) {
            self.trail_suppressed_sent = Some(suppressed);
            let _ = self.main.send(MainCommand::TrailSuppressed(suppressed));
        }
        let interior = sample.is_some_and(|sample| {
            router.local_displays().iter().any(|display| {
                let b = display.bounds;
                sample.position.x > b.x + 8.
                    && sample.position.x < b.x + b.w - 8.
                    && sample.position.y > b.y + 8.
                    && sample.position.y < b.y + b.h - 8.
            })
        });
        if router.paused() || router.dnd_pointer_active() {
            self.capture_detector = Default::default();
        }
        let detected = self.capture_detector.update(
            (focus.epoch, focus.holder),
            sample,
            capture_eligible
                && (sample.is_none_or(|s| s.windows.is_none()) || router.game_mode().mode == GameMode::Auto),
            interior,
            now,
        );
        router.observe_pointer_capture(
            sample.map(|s| oink_routing::PointerCaptureObservation {
                hidden: s.hidden,
                application_hidden: s.application_hidden,
                foreground: s.foreground,
            }),
            detected,
            now,
        );
        let active = router.game_mode().active;
        if eligible
            && active
            && let Some(sample) = sample
        {
            router.set_cursor(sample.position);
        }
        self.capture_diagnostics.observe(
            crate::diagnostics::capture::Raw::new(
                &router.game_mode(),
                sample,
                application,
                self.local,
                router.local_displays(),
                self.capture_detector.detector.evidence(now),
                detected,
            ),
            self.inject.trace.as_deref(),
        );
        let relative_capture = active && !eligible && !router.paused() && !router.mouse_local();
        if self.relative_capture != relative_capture {
            self.relative_capture = relative_capture;
            self.inject
                .push_in_epoch(self.input_safety, InjectItem::SetRelativeCapture(relative_capture));
        }
    }
    fn read(&mut self) -> bool {
        if self.cursor.pending.is_some() {
            return false;
        }
        let mut progressed = false;
        // Reuse the visit buffer: inbound motion polling must not allocate a
        // fresh peer vector on every activation. Growth occurs only on membership changes.
        let mut peers = std::mem::take(&mut self.read_peers);
        peers.clear();
        peers.extend(self.pairs.keys().copied());
        // Every round visits each peer once. Injection backpressure leaves I0 in its
        // receive queue, allowing QUIC's stream window to stop the writer naturally.
        for &peer in &peers {
            if self.cursor.pending.is_some() || self.inject.available() < 4096 {
                break;
            }
            let received = self
                .pairs
                .get_mut(&peer)
                .and_then(|p| p.inbound.as_mut())
                .map(|p| p.0.take_timed());
            match received {
                Some(Ok(Some((bytes, arrived)))) => {
                    progressed = true;
                    let decoded = self
                        .pairs
                        .get(&peer)
                        .and_then(|pair| pair.admission.as_ref())
                        .map(|admission| wire::decode(&bytes, admission, false));
                    match decoded {
                        Some(Ok(wire::Decoded::Packet(message))) if valid_reliable(&message) => {
                            self.received(peer, message, arrived, false)
                        }
                        Some(Ok(wire::Decoded::Deferred(message))) => self.received_declaration(peer, message),
                        Some(Err(error)) => {
                            self.protocol_violation(peer, &bytes, error.code, false);
                            self.fault(peer, Which::Inbound, false, StreamCode::from_wire(error.code.number()));
                        }
                        _ => self.fault(peer, Which::Inbound, false, StreamCode::Invalid),
                    }
                }
                Some(Err(_)) => self.fault(peer, Which::Inbound, false, StreamCode::None),
                _ => {}
            }
            if self.cursor.pending.is_some() {
                break;
            }
            let mut bytes = [0u8; oink_protocol::v1::limits::DATAGRAM];
            let motion = self
                .pairs
                .get(&peer)
                .and_then(|pair| pair.inbound.as_ref())
                .and_then(|pair| pair.1.as_ref())
                .map(|motion| motion.take_timed_into(&mut bytes));
            if let Some(Ok(Some((len, arrived)))) = motion
                && self.adapter.attached(peer)
            {
                let decoded = self
                    .pairs
                    .get(&peer)
                    .and_then(|pair| pair.admission.as_ref())
                    .map(|admission| wire::decode(&bytes[..len], admission, true));
                match decoded {
                    Some(Ok(wire::Decoded::Packet(message))) if valid_datagram(&message) => {
                        progressed = true;
                        self.received(peer, message, arrived, true);
                    }
                    Some(Err(error)) => self.protocol_violation(peer, &bytes[..len], error.code, true),
                    _ => {}
                }
            }
        }
        self.read_peers = peers;
        progressed
    }
    fn protocol_violation(&self, peer: DeviceId, bytes: &[u8], code: oink_protocol::v1::ErrorCode, datagram: bool) {
        let Some(stream) = self.pairs.get(&peer).and_then(|p| p.inbound.as_ref()).map(|p| p.0.id()) else {
            return;
        };
        let type_id =
            oink_protocol::v1::frame::OpaqueFrame::parse_exact(bytes, oink_protocol::v1::limits::RELIABLE_FRAME)
                .ok()
                .map(|frame| frame.header.type_id);
        self.notices.post(InputNotice::ProtocolViolation {
            peer,
            stream,
            type_id,
            code,
            datagram,
        });
    }
    fn received_declaration(&mut self, peer: DeviceId, message: oink_protocol::v1::Message) {
        use oink_protocol::v1::{self, dto};
        if !self.adapter.attached(peer) || self.paused {
            return;
        }
        let notice = match &message {
            v1::Message::DndNotice(notice) => crate::dnd::wire::notice_header(notice).map(Some),
            v1::Message::AttachmentHead(head) => crate::dnd::wire::attachment_notice_header(head).map(Some),
            v1::Message::AttachmentResult(result) => result.kind().and_then(|kind| {
                if kind == dto::AttachmentKind::Drag {
                    Ok(None)
                } else {
                    Err(v1::ErrorCode::UnsupportedValue.into())
                }
            }),
            _ => Err(v1::ErrorCode::UnsupportedValue.into()),
        };
        match notice {
            Ok(Some(notice)) => {
                let now = self.micros();
                self.adapter.router_mut().on_dnd_notice(peer, notice, true, now);
                self.outputs(Some(peer));
            }
            Ok(None) => {}
            Err(_) => {
                self.fault(peer, Which::Inbound, false, StreamCode::Invalid);
                return;
            }
        }
        let stream = self.pairs[&peer].inbound.as_ref().expect("attached declaration").0.id();
        let _ = self.main.send(MainCommand::Attachment { peer, stream, message });
    }
    fn received(&mut self, peer: DeviceId, message: Message, arrived: Instant, lossy: bool) {
        if let Message::Control(ControlMsg::FocusEnter { epoch, .. }) = &message {
            self.adapter.router_mut().observe_dnd_focus_entry(peer, *epoch, arrived);
        }
        let (message, time) = match message {
            Message::TimedInput { time, input } => (Message::Input(input), Some(time)),
            message => (message, None),
        };
        let source = crate::diagnostics::keyboard::message_source(peer, &message).unwrap_or(peer);
        // Only admitted group sources get estimator state. Raw identities/times
        // never enter diagnostics; every source uses its own unsynchronised clock.
        let measure = self.adapter.attached(peer) && self.layout.devices.contains(&source);
        self.input_time = time;
        self.input_report = if measure && matches!(&message, Message::Input(_)) {
            crate::telemetry::producer()
                .as_ref()
                .and_then(|p| p.input_stamp_at(arrived, lossy))
        } else {
            None
        };
        if lossy {
            self.cursor_inbound(peer, message, Some(source));
            self.outputs(Some(source));
        } else {
            self.message(peer, message);
        }
        self.input_time = None;
        self.input_report = None;
        if let Some(time) = time
            && measure
        {
            if let Some(pending) = self.cursor.pending.as_mut() {
                pending.arrival = Some((source, time, arrived, lossy));
            } else {
                self.record_jitter(peer, source, time, arrived, lossy);
            }
        }
    }
    fn record_jitter(&mut self, peer: DeviceId, source: DeviceId, time: CaptureTime, arrived: Instant, lossy: bool) {
        let (ingress, dispatch) = self.jitter_sources.entry((peer, source, lossy)).or_default();
        let a = arrived.saturating_duration_since(self.start).as_micros() as u64;
        // Include FIFO reconciliation wait in actual routing dispatch latency.
        let d = Instant::now().saturating_duration_since(self.start).as_micros() as u64;
        let ingress = ingress.observe(time, a);
        let dispatch = dispatch.observe(time, d);
        self.shared.jitter.record(lossy, ingress, dispatch);
    }
    // This runs only from queued work/network dispatch or poll(), never event().
    // Reading before each remote motion makes clipping/release authoritative at
    // the target decision, independent of the idle refresh period or Auto mode.
    fn refresh_system_cursor_clip(&mut self) {
        self.clip_poll_at = self.now + Duration::from_millis(100);
        if self.paused || self.stopping || self.adapter.router().config().os != Os::Windows {
            return;
        }
        let pending = self.adapter.router().system_clip_recheck_pending();
        match self.inject.backend().system_cursor_clip() {
            Ok(clip) => {
                self.clip_available = true;
                let before = self.adapter.router().system_cursor_clip_status().0;
                self.adapter.router_mut().set_system_cursor_clip(clip);
                if self.adapter.router().system_cursor_clip_status().0 != before {
                    self.cursor.revision = self.cursor.revision.wrapping_add(1);
                }
                self.adapter.router_mut().resume_system_clip_motion();
                if pending {
                    // A deferred local edge still belongs to the capture owner.
                    // Park before publishing FocusEnter even if an older worker
                    // injection/restore is blocked. This is outside the LL hook.
                    self.outputs_at(None, ModeDispatch::ClipCheck);
                }
            }
            Err(_) => self.clip_available = false, // A failed query is not evidence of release.
        }
        let router = self.adapter.router();
        let focus = router.focus();
        let (rect, suppressed) = router.system_cursor_clip_status();
        let record = crate::diagnostics::cursor_clip::Record {
            holder: focus.holder,
            epoch: focus.epoch,
            rect,
            suppressed,
            active: rect.is_some() && router.has_pointer_focus() && !router.paused(),
            available: self.clip_available,
        };
        self.clip_diagnostics
            .observe(record, self.micros(), self.inject.trace.as_deref());
    }
    fn message(&mut self, peer: DeviceId, message: Message) {
        if self.update_hold.is_some() {
            return;
        }
        let opening = self.pairs[&peer].opening;
        let Some(admission) = self.pairs[&peer].admission.as_ref() else {
            return;
        };
        let opening_len = admission.opening_len();
        self.status_dirty |= opening < opening_len
            || matches!(
                message,
                Message::Control(ControlMsg::Feel { .. }) | Message::Dnd(DndMsg::Caps { .. })
            );
        if opening < opening_len {
            if !opening_at(admission.opening_index(opening).expect("opening index"), &message) {
                self.fault(peer, Which::Inbound, false, StreamCode::Invalid);
                return;
            }
            self.pairs.get_mut(&peer).unwrap().opening += 1;
        } else if matches!(message, Message::Control(ControlMsg::InputCapability { .. })) {
            self.fault(peer, Which::Inbound, false, StreamCode::Invalid);
            return;
        }
        if let Some(trace) = &self.inject.trace {
            trace.remote(peer, &message);
            trace.control(self.local, peer, false, &message);
            trace.dnd_message(peer, false, &message);
        }
        let source = crate::diagnostics::keyboard::message_source(peer, &message).or(Some(peer));
        let attached = self.adapter.attached(peer);
        if attached
            && matches!(message, Message::Control(ControlMsg::UserActivity))
            && self.adapter.router().accepts_group_user_activity(peer)
        {
            // Low pacing observes activity even when OS display wake is disabled.
            let _ = self.main.send(MainCommand::Activity { from: Some(peer) });
        }
        let incoming_epoch = match &message {
            Message::Control(ControlMsg::FocusEnter { epoch, .. }) => Some(*epoch),
            _ => None,
        };
        if let Message::Control(ControlMsg::IdentifyScreens { id }) = message {
            if attached && !self.paused {
                let _ = self.main.send(MainCommand::IdentifyScreens(id));
            }
        } else {
            self.cursor_inbound(peer, message, source);
        }
        if let Some(epoch) = incoming_epoch {
            let focus = self.adapter.router().focus();
            if focus.epoch == epoch && focus.holder == self.local {
                self.inject.crossing(epoch, true);
            }
        }
        self.outputs(source);
        self.attach(peer);
    }
    fn application_focus_command(&mut self, command: oink_routing::ApplicationFocusCommand) {
        use oink_routing::ApplicationFocusCommand as C;
        let invalidate = match &command {
            C::Park(_) | C::Dismiss(_) => true,
            C::Cancel(request) => self
                .application_park
                .as_ref()
                .is_some_and(|(pending, _)| pending == request),
            C::Sample(claim) => self
                .application_park
                .as_ref()
                .is_some_and(|(pending, _)| pending.claim != *claim),
        };
        if invalidate && let Some((_, permit)) = self.application_park.take() {
            permit.revoke();
        }
        let permit = if let C::Park(request) = &command {
            let lifetime = Duration::from_millis(250);
            #[cfg(debug_assertions)]
            let permit = match self.inject.backend().application_focus_test_clock() {
                Some(clock) => oink_input::application_focus::Permit::new_for_test(lifetime, clock),
                None => oink_input::application_focus::Permit::new(lifetime),
            };
            #[cfg(not(debug_assertions))]
            let permit = oink_input::application_focus::Permit::new(lifetime);
            self.application_park = Some((request.clone(), permit.clone()));
            Some(permit)
        } else {
            None
        };
        let _ = self.main.send(MainCommand::ApplicationFocus { command, permit });
    }

    fn outputs(&mut self, from: Option<DeviceId>) {
        self.outputs_at(from, ModeDispatch::Queued);
    }
    fn outputs_at(&mut self, from: Option<DeviceId>, dispatch: ModeDispatch) {
        self.cursor_scope();
        if dispatch == ModeDispatch::Callback {
            self.cursor.revision = self.cursor.revision.wrapping_add(1);
        }
        if let Some(trace) = &self.inject.trace {
            let router = self.adapter.router();
            trace.dnd_state(router.dnd_state_name(), router.focus().epoch, router.local_pointer());
        }
        let router = self.adapter.router();
        let keyboard_remote = router.focus().holder != self.local && !router.keyboard_local() && !router.paused();
        if self.keyboard_remote != keyboard_remote && self.inject.backend().set_keyboard_remote(keyboard_remote).is_ok()
        {
            self.keyboard_remote = keyboard_remote;
        }
        let mut outputs = self.adapter.router_mut().drain_outputs();
        if !outputs.is_empty() {
            self.cursor.revision = self.cursor.revision.wrapping_add(1);
        }
        let emergency = outputs
            .iter()
            .any(|output| matches!(output, Output::Status(StatusEvent::EmergencyStop { .. })));
        let disables_released = outputs
            .iter()
            .any(|output| matches!(output, Output::Status(StatusEvent::InputDisablesReleased)));
        if disables_released {
            // The routing owner has already cleared the live mode. Retire the
            // matching saved snapshot too, so a later ordinary refresh cannot
            // reinstall Disabled while durable safety cleanup is queued.
            let mut settings = (*self.snapshots.settings.load()).clone();
            if settings.mouse_mode == oink_foundation::input::InputMode::Disabled {
                settings.mouse_mode = oink_foundation::input::InputMode::Shared;
                self.snapshots.settings.publish(settings);
                self.settings = self.snapshots.settings.load();
                self.source_modes_revision += 1;
            }
        }
        if disables_released && !emergency {
            self.emergency_sequence = self
                .emergency_sequence
                .checked_add(1)
                .expect("input disable safety sequence exhausted");
            self.status_dirty = true;
            self.publish();
        }
        let mut releases = Vec::new();
        if emergency {
            self.local_emergency_sequence = self
                .local_emergency_sequence
                .checked_add(1)
                .expect("local emergency sequence exhausted");
            self.emergency_sequence = self
                .emergency_sequence
                .checked_add(1)
                .expect("emergency sequence exhausted");
            // Router recognizes the hotkey before returning its cleanup batch. Publish
            // offline before handing any of that batch's native releases to injection.
            self.paused = true;
            self.inject.backend().set_input_paused(true);
            self.inject.suspend();
            self.declarations.clear();
            self.motion = None;
            self.trail.pause();
            for (peer, pair) in &mut self.pairs {
                pair.outbound = None;
                if pair.attached {
                    pair.attached = false;
                    pair.releasing = true;
                    releases.push(*peer);
                }
            }
            self.sync_layout(false);
            self.publish();
            for peer in &releases {
                if let Some(trace) = &self.inject.trace {
                    trace.pair(*peer, false);
                }
                self.adapter.detach(*peer);
            }
            self.adapter.router_mut().set_input_suspended(true);
            outputs.extend(self.adapter.router_mut().drain_outputs());
        }
        // Once confinement has entered this session's native FIFO, subsequent
        // mode switches use that FIFO too; a callback must not overtake a clip
        // release/resize that is still waiting on the injection worker.
        self.queued_cursor_constraints |= outputs.iter().any(|o| matches!(o, Output::ConstrainCursor(_)));
        let immediate_modes = dispatch != ModeDispatch::Queued
            && self.snapshots.mode_switch_in_callback
            && !self.queued_cursor_constraints;
        // Record only mode boundaries. An ordinary motion does not read the trace switch.
        for output in &outputs {
            if immediate_modes && let Some(trace) = &self.inject.trace {
                trace.output(output, self.adapter.router().focus().epoch);
            }
            let (item, point, remote) = match *output {
                Output::SetMode(mode) => (
                    InjectItem::SetMode(mode),
                    match mode {
                        CaptureMode::Remote { park } => Some(park),
                        CaptureMode::Local => None,
                    },
                    matches!(mode, CaptureMode::Remote { .. }),
                ),
                Output::RestoreLocal { at } => (InjectItem::RestoreLocal { at }, Some(at), false),
                _ => continue,
            };
            self.inject.crossing(self.adapter.router().focus().epoch, false);
            if oink_input::native_lifecycle::lifecycle_enabled() {
                let focus = self.adapter.router().focus();
                oink_input::native_lifecycle::emit_focused(
                    oink_input::native_lifecycle::Kind::ModeDecision,
                    oink_input::native_lifecycle::Data {
                        remote: Some(remote),
                        point: point.map(|p| [p.x.round() as i32, p.y.round() as i32]),
                        reason: dispatch as u32,
                        success: Some(immediate_modes),
                        ..Default::default()
                    },
                    (focus.holder, focus.epoch),
                );
            }
            if immediate_modes {
                // hand_off emits Send before SetMode. Apply the native transition before
                // publishing FocusEnter, on the capture owner (callback or clip recheck).
                self.inject.mode_in_callback(self.input_safety, item);
            }
        }
        for output in outputs {
            if let Output::Status(StatusEvent::GameMode(status)) = &output {
                self.capture_diagnostics
                    .game(status, self.adapter.router().paused(), self.inject.trace.as_deref());
            }
            if !immediate_modes && let Some(trace) = &self.inject.trace {
                trace.output(&output, self.adapter.router().focus().epoch);
            }
            self.status_dirty |= matches!(output, Output::Status(_));
            match output {
                Output::ExitVolumeKey {
                    source,
                    route,
                    press_id,
                    code,
                    pressed,
                    repeat,
                    modifiers,
                } => {
                    let Some(work) = self.inject.ordinary_input(self.input_safety) else {
                        continue;
                    };
                    let _ = self.main.send(MainCommand::ExitVolumeKey {
                        work,
                        source,
                        route,
                        press_id,
                        code,
                        pressed,
                        repeat,
                        modifiers,
                    });
                }
                Output::Send { to, msg, delivery: _ } => self.send_from(to, msg, from),
                Output::ApplicationFocus(command) => self.application_focus_command(command),
                Output::Inject(event @ InjectEvent::Volume { .. }) => {
                    if let Some(work) = self.inject.ordinary_input(self.input_safety) {
                        let _ = self.main.send(MainCommand::Media { event, work });
                    }
                }
                Output::Inject(event) => self.inject.push_reported_in_epoch(
                    self.input_safety,
                    InjectItem::Inject { event, from },
                    self.input_time,
                    self.input_report.take(),
                ),
                Output::ConstrainCursor(bounds) => self
                    .inject
                    .push_in_epoch(self.input_safety, InjectItem::ConstrainCursor(bounds)),
                Output::SetMode(_) | Output::RestoreLocal { .. } if immediate_modes => {}
                Output::SetMode(mode) => self.inject.push_in_epoch(self.input_safety, InjectItem::SetMode(mode)),
                Output::RestoreLocal { at } => self
                    .inject
                    .push_in_epoch(self.input_safety, InjectItem::RestoreLocal { at }),
                Output::Dnd(command) => {
                    // The ordinary worker may start immediately. Publish the
                    // owner's readiness/revocation before releasing its command.
                    self.dispatch_dnd(command);
                }
                Output::LocalUserActivity => {
                    let _ = self.main.send(MainCommand::Activity { from: None });
                }
                // Receipt was reported above independently of the wake setting.
                Output::UserActivity { .. } => {}
                Output::ResetUserIdle => {
                    let _ = self.main.send(MainCommand::ResetUserIdle);
                }
                Output::Status(StatusEvent::EmergencyStop { .. }) => {}
                Output::Status(status) => {
                    let work = if matches!(status, oink_routing::StatusEvent::Hotkey { .. }) {
                        let Some(work) = self.inject.ordinary_input(self.input_safety) else {
                            continue;
                        };
                        Some(work)
                    } else {
                        None
                    };
                    let _ = self.main.send(MainCommand::RoutingStatus { status, work });
                }
                decoration => {
                    self.trail.output(&decoration);
                    let _ = self.main.send(MainCommand::Decoration(decoration));
                }
            }
        }
        for peer in releases {
            self.inject
                .push_in_epoch(self.input_safety, InjectItem::ReleaseAll { peer });
        }
        if emergency {
            self.notices.post(InputNotice::EmergencyPause);
        }
    }
    fn dispatch_dnd(&mut self, command: oink_routing::DndCmd) {
        self.publish();
        if matches!(
            command,
            oink_routing::DndCmd::SetImage(_) | oink_routing::DndCmd::UpdateImage { .. }
        ) {
            let _ = self.main.send(MainCommand::DndArtwork(command));
        } else {
            self.inject.push_in_epoch(
                self.input_safety,
                InjectItem::Dnd {
                    command,
                    main: self.main.clone(),
                },
            );
        }
    }
    fn scheduler_record(&self, source: oink_foundation::realtime::ThreadRole, status: &SchedulerStatus) {
        use crate::threads::{RealtimeRecord, SCHEDULER_POLICY, SCHEDULER_TOTALS};
        let _ = self.main.send(MainCommand::Realtime(RealtimeRecord {
            source,
            code: SCHEDULER_TOTALS,
            at_us: self.micros(),
            values: [
                status.late_rounds as f64,
                status.late_max.as_secs_f64() * 1e6,
                status.over_budget as f64,
                status.over_budget_max.as_secs_f64() * 1e6,
            ],
        }));
        let (present, native, tier) = status.applied.as_ref().map_or((0., 0., 0.), |applied| {
            (
                1.,
                applied.native as f64,
                match applied.tier {
                    oink_foundation::realtime::Tier::Elevated => 1.,
                    oink_foundation::realtime::Tier::Realtime => 2.,
                },
            )
        });
        let _ = self.main.send(MainCommand::Realtime(RealtimeRecord {
            source,
            code: SCHEDULER_POLICY,
            at_us: self.micros(),
            values: [present, native, tier, status.demoted as f64],
        }));
    }
    fn trail_pointer(&mut self) {
        let router = self.adapter.router();
        if let Some(head) = router.trail_pointer() {
            let visible = router.local_pointer().unwrap_or(head);
            let clip = router.trail_clip_bounds();
            self.trail.pointer(head, visible, clip);
            let pointer = Some((head, visible, clip));
            if self.trail_pointer_sent != pointer {
                self.trail_pointer_sent = pointer;
                let _ = self.main.send(MainCommand::TrailPointer { head, visible, clip });
            }
        }
    }
    fn send(&mut self, peer: DeviceId, message: Message) {
        self.send_from(peer, message, None);
    }
    fn send_from(&mut self, peer: DeviceId, message: Message, origin: Option<DeviceId>) {
        // Router outputs can include local modifier snapshots/releases triggered
        // by remote input, or another origin's forwarded release triggered locally.
        // The ambient capture time belongs only to the triggering source. Attaching
        // it to a different outgoing origin mixes unsynchronised device clocks and
        // corrupts the receiver's delay baseline and marks healthy input as reordered.
        let time = self.input_time.filter(|_| {
            crate::diagnostics::keyboard::message_source(self.local, &message) == Some(origin.unwrap_or(self.local))
        });
        if let Message::Control(ControlMsg::FocusEnter { epoch, .. }) = &message
            && let Some(pair) = self.pairs.get_mut(&peer)
        {
            pair.first_motion_epoch = Some(*epoch);
        }
        let Some(stream) = self
            .pairs
            .get(&peer)
            .and_then(|p| p.outbound.as_ref())
            .map(|p| p.0.id())
        else {
            return;
        };
        if let Message::Dnd(DndMsg::Notice(notice)) = &message
            && let Some((head, tail)) = self.declarations.get(&None).and_then(|d| d.for_notice(notice))
        {
            self.flush_motion_for(crate::diagnostics::handoff::FlushReason::AttachmentBarrier, Some(peer));
            self.post_wire(peer, head.clone());
            if self.current_outbound(peer, stream) {
                let _ = self.main.send(MainCommand::Attachment {
                    peer,
                    stream,
                    message: head,
                });
                if let Some(message) = tail {
                    let _ = self.main.send(MainCommand::Low { peer, stream, message });
                }
            }
            return;
        }
        if matches!(message, Message::Dnd(DndMsg::Preview { .. })) {
            if let Some(trace) = &self.inject.trace {
                trace.dnd_message(peer, true, &message);
            }
            if let Message::Dnd(message) = message {
                // PNG bytes are immutable and shared. Neither CBOR encoding nor
                // a Low send runs on capture/input or the injection owner.
                let _ = self.main.send(MainCommand::DndPreview { peer, stream, message });
            }
            return;
        }
        if self
            .motion
            .as_ref()
            .is_some_and(|m| m.peer != peer || m.stream != stream)
        {
            self.flush_motion_for(crate::diagnostics::handoff::FlushReason::TargetChanged, Some(peer));
        }
        if let Message::Input(InputMsg::Motion { seq, dx, dy }) = message {
            let due = self.budget.ready_at(self.now);
            let crossing = self
                .pairs
                .get_mut(&peer)
                .and_then(|pair| pair.first_motion_epoch.take())
                .map(|epoch| {
                    let token = TimingToken { epoch, sequence: seq };
                    let mut event = TimingEvent::new(TimingStage::MotionPacing, token);
                    event.elapsed_us = due.saturating_duration_since(self.now).as_micros() as u64;
                    // Low 16 bits: Hz (0=unlimited); bit16: existing coalesced tail;
                    // bit17: first motion must wait for the pacing deadline.
                    event.code = if self.budget.frequency() == crate::api::EventFrequency::Hz125 {
                        125
                    } else {
                        0
                    } | (u32::from(self.motion.is_some()) << 16)
                        | (u32::from(due > self.now) << 17);
                    handoff_timing::emit(event);
                    (token, self.now)
                });
            if self.motion.is_none() && due <= self.now {
                self.budget.consume(self.now);
                self.post(
                    peer,
                    timed(Message::Input(InputMsg::Motion { seq, dx, dy }), time),
                    false,
                );
            } else {
                if let Some(motion) = &mut self.motion {
                    if crossing.is_some() {
                        motion.crossing = crossing;
                    }
                    motion.sequence = seq;
                    motion.time = time;
                    if self.inject.trace.as_ref().is_some_and(|t| t.enabled()) {
                        if let Some(evidence) = &mut motion.evidence {
                            evidence.count += 1;
                        }
                    } else {
                        motion.evidence = None;
                    }
                    motion.dx += dx;
                    motion.dy += dy;
                } else {
                    self.motion = Some(Motion {
                        at: self.now,
                        crossing,
                        peer,
                        stream,
                        sequence: seq,
                        evidence: self
                            .inject
                            .trace
                            .as_ref()
                            .filter(|t| t.enabled())
                            .map(|_| MotionEvidence {
                                first: seq,
                                count: 1,
                                origin: origin.or(Some(self.local)),
                            }),
                        dx,
                        dy,
                        time,
                    });
                }
                if due <= self.now {
                    self.flush_motion();
                }
            }
        } else {
            self.flush_motion_for(crate::diagnostics::handoff::FlushReason::OrderingBarrier, Some(peer));
            let datagram = valid_datagram(&message);
            self.post(peer, timed(message, time), datagram);
        }
    }
    fn flush_motion(&mut self) {
        self.flush_motion_for(crate::diagnostics::handoff::FlushReason::Due, None);
    }
    fn flush_motion_for(&mut self, reason: crate::diagnostics::handoff::FlushReason, next: Option<DeviceId>) {
        if let Some(motion) = self.motion.take()
            && self
                .pairs
                .get(&motion.peer)
                .and_then(|p| p.outbound.as_ref())
                .is_some_and(|p| p.0.id() == motion.stream)
        {
            if let Some((token, first)) = motion.crossing {
                let mut event = TimingEvent::new(TimingStage::MotionFlush, token);
                event.elapsed_us = self.now.saturating_duration_since(first).as_micros() as u64;
                event.code = reason as u32;
                event.count = motion.sequence.saturating_sub(token.sequence) + 1;
                handoff_timing::emit(event);
            }
            if let Some(trace) = &self.inject.trace
                && let Some(evidence) = &motion.evidence
            {
                trace.flush_detail(
                    motion.peer,
                    evidence.origin,
                    next,
                    evidence.first,
                    motion.sequence,
                    evidence.count,
                    oink_foundation::Point::new(motion.dx, motion.dy),
                    reason,
                );
            }
            self.post(
                motion.peer,
                timed(
                    Message::Input(InputMsg::Motion {
                        seq: motion.sequence,
                        dx: motion.dx,
                        dy: motion.dy,
                    }),
                    motion.time,
                ),
                false,
            );
            self.budget.consume(self.now);
        }
    }
    fn input_deadline(&mut self) -> Option<Instant> {
        if let Some((_, expires)) = self.update_hold
            && !self.update_committed
        {
            return Some(expires);
        }
        self.adapter
            .router()
            .next_deadline()
            .map(|at| self.start + Duration::from_micros(at))
            .into_iter()
            .chain(self.projection_until)
            .chain(
                self.adapter
                    .router()
                    .system_clip_recheck_pending()
                    .then_some(if self.clip_available {
                        self.now
                    } else {
                        self.clip_poll_at
                    }),
            )
            .chain(self.cursor.pending.as_ref().and_then(|p| p.retry))
            .chain(self.cursor.pending.as_ref().map(|p| p.at + cursor::MAX_WAIT))
            .chain(self.motion.as_ref().map(|_| self.budget.ready_at(self.now)))
            .min()
    }
    fn wake_if_earlier(&mut self) {
        if self
            .input_deadline()
            .is_some_and(|next| self.deadline.is_none_or(|armed| next < armed))
        {
            self.shared.wake.get().expect("native wake installed")();
        }
    }
    fn post_wire(&mut self, peer: DeviceId, message: oink_protocol::v1::Message) {
        let Some(pair) = self.pairs.get(&peer) else {
            return;
        };
        let Some(admission) = pair.admission.as_ref() else {
            return;
        };
        if !admission.admits(message.type_id(), false) {
            return;
        }
        let mut buffer = [0; oink_protocol::v1::limits::RELIABLE_FRAME];
        let Ok(len) = wire::encode_wire(&message, &mut buffer) else {
            self.fault(peer, Which::Outbound, false, StreamCode::Invalid);
            return;
        };
        if self.post_bytes(peer, &buffer[..len], false) {
            let notice = match &message {
                oink_protocol::v1::Message::DndNotice(notice) => crate::dnd::wire::notice_header(notice).ok(),
                oink_protocol::v1::Message::AttachmentHead(head) => {
                    crate::dnd::wire::attachment_notice_header(head).ok()
                }
                _ => None,
            };
            if let Some(notice) = notice.filter(|notice| notice.key.offer.clip.origin == self.local) {
                crate::dnd::timing::notice(notice.key.offer.clip, notice.state, Instant::now());
            }
        }
    }
    fn post(&mut self, peer: DeviceId, message: Message, datagram: bool) {
        if matches!(
            message,
            Message::Input(_) | Message::TimedInput { .. } | Message::Control(ControlMsg::FocusEnter { .. })
        ) && !self.inject.backend().input_safety().accepts(self.input_safety)
        {
            return;
        }
        let Some(pair) = self.pairs.get(&peer) else {
            return;
        };
        let Some(admission) = pair.admission.as_ref() else {
            return;
        };
        if pair.outbound.is_none() || !wire::supported(&message, admission) {
            return;
        }
        let mut buffer = [0; oink_protocol::v1::limits::RELIABLE_FRAME];
        let maximum = if datagram {
            oink_protocol::v1::limits::DATAGRAM
        } else {
            buffer.len()
        };
        let Ok(len) = wire::encode(&message, &mut buffer[..maximum]) else {
            self.fault(peer, Which::Outbound, false, StreamCode::Invalid);
            return;
        };
        let type_id = u16::from_le_bytes([buffer[0], buffer[1]]);
        if !admission.admits(type_id, datagram) {
            self.fault(peer, Which::Outbound, false, StreamCode::Invalid);
            return;
        }
        if let Some(trace) = &self.inject.trace {
            trace.control(self.local, peer, true, &message);
            trace.dnd_message(peer, true, &message);
        }
        if self.post_bytes(peer, &buffer[..len], datagram) {
            if let Message::Dnd(DndMsg::Notice(notice)) = &message
                && notice.key.offer.clip.origin == self.local
            {
                crate::dnd::timing::notice(notice.key.offer.clip, notice.state, Instant::now());
            }
            if let Some(trace) = &self.inject.trace {
                trace.sent_held_sync(self.local, peer, &message);
            }
        }
    }
    fn post_bytes(&mut self, peer: DeviceId, encoded: &[u8], datagram: bool) -> bool {
        let Some((stream, motion)) = self.pairs.get(&peer).and_then(|pair| pair.outbound.as_ref()) else {
            return false;
        };
        if datagram {
            return motion.as_ref().is_some_and(|motion| motion.post(encoded).is_ok());
        }
        if let Err(error) = stream.post(encoded.to_vec()) {
            self.fault(
                peer,
                Which::Outbound,
                error == PostError::Full,
                if error == PostError::TooLarge {
                    StreamCode::Invalid
                } else if error == PostError::Closed && stream.is_cancelled() {
                    StreamCode::Cancelled
                } else {
                    StreamCode::None
                },
            );
            return false;
        }
        true
    }
}
impl CaptureLoop for Driver {
    fn paused_hotkey(&self) -> Option<oink_input::capture_loop::ResumeHotkey> {
        self.resume_hotkey.filter(|_| self.paused && !self.stopping)
    }
    fn resume_hotkey(&mut self) {
        if self.paused && !self.stopping {
            let _ = self.main.send(MainCommand::ResumeSharing {
                epoch: self.inject.backend().input_safety().epoch,
            });
        }
    }
    fn local_rewrite(&self) -> Option<oink_foundation::input::LocalRewrite> {
        self.adapter.router().local_rewrite()
    }
    fn local_modifier_mapping(
        &self,
    ) -> (
        Vec<(oink_foundation::input::KeyCode, oink_foundation::input::KeyCode)>,
        Vec<oink_foundation::input::KeyCode>,
    ) {
        self.adapter.router().local_modifier_mapping()
    }
    fn trace_focus(&self) -> Option<(DeviceId, u64)> {
        let focus = self.adapter.router().focus();
        Some((focus.holder, focus.epoch))
    }
    fn idle_refresh_allowed(&self) -> bool {
        self.capture_requested() && self.snapshots.settings.load().wake_displays_on_input
    }
    fn capture_requested(&self) -> bool {
        self.inject.backend().input_safety().active()
            && self.capture_initialized
            && !self.paused
            && !self.inject.suspended()
            && !self.environment.system_sleeping
            && self.controllable
            && !self.stopping
    }
    fn pointer_holder(&self) -> bool {
        self.capture_requested()
            && self.adapter.router().focus().holder == self.local
            && !self.adapter.router().game_mode().active
    }
    fn keyboard_remote(&self) -> Option<bool> {
        let router = self.adapter.router();
        Some(router.focus().holder != self.local && !router.keyboard_local() && !router.paused())
    }
    fn constrained_cursor(&self) -> Option<oink_foundation::Point> {
        (!self.paused)
            .then(|| self.adapter.router().constrained_local_cursor())
            .flatten()
    }
    fn mouse_disabled(&self) -> bool {
        !self.paused && self.adapter.router().mouse_disabled()
    }
    fn keyboard_disabled(&self) -> bool {
        !self.paused && self.adapter.router().keyboard_disabled()
    }
    fn forward_injected_input(&self) -> bool {
        self.snapshots.settings.load().forward_injected_input
    }
    fn keyboard_guard_suspended(&self) -> bool {
        // A capturable lock curtain keeps the guard. Secure desktop input is
        // outside the hook boundary and is passed through by Windows itself.
        false
    }
    fn keyboard_pointer_modifiers(&self) -> Vec<oink_foundation::input::KeyCode> {
        self.adapter.router().keyboard_pointer_modifiers()
    }
    fn keyboard_key_blocked(&self, code: Option<oink_foundation::input::KeyCode>) -> bool {
        self.adapter.router().keyboard_key_blocked(code)
    }
    fn set_wake(&mut self, wake: Wake) {
        self.inject.backend().watch_input_safety(&wake);
        let _ = self.shared.wake.set(wake.clone());
        self.inject.set_wake(wake);
        self.scheduler.applied = oink_foundation::realtime::read_back().ok();
    }
    fn capture_available(&mut self, available: bool) {
        self.capture_fault = Some(if available {
            oink_protocol::v1::dto::ResourceFaultCode::None
        } else {
            oink_protocol::v1::dto::ResourceFaultCode::CaptureUnavailable
        });
        self.now = Instant::now();
        self.adapter.at(self.micros());
        self.adapter.router_mut().set_capture_available(available);
        // Native attachment has completed; pause settling/tray state must not
        // wait for the platform sensor or statistics clock to notice it.
        self.outputs_at(None, ModeDispatch::Callback);
        self.publish();
        let _ = self.main.send(MainCommand::InputStatusChanged);
        self.wake_if_earlier();
    }
    fn capture_detached(&mut self) {
        // Withdrawal is an intentional source boundary, not an attachment error.
        // A completed withdrawal is an observed nonfault, not missing evidence.
        // None would make the keyboard card unknown throughout pause; unlike the
        // mouse it has no independent injection observation to retain its health.
        self.capture_fault = Some(oink_protocol::v1::dto::ResourceFaultCode::None);
        self.now = Instant::now();
        self.adapter.at(self.micros());
        self.adapter.router_mut().set_capture_available(false);
        self.outputs_at(None, ModeDispatch::Callback);
        self.publish();
        let _ = self.main.send(MainCommand::InputStatusChanged);
        self.wake_if_earlier();
    }
    fn event(&mut self, event: &CapturedEvent) -> Disposition {
        if !self.inject.backend().input_safety().accepts(self.input_safety) {
            return Disposition::Pass;
        }
        if event.physical {
            oink_foundation::maintenance::process().user_input();
        }
        let source_time = oink_input::source_time::capture_time();
        let captured = self.inject.trace.as_ref().and_then(|trace| trace.begin_capture());
        self.now = Instant::now();
        if matches!(
            event.event,
            oink_foundation::input::InputEvent::PointerDelta { .. }
                | oink_foundation::input::InputEvent::RawPointerDelta { .. }
        ) && self.inject.discard_source_motion()
        {
            // Local physical motion must obey the same short section as incoming
            // injected motion. macOS may already have updated its cursor before
            // the tap, so positioned Down remains necessary there as on Windows.
            // The next absolute capture supplies the actual cursor, not this delta.
            if let Some(trace) = &self.inject.trace {
                trace.finish_capture(event, captured, Disposition::Swallow);
            }
            return Disposition::Swallow;
        }
        if let oink_foundation::input::InputEvent::Key {
            code: oink_foundation::input::KeyCode::ESCAPE,
            pressed,
        } = event.event
        {
            if pressed
                && event.physical
                && let Some(id) = self.overlay
            {
                self.overlay_escape.get_or_insert((id, self.now));
                let now = self.micros();
                self.adapter.router_mut().observe_emergency_event(event, now);
                self.outputs(None);
                self.wake_if_earlier();
                if let Some(trace) = &self.inject.trace {
                    trace.finish_capture(event, captured, Disposition::Swallow);
                }
                return Disposition::Swallow;
            }
            if !pressed
                && event.physical
                && let Some((id, at)) = self.overlay_escape.take()
            {
                let now = self.micros();
                self.adapter.router_mut().observe_emergency_event(event, now);
                if self.settings.emergency_hold_ms == 0
                    || self.now.duration_since(at) < Duration::from_millis(u64::from(self.settings.emergency_hold_ms))
                {
                    let _ = self.main.send(MainCommand::DismissOverlay(id));
                }
                if let Some(trace) = &self.inject.trace {
                    trace.finish_capture(event, captured, Disposition::Swallow);
                }
                return Disposition::Swallow;
            }
        }
        self.adapter.at(self.micros());
        self.trail.at(self.micros() as f64 / 1_000_000.);
        let now = self.micros();
        if matches!(
            event.event,
            oink_foundation::input::InputEvent::Key {
                code: oink_foundation::input::KeyCode::CAPS_LOCK,
                pressed: true
            }
        ) {
            let enabled = self.inject.backend().caps_lock_switches_input_source();
            self.adapter.router_mut().set_mac_caps_switch(enabled.unwrap_or(false));
        }
        let disposition = self.adapter.router_mut().on_local_event(event, now);
        if let Some(trace) = &self.inject.trace {
            trace.finish_capture_path(
                event,
                captured,
                disposition,
                Some(self.adapter.router().contacts_path()),
            );
        }
        self.input_time = Some(source_time);
        self.outputs_at(None, ModeDispatch::Callback);
        self.input_time = None;
        self.trail_pointer();
        self.publish();
        self.wake_if_earlier();
        disposition
    }
    fn poll(&mut self, now: Instant) -> Option<Instant> {
        self.now = now;
        self.adapter.at(self.micros());
        self.trail.at(self.micros() as f64 / 1_000_000.);
        if let Some(due) = self.deadline
            && now > due
        {
            self.scheduler.late_rounds += 1;
            self.scheduler.late_max = self.scheduler.late_max.max(now - due);
        }
        self.refresh();
        if (self.cursor.pending.is_none() && now >= self.clip_poll_at)
            || (self.clip_available && self.adapter.router().system_clip_recheck_pending())
        {
            self.refresh_system_cursor_clip();
        }
        self.check_backlog(now);
        if let Some((generation, expires)) = self.update_hold {
            if now >= expires && !self.update_committed {
                self.command(InputCommand::AbortUpdate { generation });
                oink_foundation::maintenance::process().reopen(generation);
            } else {
                self.adapter.router_mut().set_input_suspended(true);
            }
        }
        if self.projection_until.is_some_and(|until| until <= now) {
            self.projection_until = None;
            self.adapter.router_mut().set_projection_gesture(false);
        }
        while let Some(command) = self.shared.commands.pop() {
            match command {
                super::commands::Batch::Action(command) => self.queued(command),
                super::commands::Batch::State => {
                    for command in self.shared.commands.take_state() {
                        self.queued(command.queued());
                    }
                }
            }
        }
        let safety = self.inject.backend().input_safety();
        if safety != self.input_safety {
            // Even a pause/resume completed between polls ends the old session.
            self.adapter.router_mut().set_input_suspended(true);
            for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
                self.fault(peer, Which::Inbound, false, StreamCode::Cancelled);
                self.fault(peer, Which::Outbound, false, StreamCode::Cancelled);
            }
            self.input_safety = safety;
            self.adapter
                .router_mut()
                .set_input_suspended(self.paused || self.environment.system_sleeping || self.update_hold.is_some());
            self.adapter
                .router_mut()
                .set_paused(self.paused || self.environment.system_sleeping || !self.controllable);
        }
        if !self.paused && !self.stopping && self.update_hold.is_none() && self.inject.suspended() {
            match self.inject.resume() {
                Ok(true) => {
                    self.cursor_invalidate();
                    // The old worker has joined and released every held input.
                    // A discarded release acknowledgment cannot keep a peer in
                    // the previous session's releasing state forever.
                    for peer in self.pairs.keys().copied().collect::<Vec<_>>() {
                        self.pairs.get_mut(&peer).unwrap().releasing = false;
                        self.attach(peer);
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::error!(%error, "could not recreate input injection session"),
            }
        }
        while let Some(peer) = self.inject.take_released() {
            if let Some(pair) = self.pairs.get_mut(&peer) {
                pair.releasing = false;
                self.status_dirty = true;
            }
            self.attach(peer);
        }
        self.cursor_resume();
        self.cursor_diagnostics();
        let mut more = false;
        if !self.stopping {
            loop {
                if !self.read() {
                    break;
                }
                if now.elapsed() >= Duration::from_micros(200) {
                    more = true;
                    break;
                }
            }
            let micros = self.micros();
            self.adapter.router_mut().tick(micros);
            self.outputs(None);
            if self.motion.is_some() && self.budget.ready_at(now) <= now {
                if let Some((token, _)) = self.motion.as_ref().and_then(|motion| motion.crossing) {
                    handoff_timing::emit(TimingEvent::new(TimingStage::MotionTimer, token));
                }
                self.flush_motion();
            }
        }
        self.trail_pointer();
        self.publish();
        self.reply_to_owners();
        if now >= self.read_back_at {
            if oink_foundation::realtime::read_back().ok() != self.scheduler.applied {
                self.scheduler.demoted += 1;
            }
            if self.scheduler.demoted != self.shared.scheduler.load().demoted {
                self.scheduler_record(oink_foundation::realtime::ThreadRole::Input, &self.scheduler);
            }
            self.shared.scheduler.publish(self.scheduler.clone());
            self.read_back_at = now + oink_foundation::realtime::READBACK_INTERVAL;
        }
        let elapsed = now.elapsed();
        if elapsed > oink_foundation::realtime::ThreadRole::Input.budget().computation {
            self.scheduler.over_budget += 1;
            self.scheduler.over_budget_max = self.scheduler.over_budget_max.max(elapsed);
        }
        self.deadline = self
            .input_deadline()
            .into_iter()
            .chain(
                self.display_failure_since
                    .map(|since| since + Duration::from_secs(3))
                    .filter(|due| *due > now),
            )
            .chain(Some(self.read_back_at))
            .chain(Some(self.backlog_check))
            .chain(
                (!self.paused && self.cursor.pending.is_none() && self.adapter.router().config().os == Os::Windows)
                    .then_some(self.clip_poll_at),
            )
            .chain(more.then_some(now))
            .min();
        self.deadline
    }
    fn stopped(&self) -> bool {
        self.stopping && self.inject.stopped()
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        if let Some((_, permit)) = self.application_park.take() {
            permit.revoke();
        }
        self.shared.stopped.store(true, Ordering::Release);
        self.notices.closed.store(true, Ordering::Release);
        self.notices.changed.notify_one();
        let _ = self.done.send(());
    }
}
fn opening_at(index: usize, message: &Message) -> bool {
    matches!(
        (index, message),
        (0, Message::Control(ControlMsg::InputCapability { .. }))
            | (1, Message::Control(ControlMsg::Feel { .. }))
            | (2, Message::Control(ControlMsg::FocusState { session: Some(_), .. }))
            | (3, Message::Control(ControlMsg::GameMode { .. }))
            | (4, Message::Dnd(DndMsg::Caps { .. }))
    )
}
fn valid_datagram(message: &Message) -> bool {
    matches!(message, Message::Input(input) | Message::TimedInput { input, .. } if input.is_datagram())
}
fn valid_reliable(message: &Message) -> bool {
    match message {
        Message::Input(input) | Message::TimedInput { input, .. } => !input.is_datagram(),
        Message::Control(_) => true,
        Message::Dnd(
            DndMsg::Caps { .. }
            | DndMsg::Enter { .. }
            | DndMsg::Notice(_)
            | DndMsg::NoticeRequest { .. }
            | DndMsg::Refused { .. }
            | DndMsg::SourceButton { .. }
            | DndMsg::Source { .. }
            | DndMsg::PreviewRequest { .. },
        ) => true,
        _ => false,
    }
}

fn timed(message: Message, time: Option<CaptureTime>) -> Message {
    match (message, time) {
        (Message::Input(input), Some(time)) if captured_input(&input) => Message::TimedInput { time, input },
        (message, _) => message,
    }
}
fn captured_input(input: &InputMsg) -> bool {
    match input {
        InputMsg::Forwarded { msg, .. } => captured_input(msg),
        InputMsg::HeldSync { .. } | InputMsg::ApplicationHeldSync { .. } | InputMsg::VolumeKeyOrigin { .. } => false,
        _ => true,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests;

/// Confirmed capture belongs to this device, not a group focus epoch. Suspend
/// observation while away: the source freeze is Oink's, not the application's.
/// Unconfirmed evidence must still start over at a focus boundary.
#[derive(Default)]
struct ScopedCaptureDetector {
    scope: Option<(u64, DeviceId)>,
    detected: bool,
    detector: oink_input::pointer_capture::CaptureDetector,
}
impl ScopedCaptureDetector {
    fn update(
        &mut self,
        scope: (u64, DeviceId),
        sample: Option<oink_input::pointer_capture::CaptureSample>,
        eligible: bool,
        interior: bool,
        now: u64,
    ) -> bool {
        if !self.detected && (self.scope != Some(scope) || !eligible) {
            self.detector = Default::default();
        }
        self.scope = Some(scope);
        if !eligible {
            return false;
        }
        // Native restoration can temporarily yield no sample. Unknown state
        // cannot revoke confirmed capture; a valid sample resumes normal release
        // and foreground-change detection without requiring new motion.
        if sample.is_none() {
            return self.detected;
        }
        self.detected = self.detector.update(sample, true, interior, now);
        self.detected
    }
}

// Forwarded and timed motion must use the same target constraint as direct input.
fn clip_decision(message: &Message) -> bool {
    cursor::pointer_motion(message, true) || matches!(message, Message::Control(ControlMsg::FocusEnter { .. }))
}
