//! Backend thread ownership and the queues, snapshots and commands between threads.
//!
//! | Thread | Tier | Owns exclusively |
//! |---|---|---|
//! | input ([`input`]) | realtime | capture; the router (focus, held keys, game mode, edge crossing); motion pacing and its timer; both ends of I0 and I1 |
//! | injection ([`inject`]) | realtime | native injection (`CGEventPost`, `SendInput`, the pipe to the Windows input service helper); its record of injected keys |
//! | network (`oink_network::link`) | elevated or realtime | the QUIC endpoint, all links and streams, heartbeat, cut-over, relay |
//! | audio (`oink-audio`) | realtime | capture and encode, decode and mix; both ends of M1 |
//! | main | ordinary | the application loop; native windows; drag-and-drop system calls; on macOS the menu bar item |
//! | ordinary pool | ordinary | config module; every module's stream task; clipboard and files; screen identification; mini-game; diagnostics; logs; IPC service; low-priority TCP I/O |
//!
//! Between threads there are exactly three things: one queue per stream per end; read-only
//! snapshots replaced whole (`oink_foundation::snapshot::Snapshot`); small fixed-size
//! commands. A realtime thread never waits for an ordinary one: no shared lock, no file
//! I/O, no log formatting. Scheduling is set through `oink_foundation::realtime`.

pub mod inject;
pub mod input;
mod main_queue;
mod notices;
pub use main_queue::{MainReceiver, MainSender, main_channel};

use oink_routing::{DndCmd, Output};

/// Input thread → main thread. Fixed size, never awaited: the input thread goes on with
/// the next event.
#[derive(Debug)]
pub enum MainCommand {
    ApplicationFocus {
        command: oink_routing::ApplicationFocusCommand,
        permit: Option<oink_input::application_focus::Permit>,
    },
    InputStatusChanged,
    /// Ordinary shutdown nudge; trace queues use the shared park notification.
    Wake,
    /// Drag-and-drop system calls (begin, cancel, …). Results return to the input thread
    /// as `InputCommand::DndEvent`.
    Dnd {
        command: DndCmd,
        completion: inject::DndCompletion,
        safety: oink_input::session_gate::InputSafety,
        report: Option<crate::telemetry::Stamp>,
    },
    /// Optional artwork never acquires a pointer/injection completion fence.
    DndArtwork(DndCmd),
    DndPreview {
        peer: oink_foundation::DeviceId,
        stream: oink_network::link::StreamId,
        message: oink_routing::dnd::DndMsg,
    },
    /// Cursor decoration: the router's trail outputs, unchanged.
    Decoration(Output),
    TrailDuration(u32),
    TrailSuppressed(bool),
    TrailPointer {
        head: oink_foundation::Point,
        visible: oink_foundation::Point,
        clip: Option<oink_foundation::Rect>,
    },
    /// Ordinary-thread OS activity, never performed by injection. None is local
    /// physical input (A1): declare it even if group display wake is disabled.
    /// Some(peer) is group activity and obeys the group's wake/pause setting.
    Activity {
        from: Option<oink_foundation::DeviceId>,
    },
    ResetUserIdle,
    DismissOverlay(u64),
    IdentifyScreens(u64),
    Media {
        event: oink_foundation::input::InjectEvent,
        work: inject::OrdinaryInput,
    },
    ExitVolumeKey {
        work: inject::OrdinaryInput,
        source: oink_foundation::session::Boot,
        route: oink_routing::input::VolumeRoute,
        press_id: u64,
        code: oink_foundation::input::KeyCode,
        pressed: bool,
        repeat: bool,
        modifiers: u8,
    },
    RoutingStatus {
        status: oink_routing::StatusEvent,
        work: Option<inject::OrdinaryInput>,
    },
    ResumeSharing {
        epoch: u64,
    },
    /// Ordinary diagnostics consumes numeric records; formatting never runs on the native owner.
    Realtime(RealtimeRecord),
    Low {
        peer: oink_foundation::DeviceId,
        stream: oink_network::link::StreamId,
        message: oink_protocol::v1::Message,
    },
    Attachment {
        peer: oink_foundation::DeviceId,
        stream: oink_network::link::StreamId,
        message: oink_protocol::v1::Message,
    },
}

/// A fixed-size numeric record from a realtime thread to diagnostics. Formatting and
/// writing happen on ordinary threads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RealtimeRecord {
    pub source: oink_foundation::realtime::ThreadRole,
    pub code: u16,
    pub at_us: u64,
    pub values: [f64; 4],
}

/// Values: late rounds, maximum lateness in microseconds, over-budget rounds, maximum turn microseconds.
pub const SCHEDULER_TOTALS: u16 = 1;
/// Values: read-back present (0/1), native priority, tier (1 elevated / 2 realtime), demotions.
pub const SCHEDULER_POLICY: u16 = 2;

/// Ordinary consumer of `MainCommand::Activity`. Network activity accounting is
/// separate: the originating machine must also keep its OS awake while its physical
/// events are swallowed for forwarding. Only remote activity obeys group wake policy.
pub fn declare_activity(
    backend: &dyn oink_input::InputBackend,
    from: Option<oink_foundation::DeviceId>,
    group_allowed: bool,
) -> Result<bool, oink_input::InputError> {
    if from.is_some() && !group_allowed {
        return Ok(false);
    }
    backend.declare_user_activity()
}
