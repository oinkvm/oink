//! Capture-thread ownership of routing and the input stream endpoints.
//!
//! It alone runs capture and the router, with the motion pacing timer in its own event
//! loop (macOS: a timer source on the capture thread's run loop; Windows: the hook
//! thread's message loop with a high-resolution waitable timer; if the timer cannot be
//! created the thread fails to start, there is no fallback). It is the writer of the I0
//! and I1 this device opened and the reader of those the peer opened. It makes no system
//! queries, no drag-and-drop system calls, writes no file and formats no log line.
//!
//! In: [`InputCommand`]s from the input module's task, the snapshots below, wake-ups from
//! the network thread. Out: stream posts, injection items, [`super::MainCommand`]s,
//! [`InputNotice`]s, and the [`InputStatus`] snapshot.
use super::inject::InjectHandle;
use crate::config_module::RouterLayout;
pub use crate::streams::input::wire::PeerAdmission;
use oink_foundation::{DeviceId, snapshot::Snapshot};
use oink_network::link::{DatagramRecv, DatagramSend, RecvStream, SendStream, StreamId};
use oink_protocol::frame::StreamCode;
use oink_routing::{DndEvent, GameMode, Os, RoutingSettings};
use std::sync::Arc;

/// Ordinary thread (the input module's task) → input thread. Small and of fixed size;
/// the queue is bounded and `InputThread::send` never blocks.
#[derive(Debug)]
pub enum InputCommand {
    #[cfg(debug_assertions)]
    DndWatchdogSnapshotForTest(std::sync::mpsc::SyncSender<String>),
    ApplicationFocusObserved {
        claim: oink_routing::application_focus::Claim,
        observation: oink_foundation::application_focus::Observation,
    },
    ApplicationFocusApplied {
        request: oink_routing::application_focus::ParkRequest,
        result: oink_foundation::application_focus::ParkResult,
    },
    PrepareUpdate {
        generation: u64,
        reply:
            std::sync::mpsc::SyncSender<Result<Arc<std::sync::atomic::AtomicU8>, Vec<oink_update::barrier::Blocker>>>,
    },
    CommitUpdate {
        generation: u64,
    },
    AbortUpdate {
        generation: u64,
    },
    /// Local owner pause: detach capture and injection; keep only the native resume hotkey.
    Paused(bool),
    ApplyOwnerPreferences {
        expected_emergency_sequence: u64,
        routing: RoutingSettings,
        input_transform: oink_foundation::input_transform::InputTransform,
        accept_contacts: bool,
        reply: OwnerApplyReply,
    },
    ReceiveEmergency {
        reply: OwnerApplyReply,
    },
    TriggerEmergency {
        reply: OwnerApplyReply,
    },
    ApplyOwnerPause {
        paused: bool,
        expected_emergency_sequence: u64,
        reply: OwnerApplyReply,
    },
    Overlay(Option<u64>),
    /// Request identification on currently attached peers through I0.
    IdentifyScreens(u64),
    /// This device can or cannot be controlled (`set_paused(paused || !controllable)`).
    Controllable(bool),
    FocusLock(bool),
    ReleaseOwnedFocusLock {
        reply: OwnerApplyReply,
    },
    LockFocusTo {
        target: DeviceId,
        reply: std::sync::mpsc::SyncSender<Result<(), oink_routing::FocusLockError>>,
    },
    ReturnPointerHome,
    MovePointerTo(oink_foundation::DeviceId),
    GameMode(GameMode),
    KeyboardDisabled(bool),
    PeerAdmission(PeerAdmission),
    RetirePeer(oink_foundation::session::Boot),
    SessionLocked(Option<bool>),
    /// Keep the cursor trail lit (mini-game).
    TrailPinned(bool),
    ProjectionGesture(bool),
    /// Ordinary observer snapshots of local platform preferences.
    LocalFeel(oink_foundation::input::FeelProfile),
    LocalDisplays(Vec<oink_foundation::display::DisplayInfo>),
    /// Enumeration completed with an error; retain the last good geometry.
    LocalDisplayFailure,
    PeerFacts {
        application_modes: (oink_foundation::input::InputMode, oink_foundation::input::InputMode),
        boot: oink_foundation::session::Boot,
        os: Os,
        displays: Vec<oink_foundation::display::DisplayInfo>,
        transform: Option<oink_foundation::input_transform::InputTransform>,
    },
    EdgeResistance(Vec<oink_foundation::input::EdgeResistanceRegion>),
    PointerSample {
        /// Focus scope read before sampling; never relabel queued evidence at consumption.
        scope: Option<(u64, DeviceId)>,
        application: Option<crate::diagnostics::capture::Application>,
        sample: Option<oink_input::pointer_capture::CaptureSample>,
        hidden: bool,
    },
    VolumeExit(
        Arc<Snapshot<Option<Arc<oink_protocol::v1::dto::AudioSessionContext>>>>,
        Arc<std::sync::atomic::AtomicBool>,
    ),
    Hotkeys(Vec<oink_routing::HotkeyBinding>),
    EventFrequency(oink_display::layout::EventFrequency),
    /// Explicit recording start/save snapshot; no periodic input-owner work.
    DiagnosticsSnapshot,
    /// "This device's input stream is here": handles allocated by the task. The thread
    /// posts the opening state at once (the stream then appears on the wire) and attaches
    /// if the peer's stream is there, its opening is read and the last release completed.
    Outbound {
        peer: DeviceId,
        input: SendStream,
        motion: Option<DatagramSend>,
    },
    /// "This device's input stream is gone" (closed, paused, backing off): detach.
    OutboundGone {
        peer: DeviceId,
    },
    /// "The peer's input stream arrived": accepted handles and the system the peer
    /// reported in the handshake. The thread reads it and keeps its receive state.
    Inbound {
        peer: DeviceId,
        input: RecvStream,
        motion: Option<DatagramRecv>,
        peer_os: Os,
    },
    /// "The peer's input stream is gone or replaced": detach and clear the receive state.
    InboundGone {
        peer: DeviceId,
        stream: StreamId,
    },
    /// Result of a drag-and-drop system call from the main thread.
    DndEvent(DndEvent),
    DndPrepared(oink_routing::DndCmd),
    DndContentDemand {
        peer: DeviceId,
        offer: oink_foundation::content::OfferId,
        holder: oink_foundation::content::Holder,
        commit: bool,
        diagnostic: oink_foundation::drag_trace::Context,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    DndDeliveryReceipt {
        peer: DeviceId,
        delivery: oink_foundation::content::DeliveryId,
        success: bool,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
    DndAvailable(bool),
    DndItems {
        incomplete: bool,
        peer: DeviceId,
        stream: StreamId,
        notice: oink_routing::dnd::DragNotice,
    },
    DndRefused {
        peer: DeviceId,
        stream: StreamId,
        key: oink_routing::dnd::DragKey,
        revision: u64,
    },
    /// Large drag content/preview completed on an ordinary task.
    PeerMessage {
        peer: DeviceId,
        stream: StreamId,
        message: oink_routing::Packet,
    },
    /// Ordinary DnD task sends a small I0 control/result on the exact current writer.
    SendMessage {
        peer: DeviceId,
        stream: StreamId,
        message: oink_protocol::v1::Message,
    },
    Stop,
}

/// Which of the pair a fault concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Outbound,
    Inbound,
}

/// Input thread → the input module's task. One reserved slot per peer and kind of
/// notice, so posting never fails and never waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputNotice {
    /// The thread found a stream unusable and has already detached. Outbound: the task
    /// closes it and feeds the machine (`full` = the post reported full). Inbound with a
    /// code: the task closes the inbound stream with that code (`Invalid`, `Resend`).
    Fault {
        peer: DeviceId,
        which: Which,
        full: bool,
        code: StreamCode,
        /// Exact handle that failed, so a delayed notice cannot close its replacement.
        stream: StreamId,
    },
    /// The emergency hot key was recognised; the thread has already stopped capturing and
    /// forwarding. The task closes the outbound streams; the config module gets
    /// the local owner publishes one authenticated emergency event.
    EmergencyPause,
    /// Native permission changed. Reopen I0 so the next opening reflects the input owner.
    Controllable(bool),
    /// A structurally admitted frame had unsupported input semantics.
    ProtocolViolation {
        peer: DeviceId,
        stream: StreamId,
        type_id: Option<u16>,
        code: oink_protocol::v1::ErrorCode,
        datagram: bool,
    },
}

