//! The phone's embedded router (protocol §14): the same `oink_routing::Router`
//! as on a desktop, configured as a screenless member. It runs on its own
//! thread; it is the network's `MobileInputSink` (I0 messages, link changes,
//! closed I0s, emergency pauses) and `InputOpening` (the records each new
//! outbound I0 starts with). Control-page events are queued to it too, and its
//! outputs go straight back to I0 (`InputNetwork`), to the control page
//! (`ControlObserver`) and, on a privileged Android phone, to the input helper
//! (`ScreenSink`) without any host polling.
pub use crate::trackpad::FingerPhase;
use crate::trackpad::{self, Curve, REPORT_PERIOD, Scroller};
use oink_foundation::{
    DeviceId,
    contacts::{Contact, MAX_CONTACTS, Sample, Surface},
    display::DisplayInfo,
    input::{InjectEvent, KeyCode, MouseButton},
    session::Boot,
};
use oink_network::link::{LinkId, LinkState, LinkTable};
use oink_protocol::v2::{self, dto, frame::OpaqueFrame};
use oink_routing::{
    GameMode, MemberInput, Output, Packet, Router, RouterConfig, StatusEvent,
    adapter::{Adapter, PeerCaps},
    control::{ControlMsg, EdgeSide, EdgeState},
    wire,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// What the router needs from the phone's network.
pub trait InputNetwork: Send + Sync + 'static {
    fn links(&self) -> Arc<LinkTable>;
    fn peer_facts(&self, peer: DeviceId) -> Option<dto::DeviceFacts>;
    fn layout(&self) -> Option<dto::LayoutValue> {
        None
    }
    fn pointer_base(&self) -> Option<DeviceId> {
        None
    }
    fn send_input(&self, peer: DeviceId, message: v2::Message) -> Result<(), String>;
    /// The group's speaker exit as this phone's volume-key context: the
    /// control page's volume and mute keys go there instead of to the focus
    /// computer. Read on the router thread at each new press.
    fn volume_exit(&self) -> Option<oink_routing::input::VolumeRoute> {
        None
    }
    /// A volume key this phone executes as the speaker exit.
    fn exit_volume_key(&self, _key: ExitVolumeKey) {}
}

/// A volume key this phone executes as the speaker exit: sent by a focus
/// computer that forwards its audio here, or pressed on this phone's own
/// control page while it is the exit. The media owner validates the pinned
/// context against its exit volume, as a desktop exit's audio owner does.
#[derive(Clone, Debug, PartialEq)]
pub struct ExitVolumeKey {
    pub source: Boot,
    pub route: oink_routing::input::VolumeRoute,
    pub press_id: u64,
    pub code: KeyCode,
    pub pressed: bool,
    pub repeat: bool,
    /// HID modifier bitmap (E0–E7).
    pub modifiers: u8,
}

impl InputNetwork for crate::MobileNetwork {
    fn pointer_base(&self) -> Option<DeviceId> {
        self.input_pointer_base()
    }
    fn layout(&self) -> Option<dto::LayoutValue> {
        self.input_layout()
    }
    fn links(&self) -> Arc<LinkTable> {
        crate::MobileNetwork::links(self)
    }
    fn peer_facts(&self, peer: DeviceId) -> Option<dto::DeviceFacts> {
        crate::MobileNetwork::peer_facts(self, peer)
    }
    fn send_input(&self, peer: DeviceId, message: v2::Message) -> Result<(), String> {
        self.send_message(peer, message).map_err(|error| error.to_string())
    }
    fn volume_exit(&self) -> Option<oink_routing::input::VolumeRoute> {
        crate::MobileNetwork::volume_exit(self)
    }
    fn exit_volume_key(&self, key: ExitVolumeKey) {
        self.push_exit_volume_key(key);
    }
}

/// One device the control page can select.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlTarget {
    pub device: DeviceId,
    pub name: String,
    /// "macos", "windows", "android" or the peer's own OS string.
    pub platform: String,
    pub online: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlState {
    pub focus_device: Option<DeviceId>,
    pub selected_device: Option<DeviceId>,
    /// The latest input was dropped: no focus to join and nothing selected.
    pub no_target: bool,
    pub locked: bool,
    pub game_mode: GameMode,
    pub game_active: bool,
    pub targets: Vec<ControlTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlEdge {
    pub state: EdgeState,
    pub edge: EdgeSide,
    pub to_device: Option<DeviceId>,
}

pub trait ControlObserver: Send + Sync {
    fn state(&self, state: ControlState);
    fn edge(&self, edge: ControlEdge);
    /// The focus holder did not inject a text sent with `input_text`.
    fn text_refused(&self, reason: oink_routing::control::TextRefusal);
    /// The text-input focus of `device`, the focus holder this phone's input
    /// goes to (protocol §14.7): on every change, and the current state when
    /// the control page opens (an observer is set). The page pops its
    /// keyboard up or hides it (control-page spec §9).
    fn text_focus(&self, device: DeviceId, state: oink_routing::control::TextFocusState);
}

/// A privileged Android phone's own screen (the input helper).
pub trait ScreenSink: Send + Sync {
    /// Focus entered (`true`, with the entry point) or left this screen.
    fn focus(&self, active: bool);
    fn inject(&self, event: InjectEvent);
    /// Release everything the helper holds (emergency pause).
    fn release_all(&self);
    /// Fresh real group input, independent of focus. False revokes the lease.
    fn group_activity(&self, _active: bool) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// No focus holder to act on.
    NoTarget,
    Paused,
    InvalidText,
    /// The I0 send queue did not take the text; nothing of it was sent.
    SendFailed,
    Stopped,
}

/// Control-page tuning (protocol §14.6).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControlSettings {
    /// 1..=10: the Mac trackpad's tracking-speed step (default 4, macOS's
    /// default 0.6875).
    pub pointer_speed: u8,
    /// Host coordinate units per millimetre.
    pub points_per_mm: f32,
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            pointer_speed: crate::trackpad::DEFAULT_STEP,
            points_per_mm: 6.3,
        }
    }
}

/// One finger on the control page's touchpad, in millimetres from the pad's
/// top left. `id` is the phone's touch id (any value, stable while touching).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PadTouch {
    pub id: u64,
    pub x: f32,
    pub y: f32,
}

/// Every finger on the pad after one touch event, as a Mac trackpad source
/// captures them (raw contacts, protocol §14.6).
#[derive(Clone, Debug, PartialEq)]
pub struct PadTouches {
    /// Touch time, phone uptime in microseconds.
    pub time_us: u64,
    /// The pad's size in millimetres.
    pub width: f32,
    pub height: f32,
    pub touches: Vec<PadTouch>,
    /// The system took the touches away: the current multi-finger touch is
    /// cancelled, not ended.
    pub cancelled: bool,
}

enum Command {
    ReadFocus(mpsc::SyncSender<Option<DeviceId>>),
    Message {
        peer: Boot,
        link: LinkId,
        message: v2::Message,
        /// The emergency generation when it was received.
        generation: u64,
    },
    Emergency,
    LinksChanged,
    InputClosed(DeviceId),
    FactsChanged(DeviceId),
    LayoutChanged,
    Member(MemberInput),
    Pointer {
        dx: f32,
        dy: f32,
        /// Seconds the movement took, at least one report period.
        dt: f64,
    },
    Scroll {
        phase: FingerPhase,
        dx: f32,
        dy: f32,
        time_us: u64,
        since_us: u64,
    },
    /// A scroll in millimetres (the shared touchpad recognizer's).
    ScrollMm {
        phase: FingerPhase,
        dx: f64,
        dy: f64,
        time_us: u64,
        since_us: u64,
    },
    Touches(PadTouches),
    Settings(ControlSettings),
    Select(DeviceId, mpsc::SyncSender<Result<(), ControlError>>),
    Lock(bool, mpsc::SyncSender<Result<(), ControlError>>),
    LockTo(DeviceId, mpsc::SyncSender<Result<(), ControlError>>),
    Game(GameMode, mpsc::SyncSender<Result<(), ControlError>>),
    Text(String, mpsc::SyncSender<Result<(), ControlError>>),
    Observer(Option<Arc<dyn ControlObserver>>),
    Screen(Option<(DisplayInfo, Arc<dyn ScreenSink>)>),
    Stop,
}

/// Handle to the router thread. Dropping the last handle stops it.
pub struct InputRouter {
    tx: mpsc::Sender<Command>,
    opening: Arc<Mutex<Vec<v2::Message>>>,
    locked: Arc<Mutex<Option<DeviceId>>>,
    /// Counts emergency pauses. Input received under an older count and not
    /// yet injected is dropped when the router reaches it.
    generation: Arc<AtomicU64>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for InputRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputRouter").finish_non_exhaustive()
    }
}

impl InputRouter {
    pub fn start(local: Boot, network: Arc<dyn InputNetwork>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let opening = Arc::new(Mutex::new(Vec::new()));
        let locked = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let mut worker = Worker::new(local, network, opening.clone(), locked.clone(), generation.clone());
        let thread = std::thread::Builder::new()
            .name("oink-input-router".into())
            .spawn(move || worker.run(rx))
            .expect("input router thread");
        Arc::new(Self {
            tx,
            opening,
            locked,
            generation,
            thread: Mutex::new(Some(thread)),
        })
    }

    fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }

    fn call(
        &self,
        make: impl FnOnce(mpsc::SyncSender<Result<(), ControlError>>) -> Command,
    ) -> Result<(), ControlError> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(make(reply));
        result.recv().unwrap_or(Err(ControlError::Stopped))
    }

    /// One decoded I0 message from the admitted link of `peer`.
    pub fn message(&self, peer: Boot, link: LinkId, message: v2::Message) {
        self.send(Command::Message {
            peer,
            link,
            message,
            generation: self.generation.load(Ordering::Acquire),
        });
    }
    /// A member's emergency pause: input received before it and not yet
    /// injected is dropped, then everything held is released.
    pub fn emergency_pause(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.send(Command::Emergency);
    }
    /// A link became Ready or was lost.
    pub fn links_changed(&self) {
        self.send(Command::LinksChanged);
    }
    /// The I0 pair with `peer` closed (e.g. it paused sharing) while the link
    /// stays: it is no longer a target; what this phone held there is released
    /// and a focus there is dropped. A reopened I0 attaches again from its
    /// opening records.
    pub fn input_closed(&self, peer: DeviceId) {
        self.send(Command::InputClosed(peer));
    }
    /// `peer`'s DeviceFacts changed: its OS, screens and key mapping are read again.
    pub fn facts_changed(&self, peer: DeviceId) {
        self.send(Command::FactsChanged(peer));
    }
    /// The records a newly opened I0 starts with (protocol §14.1);
    /// `controllable` comes from the network (helper ready, including asleep/locked).
    pub fn opening(&self, controllable: bool) -> Vec<v2::Message> {
        let mut records = self.opening.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(first) = records.first_mut() {
            *first = capability(controllable);
        }
        records
    }
    pub fn set_observer(&self, observer: Option<Arc<dyn ControlObserver>>) {
        self.send(Command::Observer(observer));
    }
    /// The privileged helper's screen; `None` when the helper is not ready.
    pub fn set_screen(&self, screen: Option<(DisplayInfo, Arc<dyn ScreenSink>)>) {
        self.send(Command::Screen(screen));
    }
    pub fn set_settings(&self, settings: ControlSettings) {
        self.send(Command::Settings(settings));
    }
    /// Finger movement in host units; converted to pointer units here. `time_us` is the touch
    /// timestamp of this sample and `since_us` the one the movement is measured from: the
    /// previous pointer sample of the touch, or the touch-down for its first movement (phone
    /// clock, microseconds). The phone sends one combined movement per input event.
    pub fn pointer_move(&self, dx: f32, dy: f32, time_us: u64, since_us: u64) {
        self.send(Command::Pointer {
            dx,
            dy,
            dt: pointer_interval(time_us, since_us),
        });
    }
    pub fn button(&self, button: MouseButton, down: bool) {
        self.send(Command::Member(MemberInput::Button { button, pressed: down }));
    }
    /// One touchpad scroll event of the fingers, in host units, finger
    /// direction, between `since_us` and `time_us` (as for `pointer_move`).
    /// Accelerated like a Mac trackpad's scroll and sent with its phase; a
    /// lift (Ended) starts momentum, which this router generates.
    pub fn scroll(&self, phase: FingerPhase, dx: f32, dy: f32, time_us: u64, since_us: u64) {
        self.send(Command::Scroll {
            phase,
            dx,
            dy,
            time_us,
            since_us,
        });
    }
    /// A two-finger scroll event of the shared touchpad recognizer
    /// (`trackpad::gestures`): millimetres, finger direction, timed like
    /// [`Self::scroll`].
    pub fn scroll_mm(&self, phase: FingerPhase, dx: f64, dy: f64, time_us: u64, since_us: u64) {
        self.send(Command::ScrollMm {
            phase,
            dx,
            dy,
            time_us,
            since_us,
        });
    }
    /// Every finger on the pad after a touch event (also when the last one
    /// lifts): raw contacts for targets that recognise gestures themselves,
    /// and a new touch ends running momentum.
    pub fn touches(&self, touches: PadTouches) {
        self.send(Command::Touches(touches));
    }
    /// A recognised touchpad gesture (Dock swipe, magnify, rotate, smart
    /// magnify), sent as a Mac trackpad source sends it.
    pub fn gesture(&self, gesture: oink_foundation::input::Gesture) {
        self.send(Command::Member(MemberInput::Gesture(gesture)));
    }
    pub fn key(&self, code: u32, down: bool) {
        self.send(Command::Member(MemberInput::Key {
            code: KeyCode(code),
            pressed: down,
        }));
    }
    /// One committed text, at most one `InputText` (4096 UTF-8 bytes), so it
    /// is sent whole or not at all and a retry never types part of it twice.
    pub fn input_text(&self, text: String) -> Result<(), ControlError> {
        if text.is_empty() || text.len() > dto::INPUT_TEXT_MAX_BYTES || text.chars().any(char::is_control) {
            return Err(ControlError::InvalidText);
        }
        self.call(|reply| Command::Text(text, reply))
    }
    pub fn move_pointer_to(&self, device: DeviceId) -> Result<(), ControlError> {
        self.call(|reply| Command::Select(device, reply))
    }
    pub fn set_focus_lock(&self, locked: bool) -> Result<(), ControlError> {
        self.call(|reply| Command::Lock(locked, reply))
    }
    /// The device the group pointer is locked to, as this phone's session knows it.
    /// Read-only snapshot; choosing a transfer target never changes input focus.
    pub fn focused_device(&self) -> Option<DeviceId> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::ReadFocus(reply));
        result.recv().ok().flatten()
    }

    pub fn locked_device(&self) -> Option<DeviceId> {
        *self.locked.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Lock the group pointer to `device` (the device sheet's pointer lock).
    pub fn lock_to(&self, device: DeviceId) -> Result<(), ControlError> {
        self.call(|reply| Command::LockTo(device, reply))
    }
    pub fn set_game_mode(&self, mode: GameMode) -> Result<(), ControlError> {
        self.call(|reply| Command::Game(mode, reply))
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
        if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = thread.join();
        }
    }
}

impl crate::MobileInputSink for InputRouter {
    fn layout_changed(&self) {
        self.send(Command::LayoutChanged);
    }

    fn links_changed(&self) {
        InputRouter::links_changed(self);
    }
    fn message(&self, peer: Boot, link: LinkId, message: v2::Message) {
        InputRouter::message(self, peer, link, message);
    }
    fn input_closed(&self, peer: DeviceId) {
        InputRouter::input_closed(self, peer);
    }
    fn facts_changed(&self, peer: DeviceId) {
        InputRouter::facts_changed(self, peer);
    }
    fn emergency_pause(&self) {
        InputRouter::emergency_pause(self);
    }
}

impl crate::InputOpening for InputRouter {
    fn opening(&self, _local: Boot, controllable: bool) -> Vec<v2::Message> {
        InputRouter::opening(self, controllable)
    }
}

impl Drop for InputRouter {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
    }
}

fn capability(controllable: bool) -> v2::Message {
    wire::control::encode(&ControlMsg::InputCapability {
        controllable,
        touchpad_contacts: None,
    })
    .expect("fixed capability record")
}

/// Seconds between two touch timestamps (microseconds), at least one report
/// period (as macOS's pointer accelerator counts it, so movements that arrive
/// close together cannot make the finger speed explode); a timestamp that runs
/// backwards counts as one period too.
fn pointer_interval(time_us: u64, since_us: u64) -> f64 {
    (time_us.saturating_sub(since_us) as f64 / 1e6).max(REPORT_PERIOD)
}

/// Host units → millimetres.
fn millimetres(settings: &ControlSettings, d: f32) -> f64 {
    f64::from(d) / f64::from(settings.points_per_mm.max(0.1))
}

/// Finger movement → millimetres → the Mac trackpad's pointer acceleration at
/// the chosen tracking-speed step → pointer units (points of the pointer base
/// computer), the movement having taken `dt` seconds.
fn pointer_units(settings: &ControlSettings, dx: f32, dy: f32, dt: f64) -> (f64, f64) {
    trackpad::pointer_points(
        &Curve::pointer(settings.pointer_speed),
        millimetres(settings, dx),
        millimetres(settings, dy),
        dt,
    )
}

/// The touches as a trackpad contacts sample (contacts in 0.01 mm). More than
/// five fingers cannot be described: an invalid sample cancels the touch.
fn contacts_sample(touches: &PadTouches) -> Sample {
    let hundredths = |mm: f32| (f64::from(mm) * 100.).round().clamp(0., f64::from(u32::MAX)) as u32;
    let surface = Surface {
        width: hundredths(touches.width),
        height: hundredths(touches.height),
    };
    let mut sample = Sample {
        // One touchpad, never zero.
        device: 1,
        time_us: touches.time_us,
        surface,
        len: 0,
        contacts: Default::default(),
    };
    if touches.cancelled || touches.touches.len() > MAX_CONTACTS {
        sample.len = u8::MAX;
        return sample;
    }
    for (slot, touch) in sample.contacts.iter_mut().zip(&touches.touches) {
        *slot = Contact {
            // Contact ids are non-zero; the phone's ids are any value.
            id: (touch.id as u32).wrapping_add(1).max(1),
            x: hundredths(touch.x).min(surface.width) as i32,
            y: hundredths(touch.y).min(surface.height) as i32,
        };
    }
    sample.len = touches.touches.len() as u8;
    sample
}

#[derive(PartialEq)]
struct LayoutInputs {
    geometry: dto::LayoutValue,
    facts: std::collections::BTreeMap<DeviceId, v2::RawMap>,
    online: std::collections::BTreeSet<DeviceId>,
    pointer_base: Option<DeviceId>,
}

struct Worker {
    adapter: Adapter,
    layout_inputs: Option<LayoutInputs>,
    network: Arc<dyn InputNetwork>,
    opening: Arc<Mutex<Vec<v2::Message>>>,
    locked: Arc<Mutex<Option<DeviceId>>>,
    generation: Arc<AtomicU64>,
    observer: Option<Arc<dyn ControlObserver>>,
    screen: Option<Arc<dyn ScreenSink>>,
    links: std::collections::HashMap<DeviceId, (LinkId, Boot)>,
    settings: ControlSettings,
    /// Touchpad scrolling as a Mac trackpad does it: acceleration and momentum.
    scroller: Scroller,
    /// When the next momentum frame is due.
    momentum_due: Option<Instant>,
    /// A finger was on the pad at the last touch event.
    touching: bool,
    no_target: bool,
    published: Option<ControlState>,
    start: Instant,
    /// The I0 self-healing records (FocusState, GameMode, HeldSync) go out
    /// every 2 s, as the desktop's network heartbeat writes them (routing
    /// §5.11): a lost release is repaired, e.g. during a locked drag.
    next_heartbeat: Instant,
    // Per admitted I0 source: heartbeats/focus snapshots never renew these leases.
    group_activity: std::collections::HashMap<DeviceId, Instant>,
}

const HEARTBEAT: Duration = Duration::from_secs(2);
// Real input pulses are throttled to 1 Hz. Three seconds tolerates two missing
// pulses, yet bounds stale/failed senders without treating focus as activity.
const GROUP_ACTIVITY_FRESH: Duration = Duration::from_secs(3);

impl Worker {
    fn new(
        local: Boot,
        network: Arc<dyn InputNetwork>,
        opening: Arc<Mutex<Vec<v2::Message>>>,
        locked: Arc<Mutex<Option<DeviceId>>>,
        generation: Arc<AtomicU64>,
    ) -> Self {
        let os = if cfg!(target_os = "android") {
            oink_routing::Os::Android
        } else {
            oink_routing::Os::Other
        };
        let mut config = RouterConfig::new(local, "", os);
        config.screenless = true;
        let mut router = Router::new(config, Arc::new(oink_routing::SimpleLayout::new()));
        router.set_game_mode(GameMode::Off);
        let exit = network.clone();
        router.set_volume_exit_probe(Arc::new(move || exit.volume_exit()));
        router.drain_outputs();
        let mut worker = Self {
            adapter: Adapter::new(router),
            layout_inputs: None,
            network,
            opening,
            locked,
            generation,
            observer: None,
            screen: None,
            links: Default::default(),
            settings: ControlSettings::default(),
            scroller: Scroller::default(),
            momentum_due: None,
            touching: false,
            no_target: false,
            published: None,
            start: Instant::now(),
            next_heartbeat: Instant::now() + HEARTBEAT,
            group_activity: Default::default(),
        };
        worker.refresh_opening();
        worker
    }

    fn now(&self) -> oink_routing::Micros {
        self.start.elapsed().as_micros() as oink_routing::Micros
    }