/// Receiver of [`InputNotice`]s for the task.
#[derive(Debug)]
pub struct InputNotices(Arc<super::notices::Notices>);

impl InputNotices {
    pub async fn next(&mut self) -> Option<InputNotice> {
        loop {
            let changed = self.0.changed.notified();
            if let Some(notice) = self.0.take() {
                return Some(notice);
            }
            if self.0.closed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            changed.await;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PeerInput {
    pub peer: DeviceId,
    /// Exact stream identities for ordinary drag work and attachment assembly.
    pub inbound: Option<StreamId>,
    pub outbound: Option<StreamId>,
    /// Number of the fixed five opening records consumed, and detach barrier state.
    pub opening_received: usize,
    pub releasing: bool,
    /// Both streams present, opening read, release complete.
    pub attached: bool,
    /// Attached and the peer said it can be controlled: its screens can be crossed into
    /// (if the router layout snapshot contains them).
    pub online: bool,
    /// Capability from the currently received InputCapability record.
    pub touchpad_contacts: Option<u8>,
    /// The peer's feel settings from its input stream, for the input settings view.
    pub feel: Option<oink_foundation::input::FeelProfile>,
}

/// Snapshot produced by the input thread: focus and input state, for the UI sampling
/// (`SharingState::derive`), the tray and diagnostics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputStatus {
    pub application_focus: Option<oink_routing::application_focus::Snapshot>,
    pub update_held_input: bool,
    pub update_remote_control: bool,
    pub update_locked: bool,
    pub update_suspended: bool,
    pub focus: Option<DeviceId>,
    pub focus_info: Option<oink_routing::FocusInfo>,
    pub group_focus: oink_routing::GroupFocusStatus,
    pub game_mode: bool,
    /// Derived compatibility observation, never independent of keyboard_mode.
    pub keyboard_disabled: bool,
    pub resource_faults: ResourceFaults,
    /// Persistent local enumeration failure only; shared screen faults also include input injection.
    pub display_enumeration_fault: Option<oink_protocol::v1::dto::ResourceFaultCode>,
    /// Cached native Caps behavior; absent until a supported backend observes it.
    pub caps_lock_switches_input_source: Option<bool>,
    pub source_modes_revision: u64,
    pub mouse_mode: oink_foundation::input::InputMode,
    pub keyboard_mode: oink_foundation::input::InputMode,
    pub screen_local_input: bool,
    pub game_status: Option<oink_routing::GameModeStatus>,
    pub pointer_focus: bool,
    /// Any drag phase, including probing and release/handoff. File Provider may start
    /// a new local drag only when this is false and focus is local.
    pub dnd_active: bool,
    /// Exact host still holding Left, used to revoke asynchronous URL preparation.
    pub dnd_host_pending: Option<(oink_foundation::content::ClipId, u64)>,
    pub trail_pinned: bool,
    pub paused: bool,
    /// Safety epoch: every local or received emergency invalidates older resumes.
    pub emergency_sequence: u64,
    /// Local triggers only; received C0 emergencies must never become new group events.
    pub local_emergency_sequence: u64,
    /// Local-only backlog pause publication; never originates a group emergency.
    pub backlog_sequence: u64,
    pub injection_detached: bool,
    pub locked_device: Option<DeviceId>,
    /// Cached effective priority from the last protected Windows helper reply.
    pub helper_priority: Option<i32>,
    pub peers: Vec<PeerInput>,
}

/// Snapshot read by the input thread: what observers on ordinary threads saw of the
/// local environment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Environment {
    pub game_detected: bool,
    pub secure_input: bool,
    pub displays_asleep: bool,
    pub system_sleeping: bool,
}

/// Everything the thread reads. All produced on ordinary threads, replaced whole.
#[derive(Clone, Debug)]
pub struct InputSnapshots {
    pub mode_switch_in_callback: bool,
    pub capture_reports_key_repeat: bool,
    /// Saved source policy installed before capture starts. Later owner edits
    /// arrive atomically with routing settings through ApplyOwnerPreferences.
    pub local_input_transform: oink_foundation::input_transform::InputTransform,
    /// Compiled by the config module. When a new one no longer contains the device that
    /// has focus, the thread takes focus back.
    pub layout: Arc<Snapshot<RouterLayout>>,
    /// Runtime routing settings; source key mappings are installed separately.
    pub settings: Arc<Snapshot<RoutingSettings>>,
    pub environment: Arc<Snapshot<Environment>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerInputApplied {
    pub cleanup_pending: bool,
    /// Safety epoch: every local or received emergency invalidates older resumes.
    pub emergency_sequence: u64,
    /// Local triggers only; received C0 emergencies must never become new group events.
    pub local_emergency_sequence: u64,
}
pub type OwnerApplyReply =
    std::sync::mpsc::SyncSender<Result<OwnerInputApplied, oink_protocol::v1::dto::ControlFailure>>;
pub use faults::ResourceFaults;
mod commands;
mod drag;
mod driver;
pub(crate) mod faults;
mod rate_budget;
mod trail;

struct Shared {
    commands: commands::Commands,
    status: Snapshot<InputStatus>,
    status_changed: tokio::sync::Notify,
    scheduler: Snapshot<oink_network::link::SchedulerStatus>,
    opening: Snapshot<Vec<oink_routing::Packet>>,
    wake: std::sync::OnceLock<oink_network::link::Wake>,
    stopped: std::sync::atomic::AtomicBool,
    backlog_requested: std::sync::atomic::AtomicBool,
    done: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    jitter: Arc<oink_input::jitter::Counters>,
}

/// Handle to the native capture thread. Commands and snapshot reads never take the
/// router's ownership away from that thread.
pub struct InputThread {
    source_start: oink_clipboard::dnd::SourceStart,
    #[cfg(debug_assertions)]
    source_motion_for_test: Arc<std::sync::atomic::AtomicU64>,
    shared: Arc<Shared>,
    backend: Arc<dyn oink_input::InputBackend>,
    inject_scheduler: Arc<Snapshot<oink_network::link::SchedulerStatus>>,
    inject_failures: Arc<std::sync::atomic::AtomicU64>,
    inject_timing: Arc<oink_input::timing::InjectionTiming>,
}
impl std::fmt::Debug for InputThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputThread").finish_non_exhaustive()
    }
}
impl InputThread {
    pub(crate) fn report_input_backlog(&self) {
        self.shared
            .backlog_requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.backend.set_input_paused(true);
        if let Some(wake) = self.shared.wake.get() {
            wake();
        }
    }