    fn run(&mut self, rx: mpsc::Receiver<Command>) {
        loop {
            let now = self.now();
            let heartbeat = self.next_heartbeat.saturating_duration_since(Instant::now());
            let wait = self
                .adapter
                .router()
                .next_deadline()
                .map(|due| Duration::from_micros(due.saturating_sub(now)))
                .map_or(heartbeat, |due| due.min(heartbeat));
            let wait = self
                .group_activity
                .values()
                .min()
                .map_or(wait, |due| wait.min(due.saturating_duration_since(Instant::now())));
            let wait = self
                .momentum_due
                .map_or(wait, |due| wait.min(due.saturating_duration_since(Instant::now())));
            match rx.recv_timeout(wait) {
                Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let now = self.now();
                    self.adapter.router_mut().member_release_all(now);
                    let _ = self.flush();
                    self.clear_group_activity();
                    return;
                }
                Ok(command) => self.command(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let now = self.now();
            self.adapter.at(now);
            self.adapter.router_mut().tick(now);
            self.momentum_tick(now);
            self.expire_group_activity();
            if Instant::now() >= self.next_heartbeat {
                self.next_heartbeat = Instant::now() + HEARTBEAT;
                self.heartbeat();
            }
            let _ = self.flush();
        }
    }

    fn command(&mut self, command: Command) {
        let now = self.now();
        self.adapter.at(now);
        match command {
            Command::ReadFocus(reply) => {
                let _ = reply.send(self.adapter.router().member_target());
            }
            Command::Message {
                peer,
                link,
                message,
                generation,
            } => {
                let stale = generation != self.generation.load(Ordering::Acquire);
                self.receive(peer, link, message, stale);
            }
            Command::Emergency => {
                self.scroller.reset();
                self.momentum_due = None;
                self.adapter.router_mut().member_emergency(now);
                // The releases are injected before the helper drops the rest.
                let _ = self.flush();
                if let Some(screen) = &self.screen {
                    screen.release_all();
                }
                self.clear_group_activity();
            }
            Command::LinksChanged => self.sync_links(),
            Command::InputClosed(peer) => {
                self.remove_group_activity(peer);
                self.adapter.detach(peer);
                self.adapter.clear_received(peer);
                self.refresh_layout();
            }
            Command::FactsChanged(peer) => {
                self.apply_facts(peer);
                self.refresh_layout();
            }
            Command::LayoutChanged => self.refresh_layout(),
            Command::Member(input) => {
                self.no_target = false;
                self.adapter.router_mut().member_input(input, now);
            }
            Command::Pointer { dx, dy, dt } => {
                let (x, y) = pointer_units(&self.settings, dx, dy, dt);
                self.no_target = false;
                self.adapter
                    .router_mut()
                    .member_input(MemberInput::Motion { dx: x, dy: y }, now);
            }
            Command::Scroll {
                phase,
                dx,
                dy,
                time_us,
                since_us,
            } => {
                let (dx, dy) = (millimetres(&self.settings, dx), millimetres(&self.settings, dy));
                self.finger_scroll(phase, dx, dy, time_us, since_us, now);
            }
            Command::ScrollMm {
                phase,
                dx,
                dy,
                time_us,
                since_us,
            } => self.finger_scroll(phase, dx, dy, time_us, since_us, now),
            Command::Touches(touches) => {
                let touching = !touches.touches.is_empty() && !touches.cancelled;
                if touching && !self.touching {
                    // A finger on the pad stops the glide, as on a Mac.
                    if let Some(end) = self.scroller.touch() {
                        self.scroll_out(end, now);
                    }
                    self.momentum_due = None;
                }
                self.touching = touching;
                let sample = contacts_sample(&touches);
                let router = self.adapter.router_mut();
                router.member_input(MemberInput::Contacts(sample), now);
                if !sample.valid() {
                    // The invalid sample cancels the touch; this empty one
                    // re-arms contacts for the next touch.
                    router.member_input(MemberInput::Contacts(Sample { len: 0, ..sample }), now);
                }
            }
            Command::Settings(settings) => self.settings = settings,
            Command::Select(device, reply) => {
                let router = self.adapter.router_mut();
                router.set_selected_target(Some(device));
                let result = router.move_pointer_to(device).map_err(focus_error);
                if let Err(error) = result {
                    // Detection only: why a listed device could not take the pointer.
                    tracing::info!(
                        ?device,
                        ?error,
                        attached = self.adapter.attached(device),
                        facts = self.network.peer_facts(device).is_some(),
                        "control page: the selected device cannot take the pointer"
                    );
                }
                let _ = reply.send(result);
            }
            Command::Lock(locked, reply) => {
                let _ = reply.send(self.adapter.router_mut().member_set_locked(locked).map_err(focus_error));
            }
            Command::LockTo(device, reply) => {
                let _ = reply.send(self.adapter.router_mut().lock_focus_to(device).map_err(focus_error));
            }
            Command::Game(mode, reply) => {
                let _ = reply.send(
                    self.adapter
                        .router_mut()
                        .member_set_game_mode(mode)
                        .map_err(focus_error),
                );
            }
            Command::Text(text, reply) => {
                let routed = self.adapter.router_mut().member_input(MemberInput::Text(text), now);
                let result = if !routed {
                    Err(ControlError::NoTarget)
                } else if self.flush() {
                    Ok(())
                } else {
                    Err(ControlError::SendFailed)
                };
                let _ = reply.send(result);
            }
            Command::Observer(observer) => {
                if observer.is_none() {
                    // The control page closed: nothing stays held, the glide ends.
                    if let Some(end) = self.scroller.touch() {
                        self.scroll_out(end, now);
                    }
                    self.scroller.reset();
                    self.momentum_due = None;
                    self.adapter.router_mut().member_release_all(now);
                }
                // A page that opens learns the focus computer's text-input
                // focus at once (it pops the keyboard up for a focused field).
                if let Some(observer) = &observer
                    && let Some((device, state)) = self.adapter.router().member_text_focus()
                {
                    observer.text_focus(device, state);
                }
                self.observer = observer;
                self.published = None;
            }
            Command::Screen(screen) => {
                let displays = screen
                    .as_ref()
                    .map(|(display, _)| vec![display.clone()])
                    .unwrap_or_default();
                if let Some(old) = &self.screen {
                    old.group_activity(false);
                }
                self.screen = screen.map(|(_, sink)| sink);
                // A replacement helper waits for new activity; do not revive old leases.
                self.group_activity.clear();
                self.adapter.router_mut().set_local_displays(displays);
                self.refresh_layout();
            }
            Command::Stop => {}
        }
    }

    /// `stale`: received before the latest emergency pause; its input is
    /// dropped, its state still applies.
    fn receive(&mut self, peer: Boot, link: LinkId, message: v2::Message, stale: bool) {
        if self.links.get(&peer.device_id) != Some(&(link, peer)) {
            return;
        }
        let Some(packet) = decode(message) else {
            return;
        };
        if stale
            && matches!(
                packet,
                Packet::Input(_) | Packet::TimedInput { .. } | Packet::Control(ControlMsg::UserActivity)
            )
        {
            return;
        }
        let control = matches!(&packet, Packet::Control(_));
        self.adapter.inbound(peer.device_id, packet);
        let caps = PeerCaps {
            relative: true,
            drag: false,
            input: true,
            windows_lock: false,
        };
        if !self.adapter.attached(peer.device_id)
            && let Some(state) = self.adapter.receive_state(peer.device_id).cloned()
        {
            let os = self.peer_os(peer.device_id);
            self.adapter.attach_with(peer, caps, &state, os);
            self.apply_facts(peer.device_id);
        }
        if control {
            self.refresh_layout();
        }
    }

    /// What the router takes from an attached peer's DeviceFacts: its OS, its
    /// screens (a screenless member moves the pointer to the centre of the
    /// target's primary screen, routing §14.2) and its key mapping. Applied when
    /// the I0 pair attaches and again whenever the facts change: they usually
    /// arrive after the I0 opening, and a peer whose screens were never applied
    /// is listed on the control page but cannot be selected.
    fn apply_facts(&mut self, device: DeviceId) {
        let Some(&(_, peer)) = self.links.get(&device) else {
            return;
        };
        if !self.adapter.attached(device) {
            return;
        }
        let Some(facts) = self.network.peer_facts(device) else {
            return;
        };
        let os = self.peer_os(device);
        let now = self.now();
        let router = self.adapter.router_mut();
        router.set_peer_os(device, os, now);
        router.set_peer_displays(device, displays(&facts));
        // The source's own mapping for keys it sends to this phone
        // (the Android target row of its input transform).
        router.set_peer_input_transform(peer, wire::transform::from_facts(&facts));
    }

    fn peer_os(&self, peer: DeviceId) -> oink_routing::Os {
        self.network
            .peer_facts(peer)
            .and_then(|facts| facts.os().ok().map(|os| os.0))
            .map_or(oink_routing::Os::Other, |os| oink_routing::Os::parse(&os))
    }

    fn refresh_layout(&mut self) {
        let Some(layout) = self.network.layout() else {
            return;
        };
        let facts = self
            .links
            .keys()
            .filter_map(|id| self.network.peer_facts(*id).map(|f| (*id, f)))
            .collect::<std::collections::BTreeMap<_, _>>();
        use v2::schema::WireDto;
        let mut input_online = self.adapter.router().input_online();
        if self.adapter.router().local_displays().is_empty() {
            input_online.remove(&self.adapter.router().device());
        }
        let inputs = LayoutInputs {
            geometry: layout.clone(),
            // Facts sequence/telemetry must not reset an in-progress gap or
            // edge damping gesture. Only topology/owner inputs trigger a rebuild.
            facts: facts
                .iter()
                .map(|(id, f)| {
                    (
                        *id,
                        [7, 12, 18, 19, 32]
                            .into_iter()
                            .filter_map(|key| f.raw().get(&key).map(|value| (key, value.clone())))
                            .collect(),
                    )
                })
                .collect(),
            online: input_online.clone(),
            pointer_base: self.network.pointer_base(),
        };
        if self.layout_inputs.as_ref() == Some(&inputs) {
            return;
        }
        let base = inputs.pointer_base.and_then(|id| {
            let screens = facts.get(&id)?.displays().ok()?.0;
            let primary = screens
                .iter()
                .find(|s| s.is_primary() == Ok(true))
                .or(screens.first())?;
            Some(oink_routing::PointerBase {
                screen: oink_routing::ScreenId::new(id, primary.key().ok()?.0),
                system_scale: primary.system_scale().ok().flatten().unwrap_or(1.),
            })
        });
        // The retained base remains the same computer while it is offline.
        // Keep its last system scale instead of changing pointer units when
        // that peer's live facts disappear; a new base clears this observation.
        if base.is_some()
            || inputs.pointer_base.is_none()
            || self.layout_inputs.as_ref().and_then(|i| i.pointer_base) != inputs.pointer_base
        {
            self.adapter.router_mut().set_pointer_base(base);
        }
        let me = self.adapter.router().device();
        let online = |id: &DeviceId| {
            *id == me
                || facts.get(id).is_some_and(|f| {
                    !f.pause().ok().flatten().is_some_and(|p| p.paused() == Ok(true))
                        && (f.os().is_ok_and(|os| os.0 != "android")
                            || (f.android_privileged() == Ok(true) && f.displays().is_ok_and(|d| !d.0.is_empty())))
                })
        };
        match wire::layout::topology(&layout, &facts, online) {
            Ok(topology) => {
                self.adapter
                    .router_mut()
                    .set_layout(Arc::new(topology.with_online(input_online)));
                self.layout_inputs = Some(inputs);
            }
            Err(error) => tracing::warn!(%error, "invalid mobile input layout"),
        }
    }

    fn sync_links(&mut self) {
        let table = self.network.links();
        let mut ready = std::collections::HashMap::new();
        for entry in &table.links {
            if entry.state == LinkState::Ready
                && entry.opens(oink_protocol::frame::StreamKind::Input)
                && table.by_peer(entry.peer).map(|l| l.link) == Some(entry.link)
            {
                ready.insert(entry.peer, (entry.link, entry.peer_boot));
            }
        }
        let gone: Vec<_> = self
            .links
            .iter()
            .filter(|(peer, current)| ready.get(*peer) != Some(*current))
            .map(|(peer, (_, boot))| (*peer, *boot))
            .collect();
        for (peer, boot) in gone {
            self.links.remove(&peer);
            self.remove_group_activity(peer);
            self.adapter.detach(peer);
            self.adapter.clear_received(peer);
            self.adapter.router_mut().retire_boot(boot);
        }
        for (peer, current) in ready {
            if self.links.insert(peer, current) != Some(current) {
                self.adapter.router_mut().admit_boot(current.1);
            }
        }
        self.refresh_layout();
    }

    fn clear_group_activity(&mut self) {
        self.group_activity.clear();
        if let Some(screen) = &self.screen {
            screen.group_activity(false);
        }
    }

    fn remove_group_activity(&mut self, peer: DeviceId) {
        if self.group_activity.remove(&peer).is_some()
            && self.group_activity.is_empty()
            && let Some(screen) = &self.screen
        {
            screen.group_activity(false);
        }
    }

    fn expire_group_activity(&mut self) {
        let had_activity = !self.group_activity.is_empty();
        let now = Instant::now();
        self.group_activity.retain(|_, due| *due > now);
        if had_activity
            && self.group_activity.is_empty()
            && let Some(screen) = &self.screen
        {
            screen.group_activity(false);
        }
    }

    /// A scroll event of the fingers in millimetres: accelerated like a Mac
    /// trackpad's and sent with its phase; a lift (Ended) starts momentum.
    fn finger_scroll(
        &mut self,
        phase: FingerPhase,
        dx: f64,
        dy: f64,
        time_us: u64,
        since_us: u64,
        now: oink_routing::Micros,
    ) {
        let steps = self.scroller.finger(phase, dx, dy, time_us, since_us);
        self.no_target = false;
        for step in steps {
            self.scroll_out(step, now);
        }
        self.momentum_due = self
            .scroller
            .momentum_active()
            .then(|| Instant::now() + Duration::from_secs_f64(trackpad::MOMENTUM_FRAME));
    }

    /// One accelerated touchpad scroll event to the focus computer, as a Mac
    /// trackpad source sends it (pixels, with its phase).
    fn scroll_out(&mut self, step: trackpad::ScrollStep, now: oink_routing::Micros) {
        self.adapter.router_mut().member_input(
            MemberInput::Scroll {
                dx: step.dx,
                dy: step.dy,
                phase: step.phase,
            },
            now,
        );
    }

    /// Momentum events, one every 1/120 s (two per 1/60 s curve step) until MomentumEnded.
    fn momentum_tick(&mut self, now: oink_routing::Micros) {
        let Some(due) = self.momentum_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        match self.scroller.momentum_frame() {
            Some(step) => {
                self.scroll_out(step, now);
                // Fixed frame times, not drifting with the wake-up latency.
                self.momentum_due = self
                    .scroller
                    .momentum_active()
                    .then(|| due + Duration::from_secs_f64(trackpad::MOMENTUM_FRAME));
            }
            None => self.momentum_due = None,
        }
    }

    fn heartbeat(&mut self) {
        let peers: Vec<DeviceId> = self.links.keys().copied().collect();
        for peer in peers {
            for packet in self.adapter.router().heartbeat_messages(peer) {
                if let Some(message) = encode(&packet)
                    && let Err(error) = self.network.send_input(peer, message)
                {
                    tracing::debug!(?peer, %error, "input router: heartbeat record not sent");
                }
            }
        }
    }

    fn refresh_opening(&mut self) {
        let records: Vec<v2::Message> = self
            .adapter
            .opening_state(false)
            .into_iter()
            .filter(|packet| !matches!(packet, Packet::Dnd(_)))
            .filter_map(|packet| encode(&packet))
            .collect();
        *self.opening.lock().unwrap_or_else(|e| e.into_inner()) = records;
    }

    /// Returns whether every message was handed to the network.
    fn flush(&mut self) -> bool {
        let mut sent = true;
        let outputs = self.adapter.router_mut().drain_outputs();
        for output in outputs {
            match output {
                Output::Send { to, msg, .. } => {
                    if let Some(message) = encode(&msg)
                        && let Err(error) = self.network.send_input(to, message)
                    {
                        sent = false;
                        tracing::debug!(?to, %error, "input router: send failed");
                    }
                }
                Output::ExitVolumeKey {
                    source,
                    route,
                    press_id,
                    code,
                    pressed,
                    repeat,
                    modifiers,
                } => self.network.exit_volume_key(ExitVolumeKey {
                    source,
                    route,
                    press_id,
                    code,
                    pressed,
                    repeat,
                    modifiers,
                }),
                Output::Status(StatusEvent::NoTarget) => self.no_target = true,
                // Diagnostics: the raw-contacts source changes state once per touch phase
                // (Single → Raw at the second finger, Raw → Ready at the lift).
                Output::Status(StatusEvent::TouchpadContacts {
                    mode,
                    frames,
                    cancelled,
                    ..
                }) => tracing::info!(?mode, frames, cancelled, "control touchpad: contacts state"),
                Output::Status(StatusEvent::TextRefused { reason, .. }) => {
                    if let Some(observer) = &self.observer {
                        observer.text_refused(reason);
                    }
                }
                Output::Status(StatusEvent::TextFocus { device, state }) => {
                    tracing::info!(
                        ?device,
                        seq = state.seq,
                        editable = state.editable,
                        secure = state.secure,
                        observed = self.observer.is_some(),
                        "input router: focus holder's text-input focus"
                    );
                    if let Some(observer) = &self.observer {
                        observer.text_focus(device, state);
                    }
                }
                Output::Status(StatusEvent::Edge { state, edge, target }) => {
                    if let Some(observer) = &self.observer {
                        observer.edge(ControlEdge {
                            state,
                            edge,
                            to_device: target,
                        });
                    }
                }
                Output::UserActivity { from, .. } => {
                    self.group_activity.insert(from, Instant::now() + GROUP_ACTIVITY_FRESH);
                    if let Some(screen) = &self.screen {
                        screen.group_activity(true);
                    }
                }
                Output::Inject(event) => {
                    if let Some(screen) = &self.screen {
                        screen.inject(event);
                    }
                }
                Output::RestoreLocal { at } => {
                    if let Some(screen) = &self.screen {
                        screen.focus(true);
                        screen.inject(InjectEvent::MoveTo { pos: at });
                    }
                }
                Output::SetMode(oink_foundation::input::CaptureMode::Remote { .. }) => {
                    if let Some(screen) = &self.screen {
                        screen.focus(false);
                    }
                }
                _ => {}
            }
        }
        self.refresh_opening();
        self.publish();
        sent
    }

    fn publish(&mut self) {
        *self.locked.lock().unwrap_or_else(|e| e.into_inner()) =
            self.adapter.router().group_focus_status().locked_device;
        let Some(observer) = self.observer.clone() else {
            return;
        };
        let router = self.adapter.router();
        let status = router.group_focus_status();
        let game = router.game_mode();
        let focus = router.member_target();
        let targets = self
            .links
            .keys()
            .filter_map(|peer| {
                let facts = self.network.peer_facts(*peer)?;
                let controllable = self
                    .adapter
                    .receive_state(*peer)
                    .and_then(|state| state.controllable)
                    .unwrap_or(false);
                if !controllable {
                    return None;
                }
                Some(ControlTarget {
                    device: *peer,
                    name: facts.name().map(|n| n.0).unwrap_or_default(),
                    platform: facts.os().map(|os| os.0).unwrap_or_default(),
                    online: self.adapter.attached(*peer),
                })
            })
            .collect();
        let state = ControlState {
            focus_device: focus,
            selected_device: router.selected_target(),
            no_target: self.no_target,
            locked: focus.is_some() && status.locked_device == focus,
            game_mode: game.mode,
            game_active: focus.is_some() && game.active,
            targets,
        };
        if self.published.as_ref() != Some(&state) {
            self.published = Some(state.clone());
            observer.state(state);
        }
    }
}

fn focus_error(error: oink_routing::FocusLockError) -> ControlError {
    match error {
        oink_routing::FocusLockError::Paused => ControlError::Paused,
        oink_routing::FocusLockError::Unavailable => ControlError::NoTarget,
    }
}

/// A router packet as the v2 message on I0.
fn encode(packet: &Packet) -> Option<v2::Message> {
    match packet {
        Packet::Control(control) => wire::control::encode(control).ok(),
        Packet::Input(input) | Packet::TimedInput { input, .. } => {
            let mut buffer = vec![0u8; v2::limits::RELIABLE_FRAME];
            let len = wire::input::encode(input, None, &mut buffer).ok()?;
            OpaqueFrame::parse_exact(&buffer[..len], v2::limits::RELIABLE_FRAME)
                .ok()?
                .decode()
                .ok()
        }
        Packet::Dnd(_) => None,
    }
}

/// A received v2 message as a router packet.
fn decode(message: v2::Message) -> Option<Packet> {
    let id = message.type_id();
    if id == 0x020d {
        return wire::input::decode_text(message).ok();
    }
    if (0x0201..=0x02ff).contains(&id) {
        let bytes = v2::frame::encode(&message, v2::limits::RELIABLE_FRAME).ok()?;
        let frame = OpaqueFrame::parse_exact(&bytes, v2::limits::RELIABLE_FRAME).ok()?;
        return wire::input::decode(frame).ok();
    }
    wire::control::decode(message).ok().map(Packet::Control)
}

fn displays(facts: &dto::DeviceFacts) -> Vec<DisplayInfo> {
    let Ok(list) = facts.displays() else {
        return Vec::new();
    };
    list.0
        .into_iter()
        .filter_map(|display| {
            let bounds = display.bounds().ok()?;
            Some(DisplayInfo {
                key: display.key().ok()?.0,
                name: display.name().ok()?.0,
                bounds: oink_foundation::Rect::new(
                    bounds.x().ok()?,
                    bounds.y().ok()?,
                    bounds.w().ok()?,
                    bounds.h().ok()?,
                ),
                pixel_width: display.pixel_width().ok()?,
                pixel_height: display.pixel_height().ok()?,
                reference_extent: None,
                rotation_deg: display.rotation_deg().ok()?,
                is_primary: display.is_primary().ok()?,
                is_builtin: display.is_builtin().ok()?,
                is_virtual: display.is_virtual().ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! The embedded router against a real desktop Router over the v2 I0 codec.
    use super::*;
    use oink_foundation::{
        Point, Rect,
        input::{CapturedEvent, FeelProfile, Gesture, GesturePhase, InputEvent, ScrollPhase, SourceKind},
        session::{BootPair, PathId},
    };
    use oink_network::link::LinkPath;

    const PC: DeviceId = DeviceId([1; 32]);
    const PHONE: DeviceId = DeviceId([9; 32]);
    const LINK: LinkId = LinkId(7);

    fn boot(id: DeviceId) -> Boot {
        Boot {
            device_id: id,
            nonce: [id.0[0]; 16],
        }
    }

    struct FakeNet {
        links: Arc<LinkTable>,
        sent: Mutex<mpsc::Sender<v2::Message>>,
        /// The PC's DeviceFacts as the control stream has delivered them so far.
        facts: Mutex<Option<dto::DeviceFacts>>,
        layout: Mutex<Option<dto::LayoutValue>>,
        /// The media owner's published speaker exit.
        exit: Mutex<Option<oink_routing::input::VolumeRoute>>,
        /// Volume keys handed to the media owner (this phone as the exit).
        exit_keys: Mutex<Vec<ExitVolumeKey>>,
    }
    impl InputNetwork for FakeNet {
        fn layout(&self) -> Option<dto::LayoutValue> {
            self.layout.lock().unwrap().clone()
        }
        fn links(&self) -> Arc<LinkTable> {
            self.links.clone()
        }
        fn peer_facts(&self, peer: DeviceId) -> Option<dto::DeviceFacts> {
            assert_eq!(peer, PC);
            self.facts.lock().unwrap().clone()
        }
        fn send_input(&self, peer: DeviceId, message: v2::Message) -> Result<(), String> {
            assert_eq!(peer, PC);
            self.sent.lock().unwrap().send(message).map_err(|e| e.to_string())
        }
        fn volume_exit(&self) -> Option<oink_routing::input::VolumeRoute> {
            self.exit.lock().unwrap().clone()
        }
        fn exit_volume_key(&self, key: ExitVolumeKey) {
            self.exit_keys.lock().unwrap().push(key);
        }
    }

    /// A volume-key context of `source` for the exit `target` (route
    /// version and output binding as the exit's published volume state).
    fn exit_route(source: DeviceId, target: DeviceId) -> oink_routing::input::VolumeRoute {
        use oink_protocol::v2::{RawValue, schema::WireDto, schema::WireValue};
        let wire = |id: DeviceId| oink_protocol::v2::identity::boot(boot(id));
        let output = dto::ReceiverOutputBinding::from_raw(
            [
                (1, wire(target).write()),
                (2, 3u64.write()),
                (3, RawValue::from("mobile:default")),
            ]
            .into(),
        )
        .unwrap();
        let version = dto::AudioRouteVersion::from_raw(
            [(1, 1u64.write()), (2, wire(target).device_id().unwrap().write())].into(),
        )
        .unwrap();
        let session = dto::AudioSessionContext::from_raw(
            [
                (1, wire(source).write()),
                (2, 3u64.write()),
                (3, output.write()),
                (4, version.write()),
            ]
            .into(),
        )
        .unwrap();
        oink_routing::input::VolumeRoute::from_session(Arc::new(session)).unwrap()
    }

    fn table() -> LinkTable {
        LinkTable {
            links: vec![oink_network::link::LinkEntry {
                link: LINK,
                peer: PC,
                peer_boot: boot(PC),
                path_id: PathId {
                    boot_pair: BootPair::new(boot(PC), boot(PHONE)).unwrap(),
                    coordinator_counter: 1,
                },
                route_revision: 1,
                agreement: v2::admission::Agreement::new(["oink.core/2".to_owned(), "oink.input/2".to_owned()])
                    .unwrap(),
                peer_formats: None,
                peer_audio: None,
                state: LinkState::Ready,
                path: LinkPath::Direct,
                remote_address: None,
                rtt: None,
                rtt_measured_at: None,
                lost_packets: 0,
                sent_packets: 0,
                udp_bytes_sent: 0,
                udp_datagrams_sent: 0,
                udp_bytes_received: 0,
                low_available: false,
                low_failure: None,
                route: None,
            }],
        }
    }

    struct Pair {
        phone: Arc<InputRouter>,
        net: Arc<FakeNet>,
        sent: mpsc::Receiver<v2::Message>,
        pc: Router,
        injected: Vec<InjectEvent>,
        /// Volume keys the PC executed as the exit.
        pc_exit_keys: Vec<Output>,
    }
    impl Pair {
        fn new() -> Self {
            let (tx, sent) = mpsc::channel();
            let net = Arc::new(FakeNet {
                links: Arc::new(table()),
                sent: Mutex::new(tx),
                facts: Mutex::new(None),
                layout: Mutex::new(None),
                exit: Mutex::new(None),
                exit_keys: Mutex::new(Vec::new()),
            });
            let phone = InputRouter::start(boot(PHONE), net.clone());
            phone.links_changed();
            let mut pc = Router::new(
                RouterConfig::new(boot(PC), "pc", oink_routing::Os::Windows),
                Arc::new(oink_routing::SimpleLayout::new()),
            );
            pc.set_local_displays(vec![DisplayInfo {
                key: "main".into(),
                name: "main".into(),
                bounds: Rect::new(0., 0., 1000., 800.),
                pixel_width: 1000,
                pixel_height: 800,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: true,
                is_builtin: false,
                is_virtual: false,
            }]);
            pc.set_cursor(Point::new(500., 400.));
            pc.set_game_mode(GameMode::Off);
            assert!(pc.attach_input(boot(PHONE), [PHONE.0[0]; 16], false, false, FeelProfile::default(), 0));
            let mut pair = Self {
                phone,
                net,
                sent,
                pc,
                injected: Vec::new(),
                pc_exit_keys: Vec::new(),
            };
            // The phone's opening reaches the PC; the PC's opening reaches the phone.
            for message in pair.phone.opening(false) {
                pair.deliver_to_pc(message);
            }
            for packet in pair.pc.input_opening(true) {
                if let Some(message) = encode(&packet) {
                    pair.phone.message(boot(PC), LINK, message);
                }
            }
            pair.pump(Duration::from_millis(200));
            pair
        }
        fn deliver_to_pc(&mut self, message: v2::Message) {
            let packet = decode(message).expect("the phone sends valid I0");
            self.pc.input_message(PHONE, packet, 1);
        }
        /// Move messages both ways until the phone is quiet for `quiet`.
        fn pump(&mut self, quiet: Duration) {
            loop {
                for output in self.pc.drain_outputs() {
                    match output {
                        Output::Send { to, msg, .. } if to == PHONE => {
                            if let Some(message) = encode(&msg) {
                                self.phone.message(boot(PC), LINK, message);
                            }
                        }
                        Output::Inject(event) => self.injected.push(event),
                        output @ Output::ExitVolumeKey { .. } => self.pc_exit_keys.push(output),
                        _ => {}
                    }
                }
                match self.sent.recv_timeout(quiet) {
                    Ok(message) => self.deliver_to_pc(message),
                    Err(_) => return,
                }
            }
        }
    }

    /// A Windows PC's DeviceFacts with one 1000 × 800 primary screen "main".
    fn windows_facts() -> dto::DeviceFacts {
        use oink_protocol::v2::{
            RawValue,
            schema::{WireDto, WireValue},
        };
        fn map<const N: usize>(items: [(u32, RawValue); N]) -> RawValue {
            RawValue::Map(items.into())
        }
        let display = map([
            (1, RawValue::from("main")),
            (2, RawValue::from("main")),
            (
                3,
                map([
                    (1, 0f64.write()),
                    (2, 0f64.write()),
                    (3, 1000f64.write()),
                    (4, 800f64.write()),
                ]),
            ),
            (5, 1000u32.write()),
            (6, 800u32.write()),
            (8, 0u16.write()),
            (9, 1f64.write()),
            (10, true.write()),
            (11, false.write()),
            (12, false.write()),
        ]);
        dto::DeviceFacts::from_raw(
            [
                (1, oink_protocol::v2::identity::boot(boot(PC)).write()),
                (2, 1u64.write()),
                (3, map([(1, v2::MAJOR.write()), (2, v2::MINOR.write())])),
                (4, RawValue::from("build")),
                (5, RawValue::Bytes(vec![1; 16])),
                (6, RawValue::from("a Windows test machine")),
                (7, RawValue::from("windows")),
                (8, RawValue::from("")),
                (9, RawValue::from("build")),
                (10, RawValue::Array(Vec::new())),
                (
                    11,
                    map([
                        (1, true.write()),
                        (2, dto::RunIntent::Running.write()),
                        (4, 0u64.write()),
                    ]),
                ),
                (12, RawValue::Array(vec![display])),
                (
                    13,
                    map([
                        (1, RawValue::Array(Vec::new())),
                        (2, dto::CapturePermission::Authorized.write()),
                        (3, RawValue::Array(Vec::new())),
                        (4, RawValue::Array(Vec::new())),
                    ]),
                ),
                (
                    14,
                    map([
                        (1, true.write()),
                        (3, 1f32.write()),
                        (4, false.write()),
                        (5, false.write()),
                        (6, false.write()),
                    ]),
                ),
                (15, RawValue::Array(Vec::new())),
                (25, dto::DeviceKind::Desktop.write()),
                (26, v2::MAJOR.write()),
                (27, v2::MINOR.write()),
            ]
            .into(),
        )
        .expect("valid Windows DeviceFacts")
    }

    #[test]
    fn embedded_phone_compiles_the_shared_layout_and_refreshes_rotation() {
        use oink_protocol::v2::{
            RawValue,
            schema::{WireDto, WireValue},
        };
        use oink_routing::{LayoutQuery, ScreenId, ScreenPos};
        fn map<const N: usize>(items: [(u32, RawValue); N]) -> RawValue {
            RawValue::Map(items.into())
        }
        let (sent, _) = mpsc::channel();
        let net = Arc::new(FakeNet {
            links: Arc::new(table()),
            sent: Mutex::new(sent),
            facts: Mutex::new(Some(windows_facts())),
            layout: Mutex::new(None),
            exit: Mutex::new(None),
            exit_keys: Mutex::new(Vec::new()),
        });
        let layout = |w: f64, h: f64| {
            let devices = [
                (PC, "main", 0., 1000., 800.),
                (PHONE, oink_foundation::display::ANDROID_INPUT_DISPLAY, 1., w, h),
            ]
            .into_iter()
            .map(|(id, key, x, w, h)| {
                let device = v2::identity::device(id).write();
                map([
                    (1, device.clone()),
                    (2, map([(1, x.write()), (2, 0f64.write())])),
                    (3, 0.001f64.write()),
                    (
                        4,
                        RawValue::Array(vec![map([
                            (1, map([(1, device), (2, RawValue::from(key))])),
                            (2, RawValue::from(key)),
                            (
                                3,
                                map([(1, 0f64.write()), (2, 0f64.write()), (3, w.write()), (4, h.write())]),
                            ),
                            (5, dto::SizeSource::Reported.write()),
                            (9, true.write()),
                            (10, true.write()),
                        ])]),
                    ),
                ])
            })
            .collect();
            dto::LayoutValue::from_raw(
                [
                    (1, RawValue::Array(devices)),
                    (2, dto::LayoutSource::Default.write()),
                    (4, RawValue::Map(Default::default())),
                ]
                .into(),
            )
            .unwrap()
        };
        *net.layout.lock().unwrap() = Some(layout(400., 800.));
        let mut worker = Worker::new(
            boot(PHONE),
            net.clone(),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU64::new(0)),
        );
        worker.sync_links();
        worker
            .adapter
            .router_mut()
            .attach_input(boot(PC), [1; 16], true, false, FeelProfile::default(), 0);
        worker.refresh_layout();
        let phone = ScreenId::new(PHONE, oink_foundation::display::ANDROID_INPUT_DISPLAY);
        let from = ScreenPos::new(phone.clone(), Point::new(1., 400.));
        // Query the same production converter used by refresh_layout: the phone's
        // left edge returns to the PC; its new width is visible after rotation.
        let topology =
            wire::layout::topology(&net.layout().unwrap(), &[(PC, windows_facts())].into(), |_| true).unwrap();
        assert_eq!(topology.step(&from, (-0.01, 0.)).to.screen.device, PC);
        assert_eq!(worker.adapter.router().focus().holder, PHONE);
        *net.layout.lock().unwrap() = Some(layout(800., 400.));
        worker.command(Command::LayoutChanged);
        let mut screen = displays(&windows_facts()).remove(0);
        screen.key = phone.display.clone();
        screen.bounds = Rect::new(0., 0., 800., 400.);
        worker.adapter.router_mut().set_local_displays(vec![screen]);
        worker.refresh_layout();
        let session = worker.adapter.router().routing_session();
        worker.adapter.router_mut().input_message(
            PC,
            Packet::Control(ControlMsg::FocusEnter {
                epoch: 1,
                display: phone.display,
                pos: Point::new(600., 200.),
                scope: Some(oink_routing::control::FocusScope {
                    members: vec![(PC, [1; 16]), (PHONE, session)],
                }),
                pointer_source: Some(PC),
            }),
            1,
        );
        // With stale portrait topology this step clamps at x=400. The worker's
        // refreshed landscape topology lets its virtual pointer reach x=650.
        worker.adapter.router_mut().input_message(
            PC,
            Packet::Input(oink_routing::input::InputMsg::Motion {
                seq: 1,
                dx: 0.05,
                dy: 0.,
            }),
            2,
        );
        let pointer = worker.adapter.router().focus().pointer.unwrap();
        assert!((pointer.local.x - 650.).abs() < 1e-6, "{pointer:?}");
        worker.adapter.router_mut().set_local_displays(Vec::new());
        assert!(worker.adapter.router().focus().pointer.is_none());
    }

    /// Installed 0.4.0-inhouse.4 (2026-11-12): choosing a Windows PC in the
    /// control page's device chooser answered 「请先选择要控制的电脑」. The PC's
    /// I0 opening attaches it before its DeviceFacts arrive on the control
    /// stream; its screens were taken only at attach, so with nothing holding
    /// the group pointer the phone had no screen to move the pointer to. The
    /// facts arriving later must make it selectable.
    #[test]
    fn a_pc_whose_facts_arrive_after_its_i0_opening_can_be_selected() {
        let mut pair = Pair::new();
        // Attached without facts (the opening came first); nobody holds focus.
        let (before, _) = pair.pc.focus_claim();
        assert_eq!(pair.phone.move_pointer_to(PC), Err(ControlError::NoTarget));
        // The PC's DeviceFacts arrive on the control stream.
        *pair.net.facts.lock().unwrap() = Some(windows_facts());
        crate::MobileInputSink::facts_changed(pair.phone.as_ref(), PC);
        assert_eq!(pair.phone.move_pointer_to(PC), Ok(()));
        pair.pump(Duration::from_millis(200));
        // The PC took the pointer at the centre of its primary screen.
        let (epoch, holder) = pair.pc.focus_claim();
        assert_eq!(holder, PC);
        assert!(epoch > before, "the move started a new focus epoch");
        pair.phone.stop();
    }

    #[test]
    fn phone_text_reaches_the_focus_holder_over_i0_in_order_with_keys() {
        let mut pair = Pair::new();
        // The PC's own first input makes it the group focus (epoch 1).
        pair.pc.on_local_event(
            &CapturedEvent {
                event: InputEvent::Key {
                    code: KeyCode::keyboard(0x04),
                    pressed: true,
                },
                physical: true,
                time_us: 0,
                cursor: Some(Point::new(500., 400.)),
                source: SourceKind::Keyboard,
            },
            2,
        );
        pair.pump(Duration::from_millis(200));
        assert_eq!(pair.pc.focus_claim().1, PC);
        pair.injected.clear();
        // Text, then Return, from the control page.
        assert_eq!(pair.phone.input_text("你好".into()), Ok(()));
        pair.phone.key(KeyCode::keyboard(0x28).0, true);
        pair.phone.key(KeyCode::keyboard(0x28).0, false);
        pair.pump(Duration::from_millis(300));
        let seen: Vec<String> = pair
            .injected
            .iter()
            .filter_map(|event| match event {
                InjectEvent::Text(text) => Some(text.0.clone()),
                InjectEvent::Key { code, pressed } if *code == KeyCode::keyboard(0x28) => {
                    Some(format!("enter:{pressed}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(seen, ["你好", "enter:true", "enter:false"]);
        pair.phone.stop();
    }

    /// The native keyboard owns Caps timing: I0 must preserve its down/up edges, with no
    /// synthetic hold or repeat in the embedded router, even across a rapid second press.
    #[test]
    fn phone_caps_follows_host_edges_without_synthetic_hold_or_repeat() {
        let mut pair = Pair::new();
        *pair.net.facts.lock().unwrap() = Some(windows_facts());
        crate::MobileInputSink::facts_changed(pair.phone.as_ref(), PC);
        assert_eq!(pair.phone.move_pointer_to(PC), Ok(()));
        pair.pump(Duration::from_millis(100));
        pair.injected.clear();
        let caps = KeyCode::keyboard(0x39);
        let caps_edges = |events: &[InjectEvent]| {
            events
                .iter()
                .filter_map(|event| match event {
                    InjectEvent::Key { code, pressed } if *code == caps => Some(*pressed),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let mut expected = Vec::new();
        for hold in [Duration::from_millis(60), Duration::from_millis(1100)] {
            pair.phone.key(caps.0, true);
            expected.push(true);
            pair.pump(Duration::from_millis(100));
            assert_eq!(caps_edges(&pair.injected), expected, "down reaches PC before lift");
            pair.pump(hold);
            assert_eq!(caps_edges(&pair.injected), expected, "no repeat or timed up while held");
            // Lift (also the edge the UI sends after slide-off/cancel) arrives immediately,
            // even for a press shorter than the removed 800 ms hold.
            pair.phone.key(caps.0, false);
            expected.push(false);
            pair.pump(Duration::from_millis(100));
            assert_eq!(caps_edges(&pair.injected), expected, "up follows the host's real edge");
        }
        pair.pump(Duration::from_millis(900));
        assert_eq!(caps_edges(&pair.injected), expected, "no delayed edge after release");
        pair.phone.stop();
    }

    #[test]
    fn a_control_page_volume_key_reaches_the_published_exit_over_i0() {
        let mut pair = Pair::new();
        *pair.net.exit.lock().unwrap() = Some(exit_route(PHONE, PC));
        pair.injected.clear();
        let up = KeyCode::new(0x0c, 0xe9);
        pair.phone.key(up.0, true);
        pair.phone.key(up.0, false);
        pair.pump(Duration::from_millis(300));
        let executed: Vec<(DeviceId, bool)> = pair
            .pc_exit_keys
            .iter()
            .filter_map(|output| match output {
                Output::ExitVolumeKey {
                    source, code, pressed, ..
                } if *code == up => Some((source.device_id, *pressed)),
                _ => None,
            })
            .collect();
        assert_eq!(executed, [(PHONE, true), (PHONE, false)]);
        assert!(pair.injected.is_empty(), "the key is not injected as an ordinary key");
        pair.phone.stop();
    }

    #[test]
    fn a_volume_key_for_this_phone_as_the_exit_goes_to_the_media_owner() {
        let pair = Pair::new();
        let route = exit_route(PC, PHONE);
        let message = encode(&Packet::Input(oink_routing::input::InputMsg::Volume {
            route: route.clone(),
            press_id: 1,
            code: KeyCode::new(0x0c, 0xe2),
            pressed: true,
            repeat: false,
            modifiers: 0,
        }))
        .expect("InputVolume encodes");
        pair.phone.message(boot(PC), LINK, message);
        let deadline = Instant::now() + Duration::from_secs(2);
        while pair.net.exit_keys.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let keys = pair.net.exit_keys.lock().unwrap().clone();
        assert_eq!(
            keys,
            [ExitVolumeKey {
                source: boot(PC),
                route,
                press_id: 1,
                code: KeyCode::new(0x0c, 0xe2),
                pressed: true,
                repeat: false,
                modifiers: 0,
            }]
        );
        pair.phone.stop();
    }

    /// The helper's calls; the first injected key-down blocks the router
    /// thread until the test lets it go.
    struct GatedScreen {
        events: Mutex<Vec<String>>,
        gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    }
    impl ScreenSink for GatedScreen {
        fn focus(&self, active: bool) {
            self.events.lock().unwrap().push(format!("focus:{active}"));
        }
        fn inject(&self, event: InjectEvent) {
            if let InjectEvent::Key { code, pressed } = event {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("key:{}:{pressed}", code.usage()));
                let gate = if pressed {
                    self.gate.lock().unwrap().take()
                } else {
                    None
                };
                if let Some((entered, go)) = gate {
                    entered.send(()).unwrap();
                    go.recv().unwrap();
                }
            }
        }
        fn release_all(&self) {
            self.events.lock().unwrap().push("release_all".into());
        }
    }

    fn key_event(usage: u16, pressed: bool) -> CapturedEvent {
        CapturedEvent {
            event: InputEvent::Key {
                code: KeyCode::keyboard(usage),
                pressed,
            },
            physical: true,
            time_us: 0,
            cursor: None,
            source: SourceKind::Keyboard,
        }
    }

    #[test]
    fn emergency_pause_drops_input_not_yet_injected_and_releases_the_helper() {
        let mut pair = Pair::new();
        let screen_display = DisplayInfo {
            key: oink_foundation::display::ANDROID_INPUT_DISPLAY.into(),
            name: "Built-in display".into(),
            bounds: Rect::new(0., 0., 400., 800.),
            pixel_width: 400,
            pixel_height: 800,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: true,
            is_virtual: false,
        };
        let (entered_tx, entered) = mpsc::channel();
        let (go, go_rx) = mpsc::channel();
        let screen = Arc::new(GatedScreen {
            events: Mutex::new(Vec::new()),
            gate: Mutex::new(Some((entered_tx, go_rx))),
        });
        pair.phone.set_screen(Some((screen_display.clone(), screen.clone())));
        // The helper is ready: the network reopens I0 with a controllable
        // opening (the PC attaches the new pair as sharing), and the PC moves
        // the pointer onto the phone's own screen.
        let opening = pair.phone.opening(true);
        let instance = opening
            .iter()
            .find_map(|message| match decode(message.clone()) {
                Some(Packet::Control(ControlMsg::FocusState {
                    session: Some(session), ..
                })) => Some(session),
                _ => None,
            })
            .expect("the opening names the phone's router instance");
        pair.pc.detach_input(PHONE);
        assert!(
            pair.pc
                .attach_input(boot(PHONE), instance, true, false, FeelProfile::default(), 2)
        );
        for message in opening {
            pair.deliver_to_pc(message);
        }
        pair.pc.set_peer_os(PHONE, oink_routing::Os::Android, 1);
        let mut layout = oink_routing::SimpleLayout::new();
        layout.add_screen(
            oink_routing::ScreenId::new(PC, "main"),
            Rect::new(0., 0., 1000., 800.),
            Rect::new(0., 0., 1000., 800.),
        );
        layout.add_screen(
            oink_routing::ScreenId::new(PHONE, &screen_display.key),
            screen_display.bounds,
            Rect::new(1000., 0., 400., 800.),
        );
        pair.pc.set_layout(Arc::new(layout));
        pair.pc.set_peer_displays(PHONE, vec![screen_display]);
        pair.pc.move_pointer_to(PHONE).unwrap();
        pair.pump(Duration::from_millis(200));
        assert!(screen.events.lock().unwrap().contains(&"focus:true".to_owned()));
        // 'a' is being injected (the router thread is inside the helper) when
        // 'b' arrives and then the emergency pause.
        pair.pc.on_local_event(&key_event(0x04, true), 3);
        pair.pump(Duration::from_millis(50));
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("'a' reached the helper");
        pair.pc.on_local_event(&key_event(0x05, true), 4);
        pair.pump(Duration::from_millis(50));
        pair.phone.emergency_pause();
        go.send(()).unwrap();
        pair.pump(Duration::from_millis(300));
        let events = screen.events.lock().unwrap().clone();
        let keys: Vec<&str> = events
            .iter()
            .map(String::as_str)
            .filter(|event| event.starts_with("key:") || *event == "release_all")
            .collect();
        assert_eq!(keys, ["key:4:true", "key:4:false", "release_all"], "{events:?}");
        pair.phone.stop();
    }

    #[derive(Default)]
    struct TextFocusObserver {
        seen: Mutex<Vec<(DeviceId, oink_routing::control::TextFocusState)>>,
    }
    impl ControlObserver for TextFocusObserver {
        fn state(&self, _: ControlState) {}
        fn edge(&self, _: ControlEdge) {}
        fn text_refused(&self, _: oink_routing::control::TextRefusal) {}
        fn text_focus(&self, device: DeviceId, state: oink_routing::control::TextFocusState) {
            self.seen.lock().unwrap().push((device, state));
        }
    }

    impl Pair {
        /// The PC's heartbeat records for the phone, delivered.
        fn heartbeat_to_phone(&mut self) {
            for packet in self.pc.heartbeat_messages(PHONE) {
                if let Some(message) = encode(&packet) {
                    self.phone.message(boot(PC), LINK, message);
                }
            }
            self.pump(Duration::from_millis(150));
        }
    }

    fn field(seq: u64, editable: bool, secure: bool) -> oink_routing::control::TextFocusState {
        oink_routing::control::TextFocusState { seq, editable, secure }
    }

    /// Protocol §14.7 end to end over the v2 I0 codec: every change of the
    /// focus computer's text-input focus (entering, a password field, leaving)
    /// reaches the open control page once; heartbeat copies do not repeat it;
    /// a page that opens gets the current state at once.
    #[test]
    fn the_focus_computers_text_input_focus_reaches_the_control_page() {
        let mut pair = Pair::new();
        let observer = Arc::new(TextFocusObserver::default());
        pair.phone.set_observer(Some(observer.clone()));
        pair.pc.on_local_event(&key_event(0x04, true), 2);
        pair.pc.on_local_event(&key_event(0x04, false), 3);
        pair.pump(Duration::from_millis(200));
        assert_eq!(pair.pc.focus_claim().1, PC);
        pair.pc.set_text_focus(field(1, true, false));
        pair.pump(Duration::from_millis(200));
        assert_eq!(*observer.seen.lock().unwrap(), [(PC, field(1, true, false))]);
        // The 2 s heartbeat repeats the state: nothing new.
        pair.heartbeat_to_phone();
        pair.heartbeat_to_phone();
        assert_eq!(observer.seen.lock().unwrap().len(), 1);
        // Leaving the field arrives at once; then a password field.
        pair.pc.set_text_focus(field(1, false, false));
        pair.pump(Duration::from_millis(200));
        pair.pc.set_text_focus(field(2, true, true));
        pair.pump(Duration::from_millis(200));
        assert_eq!(
            *observer.seen.lock().unwrap(),
            [
                (PC, field(1, true, false)),
                (PC, field(1, false, false)),
                (PC, field(2, true, true))
            ]
        );
        // The page closes; the field changes meanwhile; a page opening later
        // gets the current state at once.
        pair.phone.set_observer(None);
        pair.pc.set_text_focus(field(3, true, false));
        pair.pump(Duration::from_millis(200));
        let reopened = Arc::new(TextFocusObserver::default());
        pair.phone.set_observer(Some(reopened.clone()));
        pair.pump(Duration::from_millis(200));
        assert_eq!(*reopened.seen.lock().unwrap(), [(PC, field(3, true, false))]);
        pair.heartbeat_to_phone();
        assert_eq!(reopened.seen.lock().unwrap().len(), 1);
        pair.phone.stop();
    }

    #[test]
    fn opening_reports_controllability_and_this_phones_instance() {
        let pair = Pair::new();
        let opening = pair.phone.opening(true);
        assert!(matches!(&opening[0], v2::Message::InputCapability(c) if c.controllable() == Ok(true)));
        assert!(opening.iter().any(|m| matches!(m, v2::Message::ControlFocusState(_))));
        pair.phone.stop();
    }

    /// 10 host units per millimetre at the default tracking speed (step 4).
    const TEN_PER_MM: ControlSettings = ControlSettings {
        pointer_speed: 4,
        points_per_mm: 10.0,
    };

    #[test]
    fn pointer_interval_is_the_touch_time_and_at_least_one_report_period() {
        assert!((pointer_interval(1_016_000, 1_000_000) - 0.016).abs() < 1e-12);
        // Two movements a few microseconds apart (one frame): one report period, as on macOS.
        assert_eq!(pointer_interval(1_000_005, 1_000_000), REPORT_PERIOD);
        assert_eq!(pointer_interval(1_000_000, 1_000_000), REPORT_PERIOD);
        // A timestamp running backwards is one period, not a huge interval.
        assert_eq!(pointer_interval(1_000_000, 1_000_100), REPORT_PERIOD);
        // A finger resting before it moves: the whole rest counts (slow, no acceleration).
        assert!((pointer_interval(3_000_000, 1_000_000) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn default_speed_is_the_macos_default_tracking_speed() {
        assert_eq!(ControlSettings::default().pointer_speed, 4);
    }

    #[test]
    fn pointer_units_are_the_mac_curve_over_the_finger_millimetres() {
        // Host units become millimetres first: 10 units = 1 mm.
        let dt = pointer_interval(1_016_667, 1_000_000);
        assert_eq!(
            pointer_units(&TEN_PER_MM, 10.0, -5.0, dt),
            trackpad::pointer_points(&Curve::pointer(4), 1.0, -0.5, dt)
        );
        // 0.5 mm in one report period is 60 mm/s: 7.07 points per mm at the
        // default step (mac-curve-compare.out.txt).
        let (x, y) = pointer_units(&TEN_PER_MM, 5.0, 0.0, REPORT_PERIOD);
        assert!((x / 0.5 - 7.07).abs() < 0.006, "{x}");
        assert_eq!(y, 0.0);
        // The speed setting is the tracking-speed step: step 10 at 60 mm/s is 21.70.
        let fast = ControlSettings {
            pointer_speed: 10,
            ..TEN_PER_MM
        };
        assert!((pointer_units(&fast, 5.0, 0.0, REPORT_PERIOD).0 / 0.5 - 21.70).abs() < 0.006);
    }

    #[test]
    fn movements_close_together_are_not_faster_than_one_report_period() {
        // One frame's movement split over two calls microseconds apart: no
        // more than the same movement over one report period gives.
        let one = pointer_interval(1_000_003, 1_000_000);
        assert_eq!(
            pointer_units(&TEN_PER_MM, 5.0, 0.0, one),
            pointer_units(&TEN_PER_MM, 5.0, 0.0, REPORT_PERIOD)
        );
        // The first movement of a touch is timed from the touch-down: 1.5 mm
        // 200 ms after landing is slow, so it gains less than in one period.
        let first = pointer_units(&TEN_PER_MM, 15.0, 0.0, pointer_interval(10_200_000, 10_000_000)).0;
        assert!(first < pointer_units(&TEN_PER_MM, 15.0, 0.0, REPORT_PERIOD).0);
    }

    fn pad(time_us: u64, touches: &[(u64, f32, f32)]) -> PadTouches {
        PadTouches {
            time_us,
            width: 60.0,
            height: 70.0,
            touches: touches.iter().map(|&(id, x, y)| PadTouch { id, x, y }).collect(),
            cancelled: false,
        }
    }

    #[test]
    fn touches_become_a_contacts_sample_in_hundredths_of_a_millimetre() {
        let sample = contacts_sample(&pad(5, &[(0, 10.0, 20.5), (7, 59.999, 80.0)]));
        assert!(sample.valid());
        assert_eq!(sample.time_us, 5);
        assert_eq!((sample.surface.width, sample.surface.height), (6000, 7000));
        assert_eq!(sample.len, 2);
        assert_eq!(
            sample.contacts[0],
            Contact {
                id: 1,
                x: 1000,
                y: 2050
            }
        );
        // Clamped to the surface.
        assert_eq!(
            sample.contacts[1],
            Contact {
                id: 8,
                x: 6000,
                y: 7000
            }
        );
        assert!(contacts_sample(&pad(5, &[])).valid());
        // Six fingers cannot be described; a cancel is never a valid sample.
        let six: Vec<(u64, f32, f32)> = (0..6).map(|i| (i, 1.0, 1.0)).collect();
        assert!(!contacts_sample(&pad(5, &six)).valid());
        let mut cancelled = pad(5, &[(1, 1.0, 1.0)]);
        cancelled.cancelled = true;
        assert!(!contacts_sample(&cancelled).valid());
    }

    /// The PC's own key press makes it the group focus (epoch 1).
    fn focus_pc(pair: &mut Pair) {
        pair.pc.on_local_event(
            &CapturedEvent {
                event: InputEvent::Key {
                    code: KeyCode::keyboard(0x04),
                    pressed: true,
                },
                physical: true,
                time_us: 0,
                cursor: Some(Point::new(500., 400.)),
                source: SourceKind::Keyboard,
            },
            2,
        );
        pair.pump(Duration::from_millis(200));
        assert_eq!(pair.pc.focus_claim().1, PC);
        pair.injected.clear();
    }

    #[test]
    fn touchpad_scroll_reaches_the_focus_computer_with_phases_and_momentum() {
        let mut pair = Pair::new();
        focus_pc(&mut pair);
        let mut t = 1_000_000;
        pair.phone.touches(pad(t, &[(1, 20.0, 30.0), (2, 35.0, 30.0)]));
        pair.phone.scroll(FingerPhase::Began, 0.0, 10.0, t + 8_333, t);
        t += 8_333;
        for _ in 0..6 {
            pair.phone.scroll(FingerPhase::Changed, 0.0, 10.0, t + 8_333, t);
            t += 8_333;
        }
        pair.phone.scroll(FingerPhase::Ended, 0.0, 0.0, t + 2_000, t);
        pair.phone.touches(pad(t + 2_000, &[]));
        pair.pump(Duration::from_millis(300));
        let phases: Vec<ScrollPhase> = pair
            .injected
            .iter()
            .filter_map(|event| match event {
                InjectEvent::TouchpadScroll { wheel, .. } => Some(wheel.phase),
                _ => None,
            })
            .collect();
        assert_eq!(phases.first(), Some(&ScrollPhase::Began), "{phases:?}");
        let ended = phases.iter().position(|p| *p == ScrollPhase::Ended).expect("Ended");
        assert_eq!(phases[ended + 1], ScrollPhase::MomentumBegan, "{phases:?}");
        assert!(
            phases[ended + 2..phases.len() - 1]
                .iter()
                .all(|p| *p == ScrollPhase::Momentum)
        );
        assert!(phases.len() > ended + 5, "the fling glides: {phases:?}");
        assert_eq!(phases.last(), Some(&ScrollPhase::MomentumEnded));
        pair.phone.stop();
    }

    #[test]
    fn a_new_touch_ends_the_momentum_and_gestures_reach_the_focus_computer() {
        let mut pair = Pair::new();
        focus_pc(&mut pair);
        let mut t = 1_000_000;
        pair.phone.scroll(FingerPhase::Began, 0.0, 10.0, t + 8_333, t);
        t += 8_333;
        pair.phone.scroll(FingerPhase::Changed, 0.0, 10.0, t + 8_333, t);
        t += 8_333;
        pair.phone.scroll(FingerPhase::Ended, 0.0, 0.0, t + 1_000, t);
        pair.phone.touches(pad(t + 20_000, &[(3, 20.0, 30.0)]));
        pair.pump(Duration::from_millis(300));
        let phases: Vec<ScrollPhase> = pair
            .injected
            .iter()
            .filter_map(|event| match event {
                InjectEvent::TouchpadScroll { wheel, .. } => Some(wheel.phase),
                _ => None,
            })
            .collect();
        assert_eq!(phases.last(), Some(&ScrollPhase::MomentumEnded), "{phases:?}");
        assert!(phases.len() < 20, "the touch stopped the glide early: {phases:?}");
        pair.injected.clear();
        for phase in [GesturePhase::Began, GesturePhase::Ended] {
            pair.phone.gesture(Gesture::Magnify { phase, amount: 0.1 });
        }
        pair.pump(Duration::from_millis(200));
        assert_eq!(
            pair.injected
                .iter()
                .filter(|event| matches!(event, InjectEvent::Gesture(Gesture::Magnify { .. })))
                .count(),
            2
        );
        pair.phone.stop();
    }
    /// What the control page does with the shared recognizer's output (the
    /// FFI's `send_recognized`).
    fn forward(phone: &InputRouter, out: Vec<crate::trackpad::gestures::Recognized>) {
        use crate::trackpad::gestures::Recognized;
        for recognized in out {
            match recognized {
                Recognized::Gesture(gesture) => phone.gesture(gesture),
                Recognized::Scroll {
                    phase,
                    dx,
                    dy,
                    time_us,
                    since_us,
                } => phone.scroll_mm(phase, dx, dy, time_us, since_us),
            }
        }
    }

    #[test]
    fn recognised_two_finger_scrolls_and_dock_gestures_reach_the_focus_computer() {
        use crate::trackpad::gestures::{Claim, Finger, Recognizer};
        let mut pair = Pair::new();
        focus_pc(&mut pair);
        let mut recognizer = Recognizer::default();
        let mut frame = |pair: &Pair, time_us: u64, fingers: &[Finger]| {
            let mut out = Vec::new();
            let claim = recognizer.frame(time_us, fingers, false, true, &mut out);
            forward(&pair.phone, out);
            claim
        };
        // Two fingers move up 1.5 mm per 8.3 ms and lift: a scroll in
        // millimetres, accelerated, with momentum after the lift.
        let mut t = 1_000_000;
        let mut claim = Claim::Unclaimed;
        for k in 0..12 {
            let y = 40.0 - 1.5 * f64::from(k);
            claim = frame(&pair, t, &[Finger { id: 1, x: 20.0, y }, Finger { id: 2, x: 35.0, y }]);
            t += 8_333;
        }
        assert_eq!(claim, Claim::Scroll);
        frame(&pair, t, &[]);
        pair.pump(Duration::from_millis(300));
        let phases: Vec<ScrollPhase> = pair
            .injected
            .iter()
            .filter_map(|event| match event {
                InjectEvent::TouchpadScroll { wheel, .. } => Some(wheel.phase),
                _ => None,
            })
            .collect();
        assert_eq!(phases.first(), Some(&ScrollPhase::Began), "{phases:?}");
        let ended = phases.iter().position(|p| *p == ScrollPhase::Ended).expect("Ended");
        assert_eq!(phases[ended + 1], ScrollPhase::MomentumBegan, "{phases:?}");

        // Four fingers pinch in quickly: Launchpad's Dock gesture, Began to Ended.
        pair.injected.clear();
        let hand = |r: f64| -> Vec<Finger> {
            (0..4u8)
                .map(|i| {
                    let a = std::f64::consts::FRAC_PI_2 * f64::from(i) + 0.3;
                    Finger {
                        id: 10 + u64::from(i),
                        x: 30.0 + r * a.cos(),
                        y: 30.0 + r * a.sin(),
                    }
                })
                .collect()
        };
        t += 1_000_000;
        for k in 0..30 {
            claim = frame(&pair, t, &hand(20.0 - 0.4 * f64::from(k)));
            t += 8_000;
        }
        assert_eq!(claim, Claim::Gesture);
        frame(&pair, t, &[]);
        pair.pump(Duration::from_millis(200));
        let docks: Vec<GesturePhase> = pair
            .injected
            .iter()
            .filter_map(|event| match event {
                InjectEvent::Gesture(Gesture::DockSwipe { phase, .. }) => Some(*phase),
                _ => None,
            })
            .collect();
        assert_eq!(docks.first(), Some(&GesturePhase::Began), "{docks:?}");
        assert_eq!(docks.last(), Some(&GesturePhase::Ended), "{docks:?}");
        pair.phone.stop();
    }
    #[cfg(test)]
    mod group_screen_activity_tests {
        use super::*;

        #[derive(Default)]
        struct Screen(Mutex<Vec<bool>>);
        impl ScreenSink for Screen {
            fn focus(&self, _: bool) {}
            fn inject(&self, _: InjectEvent) {
                panic!("group activity must not inject input");
            }
            fn release_all(&self) {}
            fn group_activity(&self, active: bool) {
                self.0.lock().unwrap().push(active);
            }
        }

        #[test]
        fn independent_sources_expire_and_disconnect_without_latching_the_screen() {
            let pair = Pair::new();
            let mut worker = Worker::new(
                boot(PHONE),
                pair.net.clone(),
                Arc::default(),
                Arc::default(),
                Arc::default(),
            );
            let screen = Arc::new(Screen::default());
            worker.screen = Some(screen.clone());
            let other = DeviceId([9; 32]);
            worker
                .group_activity
                .insert(PC, Instant::now() - Duration::from_millis(1));
            worker
                .group_activity
                .insert(other, Instant::now() + GROUP_ACTIVITY_FRESH);
            worker.expire_group_activity();
            assert!(screen.0.lock().unwrap().is_empty());
            worker.remove_group_activity(other);
            assert_eq!(*screen.0.lock().unwrap(), [false]);
            worker
                .group_activity
                .insert(PC, Instant::now() - Duration::from_millis(1));
            worker.expire_group_activity();
            assert_eq!(*screen.0.lock().unwrap(), [false, false]);
            pair.phone.stop();
        }

        #[test]
        fn only_authenticated_current_activity_pulses_renew_not_focus_heartbeats() {
            let pair = Pair::new();
            let mut worker = Worker::new(
                boot(PHONE),
                pair.net.clone(),
                Arc::default(),
                Arc::default(),
                Arc::default(),
            );
            worker.sync_links();
            for packet in pair.pc.input_opening(true) {
                if let Some(message) = encode(&packet) {
                    worker.receive(boot(PC), LINK, message, false);
                }
            }
            worker.flush();
            let screen = Arc::new(Screen::default());
            worker.screen = Some(screen.clone());
            let pulse = || encode(&Packet::Control(ControlMsg::UserActivity)).unwrap();
            // A different Boot or a pulse queued before emergency pause cannot hold the screen.
            worker.receive(boot(DeviceId([9; 32])), LINK, pulse(), false);
            worker.receive(boot(PC), LINK, pulse(), true);
            for packet in pair.pc.heartbeat_messages(PHONE) {
                if let Some(message) = encode(&packet) {
                    worker.receive(boot(PC), LINK, message, false);
                }
            }
            worker.flush();
            assert!(worker.group_activity.is_empty());
            assert!(screen.0.lock().unwrap().is_empty());
            worker.receive(boot(PC), LINK, pulse(), false);
            worker.flush();
            assert_eq!(*screen.0.lock().unwrap(), [true]);
            assert!(worker.group_activity[&PC] <= Instant::now() + GROUP_ACTIVITY_FRESH);
            worker.command(Command::Emergency);
            assert_eq!(*screen.0.lock().unwrap(), [true, false]);
            worker.receive(boot(PC), LINK, pulse(), false);
            worker.flush();
            worker.command(Command::InputClosed(PC));
            assert_eq!(*screen.0.lock().unwrap(), [true, false, true, false]);
            pair.phone.stop();
        }
    }
}