    pub fn start(
        boot: oink_foundation::session::Boot,
        snapshots: InputSnapshots,
        inject: InjectHandle,
        main: crate::threads::MainSender,
    ) -> std::io::Result<(Self, InputNotices)> {
        let local = boot.device_id;
        let source_start = inject.source_start();
        #[cfg(debug_assertions)]
        let source_motion_for_test = inject.source_motion_counter_for_test();
        let backend = inject.backend();
        let inject_scheduler = inject.scheduler_snapshot();
        let inject_failures = inject.failure_counter();
        let inject_timing = inject.timing();
        let (done, received) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            commands: commands::Commands::new(),
            status: Snapshot::new(InputStatus {
                focus: Some(local),
                ..Default::default()
            }),
            status_changed: tokio::sync::Notify::new(),
            scheduler: Snapshot::default(),
            opening: Snapshot::new(Vec::new()),
            wake: std::sync::OnceLock::new(),
            stopped: std::sync::atomic::AtomicBool::new(false),
            backlog_requested: std::sync::atomic::AtomicBool::new(false),
            done: std::sync::Mutex::new(received),
            jitter: Arc::default(),
        });
        let notices = super::notices::Notices::new();
        let owner = driver::Driver::new(boot, snapshots, inject, main, shared.clone(), notices.clone(), done);
        backend.start_loop(Box::new(owner)).map_err(std::io::Error::other)?;
        Ok((
            Self {
                source_start,
                #[cfg(debug_assertions)]
                source_motion_for_test,
                shared,
                backend,
                inject_scheduler,
                inject_failures,
                inject_timing,
            },
            InputNotices(notices),
        ))
    }
    pub fn send(&self, command: InputCommand) {
        if self.shared.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.shared.commands.push(command);
        self.wake()();
    }
    pub(crate) fn source_start(&self) -> oink_clipboard::dnd::SourceStart {
        self.source_start.clone()
    }
    /// Drained by the ordinary input-module task, at most once per five seconds.
    pub(crate) fn report_command_rejections(&self) {
        let count = self.shared.commands.take_rejected();
        if count != 0 {
            tracing::warn!(count, "input commands rejected at bounded admission");
        }
    }
    pub(crate) async fn status_changed(&self) {
        self.shared.status_changed.notified().await;
    }
    pub fn status(&self) -> Arc<InputStatus> {
        self.shared.status.load()
    }
    #[cfg(all(test, debug_assertions, target_os = "macos"))]
    pub(crate) fn pending_drag_for_test(&self, id: oink_foundation::content::ClipId, epoch: u64) {
        let mut status = (*self.status()).clone();
        status.dnd_host_pending = Some((id, epoch));
        self.shared.status.publish(status);
    }

    /// Only a failing watchdog asks the owner for its full DND state. A stalled
    /// owner must not stall the watchdog as well; the published status remains
    /// available even when this bounded query cannot be answered.
    #[cfg(debug_assertions)]
    pub(crate) fn dnd_watchdog_snapshot_for_test(&self) -> String {
        let (reply, receive) = std::sync::mpsc::sync_channel(1);
        self.send(InputCommand::DndWatchdogSnapshotForTest(reply));
        receive
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap_or_else(|error| {
                format!(
                    "router_dnd unavailable: {error}; owner stopped={}",
                    self.shared.stopped.load(std::sync::atomic::Ordering::Acquire)
                )
            })
    }

    /// Observe receipt of discarded motion while a test holds native source press.
    #[cfg(debug_assertions)]
    pub fn source_motion_discarded_for_test(&self) -> u64 {
        self.source_motion_for_test.load(std::sync::atomic::Ordering::Acquire) & !(1 << 63)
    }
    pub fn touchpad_contacts(&self) -> Option<u8> {
        self.backend.touchpad_contacts()
    }
    pub fn opening_state(&self, controllable: bool) -> Vec<oink_routing::Packet> {
        let mut opening = (*self.shared.opening.load()).clone();
        if let Some(first) = opening.first_mut() {
            *first = oink_routing::Packet::Control(oink_routing::control::ControlMsg::InputCapability {
                controllable,
                touchpad_contacts: self.touchpad_contacts(),
            });
        }
        opening
    }
    pub fn scheduler_status(&self) -> Arc<oink_network::link::SchedulerStatus> {
        self.shared.scheduler.load()
    }
    pub fn injection_failures(&self) -> u64 {
        self.inject_failures.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn take_injection_timing(&self, now: std::time::Instant) -> oink_input::timing::Sample {
        let mut sample = self.inject_timing.take(now);
        sample.jitter = self.shared.jitter.take();
        sample
    }
    pub fn injection_scheduler_status(&self) -> Arc<oink_network::link::SchedulerStatus> {
        self.inject_scheduler.load()
    }
    pub fn wake(&self) -> oink_network::link::Wake {
        self.shared
            .wake
            .get()
            .expect("capture startup installed its wake source")
            .clone()
    }
    /// Called on an ordinary thread during orderly shutdown. The native loop continues
    /// dispatching mode barriers until injection has drained; only then capture ends.
    pub fn stop(&self) {
        if !self.shared.stopped.load(std::sync::atomic::Ordering::Acquire) {
            self.send(InputCommand::Stop);
            let _ = self.shared.done.lock().unwrap_or_else(|e| e.into_inner()).recv();
        }
        self.backend.stop();
    }
}
impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop();
    }
}
