//! Pure input-routing state machine.
//!
//! 输入：本机截获事件（同步快路径，立即返回 [`Disposition`]）、对端消息、连接/断开、时钟。
//! 输出：[`Output`] 队列（发送消息、注入、切换截获模式、状态通知），由调用方用 [`Router::drain_outputs`]
//! 取走后异步派发。运行时状态转换不做 I/O、不读系统时钟；构造时生成一次随机进程实例标识。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::Packet as Message;
use crate::control::{ControlMsg, FocusScope};
use crate::input::{InputMsg, ScrollAmount};
use oink_foundation::display::DisplayInfo;
use oink_foundation::input::{
    CaptureMode, CapturedEvent, Disposition, FeelProfile, Gesture, GesturePhase, InjectEvent, InputEvent, InputMode,
    KeyCode, MouseButton, ScrollPhase, ScrollUnit, SourceKind,
};
use oink_foundation::{DeviceId, Point, Rect};

use crate::config::{ButtonHandoff, Os, RouterConfig};
use crate::hotkey::{HotkeyBinding, Hotkeys, Mods};
use crate::layout::{LayoutQuery, ScreenId, ScreenPos};

mod caps;
#[cfg(test)]
mod connection_tests;
mod contacts;
#[cfg(test)]
mod entry_policy_tests;
pub use contacts::ContactsPath;
mod emergency_input;
mod focus_lock;
pub use focus_lock::FocusLockError;
mod fresh_join;
mod game;
mod game_capture;
mod input_pair;
pub(crate) mod keyboard;
mod local_modes;
mod screen;
use game::GameState;
pub use game::{GameMode, GameModeStatus};
pub use game_capture::PointerCaptureObservation;
pub use local_modes::{SourceDecision, SourceDestination, SourceInput};
mod button_diagnostics;
mod dnd;
mod edge;
use edge::ExitEdge;
mod gap;
mod resistance;
mod trail;
mod volume_diagnostics;
pub use button_diagnostics::{ButtonHold, ButtonRefusal, ButtonSourcePress};
pub use dnd::{
    DndCmd, DndEvent, DndIntegrity, DndPreparation, DndSourceEffect, DndTarget, IntegrityOutcome, NativeObservation,
    PhysicalObservation, RolePartition, SourcePhase,
};
use resistance::Resistance;
pub use volume_diagnostics::{VolumeAction, VolumeDecision, VolumeDestination, VolumeOriginKind};

/// 单调时钟，微秒。由调用方提供（服务进程用真实时钟，模拟器用虚拟时钟）。
pub type Micros = u64;

/// 光标离屏幕边缘多近（本机单位）算「停在边上」：macOS 光标最多到 `right - ε`，Windows 到 `right - 1`。
const EDGE_UNITS: f64 = 1.0;

/// 焦点自愈：每隔几次心跳（250 ms × 2 = 500 ms）向每个已连接设备
/// 通告一次本机所知的焦点（`FocusState`）。
const FOCUS_GOSSIP_EVERY: u64 = 2;

/// 消息的发送方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// 不可靠、低延迟（QUIC datagram）：游戏模式 RelativeMotion。
    Datagram,
    /// 可靠，且**发往同一设备的所有 `Reliable` 消息保持发送顺序**（同一条有序流）：
    /// 焦点移交必须先于随后的按键到达。
    Reliable,
}

/// 路由的输出，由调用方异步执行。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionKind {
    Handoff,
    Refused,
    Lock,
    Unlock,
    ReturnHome,
    FocusAccepted,
    GapDepart,
    GapReturn,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionReason {
    EdgeCrossed,
    Locked,
    Paused,
    GameMode,
    HeldButtons,
    NotControllable,
    Resistance,
    PreviousInstance,
    Stale,
    Accepted,
    UserRequest,
    GapTravel,
    NoTarget,
}
/// Optional fixed-size evidence sink. Implementations must never allocate, wait or format here.
#[derive(Clone, Copy, Debug)]
pub enum TrailStage {
    Source,
    Converted,
}
#[derive(Clone, Copy, Debug)]
pub struct DepartureEvidence<'a> {
    pub pointer_source: Option<DeviceId>,
    pub revision: u64,
    pub bounds: Option<Rect>,
    pub point: Option<&'a ScreenPos>,
}
pub trait RoutingDiagnostics: Send + Sync {
    fn held_buttons(&self, _record: ButtonRefusal) {}
    fn volume(&self, _record: VolumeDecision) {}
    fn source_decision(&self, _record: local_modes::SourceDecision) {}
    fn enabled(&self) -> bool;
    /// Reviewed static state-transition text only; never key identities or formatted payloads.
    fn log(&self, _line: u32, _message: &'static str, _epoch: u64) {}
    #[allow(clippy::too_many_arguments)]
    fn state(
        &self,
        _phase: &'static str,
        _local: DeviceId,
        _epoch: u64,
        _revision: u64,
        _pointer: Option<&ScreenPos>,
        _exit: Option<&ScreenPos>,
        _reference: Option<&str>,
        _displays: &[DisplayInfo],
        _layout: &dyn LayoutQuery,
    ) {
    }
    #[allow(clippy::too_many_arguments)]
    fn crossing(
        &self,
        _layout: Arc<dyn LayoutQuery>,
        _epoch: u64,
        _from: &ScreenPos,
        _delta: Point,
        _local: bool,
        _gap: bool,
        _to: &ScreenPos,
        _departure: DepartureEvidence<'_>,
    ) {
    }
    /// Typed route metadata; the default preserves existing diagnostic sinks.
    #[allow(clippy::too_many_arguments)]
    fn crossing_with_zone(
        &self,
        layout: Arc<dyn LayoutQuery>,
        epoch: u64,
        from: &ScreenPos,
        delta: Point,
        local: bool,
        gap: bool,
        _trigger_zone: bool,
        to: &ScreenPos,
        departure: DepartureEvidence<'_>,
    ) {
        self.crossing(layout, epoch, from, delta, local, gap, to, departure);
    }
    fn trail(
        &self,
        _stage: TrailStage,
        _epoch: u64,
        _screen: &ScreenId,
        _anchor: Point,
        _scale: Point,
        _state: &crate::control::TrailContinuation,
    ) {
    }
    fn decision(
        &self,
        kind: DecisionKind,
        reason: DecisionReason,
        epoch: u64,
        holder: DeviceId,
        target: Option<&ScreenPos>,
    );
}
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// The audio owner validates the pinned claim/output before changing native volume.
    ExitVolumeKey {
        source: oink_foundation::session::Boot,
        route: crate::input::VolumeRoute,
        press_id: u64,
        code: KeyCode,
        pressed: bool,
        repeat: bool,
        modifiers: u8,
    },
    Send {
        to: DeviceId,
        msg: Message,
        delivery: Delivery,
    },
    /// 注入到本机。
    Inject(InjectEvent),
    /// 切换本机截获模式（焦点离开本机时 `Remote`，回到本机且不知道光标落点时 `Local`）。
    SetMode(CaptureMode),
    /// 焦点回到本机：切回 `Local`，光标出现在 `at`（`InputBackend::restore_local`，由 backend 原子地
    /// 「放光标、切模式、显示」）。与 `SetMode` 共用模式切换的顺序（执行方按产生顺序执行两者）。
    RestoreLocal {
        at: Point,
    },
    /// 给界面、日志的状态通知。
    Status(StatusEvent),
    /// 跨设备拖放的平台操作。执行方与注入、模式切换按产生顺序执行，完成后把
    /// 结果经 [`Router::on_dnd_event`] 交回。
    Dnd(DndCmd),
    /// 光标定位拖尾：本机开始 / 停止在本机光标后面绘制拖尾。
    /// 只在状态变化时输出；执行方自己跟踪本机光标的轨迹（[`Router::local_pointer`]）。
    CursorTrail {
        active: bool,
    },
    CursorTrailResume(crate::control::TrailContinuation),
    /// Resume in physical layout space, clipped to the receiving display.
    /// This is a local rendering action; the network continuation is unchanged.
    CursorTrailResumeClipped {
        state: crate::control::TrailContinuation,
        clip: Rect,
    },
    CursorTrailDepart {
        exit: Point,
    },
    /// Declare user activity without changing focus or injecting routed input.
    UserActivity {
        from: DeviceId,
        device_name: String,
    },
    /// Physical activity on this device, including input forwarded to a peer.
    LocalUserActivity,
    /// Refresh OS input-idle accounting for one real activity pulse. Never renewed
    /// by a timer, and gated by sharing plus `wake_displays_on_input`.
    ResetUserIdle,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StatusEvent {
    /// Cmd/Win volume control consumed on the focus source. The audio owner
    /// adjusts its local forwarding gain or toggles forwarding mute.
    AudioVolumeKey {
        code: KeyCode,
    },
    TouchpadContacts {
        mode: crate::contacts::Mode,
        frames: u64,
        gestures: u64,
        cancelled: u64,
        ended: bool,
    },
    LocalFactsChanged,
    KeyboardDisabled(bool),
    KeyboardReleased,
    ScreenLocalInput(bool),
    GameMode(GameModeStatus),
    /// Ordered local observation; snapshots can coalesce rapid lock transitions.
    FocusLock {
        holder: Option<DeviceId>,
    },
    Focus(FocusInfo),
    PeerTimedOut(DeviceId),
    PeerRecovered(DeviceId),
    /// 路由使用的布局重新编译了（显示器变化、设备上线 / 离线、布局被编辑）。
    LayoutChanged,
    /// Peer display metadata changed without changing the compiled topology.
    PeerDisplaysChanged(DeviceId),
    /// 紧急暂停：`origin` 上的用户长按了 Esc（或按了组合键），本机已暂停共享、回到本机。
    /// 服务进程据此保存暂停状态、通知界面（`origin` 是本机时把主窗口弹到前台）。
    EmergencyStop {
        origin: DeviceId,
    },
    /// 本机键盘按下了一个全局快捷键：这组按键已吞掉；焦点锁已在回调请求，其余由服务进程执行
    /// （界面 API 的 `HotkeyAction` 名字，例如 `"toggle_sharing"`）。
    Hotkey {
        action: String,
    },
}

/// 本机所知的焦点。
#[derive(Clone, Debug, PartialEq)]
pub struct FocusInfo {
    pub epoch: u64,
    pub holder: DeviceId,
    /// 焦点在本机时，指针所在的屏幕与位置。
    pub pointer: Option<ScreenPos>,
}

/// Group claim and its current incarnation proof, separate from a local island cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupFocusStatus {
    pub epoch: u64,
    pub holder: Option<DeviceId>,
    pub valid: bool,
    pub participating: bool,
    pub independent: bool,
    pub locked_device: Option<DeviceId>,
}

impl Default for GroupFocusStatus {
    fn default() -> Self {
        Self {
            epoch: 0,
            holder: None,
            valid: false,
            participating: false,
            independent: true,
            locked_device: None,
        }
    }
}

// One target, independent finger and momentum lifetimes. Native traces showed a new
// finger scroll while the previous source momentum was still being delivered.
#[derive(Clone, Copy, Debug)]
struct ScrollRoute<T> {
    target: T,
    fingers: bool,
    momentum: bool,
}
impl<T: Copy> ScrollRoute<T> {
    fn new(target: T) -> Self {
        Self {
            target,
            fingers: true,
            momentum: false,
        }
    }
    fn phase(&mut self, phase: ScrollPhase) -> bool {
        match phase {
            ScrollPhase::Began => self.fingers = true,
            ScrollPhase::Ended | ScrollPhase::Cancelled => self.fingers = false,
            ScrollPhase::MomentumBegan => self.momentum = true,
            ScrollPhase::MomentumEnded => self.momentum = false,
            _ => {}
        }
        matches!(phase, ScrollPhase::Cancelled | ScrollPhase::MomentumEnded) && !self.fingers && !self.momentum
    }
}

/// 源端：一个按下的键 / 按钮发往了哪里（按下时的目标即抬起时的目标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Target {
    /// 放行给本机系统。
    Local,
    /// 由路由注入到本机（焦点回到本机时补发的修饰键）。
    LocalInjected,
    Remote(DeviceId),
    /// 目标设备已断开：抬起时静默吞掉。
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum HeldItem {
    Key(KeyCode),
    Button(MouseButton),
}

#[derive(Debug)]
struct Peer {
    touchpad_contacts: Option<u8>,
    connected: bool,
    name: String,
    os: Os,
    feel: FeelProfile,
    displays: Vec<DisplayInfo>,
    /// 上一次发给它的 HeldSync 非空（变空时再发一次空的）。
    held_sync_nonempty: bool,
    /// SessionInit confirms whether this endpoint can receive input.
    sharing: bool,
    /// 当前对端路由实例；连接变化不会清除，FocusState 持续确认。
    session: Option<[u8; 16]>,
    /// 已收到本次连接的初始焦点／实例通告，可以向它移交。
    focus_ready: bool,
    /// 防止乱序通告把已经替换的旧实例复活。
    retired_sessions: HashSet<[u8; 16]>,
}

impl Peer {
    fn new(_now: Micros) -> Self {
        Self {
            touchpad_contacts: None,
            connected: false,
            name: String::new(),
            os: Os::Other,
            feel: FeelProfile::default(),
            displays: Vec::new(),
            held_sync_nonempty: false,
            sharing: true,
            session: None,
            focus_ready: false,
            retired_sessions: HashSet::new(),
        }
    }
}

/// 目标端：为某个源（`origin`）注入并仍按着的键和按钮。
#[derive(Debug, Default)]
struct OriginHeld {
    /// 源的键码 → 按下时实际注入的键码（输入转换后）。
    keys: HashMap<KeyCode, KeyCode>,
    buttons: HashSet<MouseButton>,
    /// 按着的键已注入的重复序号（`KeyRepeat::n`），用于丢弃重复投递。
    repeats: HashMap<KeyCode, u32>,
}

#[derive(Clone, Debug)]
enum VolumeTarget {
    Exit {
        route: crate::input::VolumeRoute,
        press_id: u64,
    },
    Forwarding,
}

/// 输入路由状态机。每台设备一个。
pub struct Router {
    cfg: RouterConfig,
    admitted_boots: HashMap<DeviceId, oink_foundation::session::Boot>,
    /// 当前使用的布局查询（配置模块编译后发布）。
    layout: Arc<dyn LayoutQuery>,
    game: GameState,
    last_game_status: Option<GameModeStatus>,
    /// 光标定位拖尾：源端的画圈识别与焦点机的绘制状态。
    trail: trail::TrailState,
    gap: Option<gap::GapTravel>,
    gap_input: gap::GapInput,
    last_user_activity: Option<Micros>,
    last_local_user_activity: Option<Micros>,
    update_remote_until: Micros,
    /// Group-visible manual focus lock, including the last unlock tombstone.
    focus_lock: Option<crate::control::FocusLock>,
    /// 共享暂停：本机自成一岛，不接受其他设备的焦点与输入。
    paused: bool,
    fully_paused: bool,
    projection_gesture: bool,
    capture_available: bool,
    keyboard: keyboard::Keyboard,
    screen_local: bool,
    last_enabled_focus: Option<DeviceId>,
    displays: Vec<DisplayInfo>,
    display_revision: u64,
    feel: FeelProfile,
    edge_regions: Vec<oink_foundation::input::EdgeResistanceRegion>,

    // ---- 焦点 ----
    epoch: u64,
    holder: DeviceId,
    /// 本次 Router 的身份，重启换新；链路变化保留。
    session: [u8; 16],
    /// 产生当前 claim 时的参与实例快照，gossip 原样保存。
    focus_scope: Option<FocusScope>,
    /// This incarnation has participated in an explicitly scoped focus claim.
    focus_joined: bool,
    fresh_join: bool,
    /// 本机光标位置（焦点在本机时即全局指针）。
    pointer: Option<ScreenPos>,
    local_cursor: Option<ScreenPos>,
    /// A shared pointer departed; own mouse-local use can reveal this cursor
    /// independently of the group holder, including during gap travel.
    local_mouse_hidden: bool,
    /// 焦点离开本机时的位置：收回焦点时光标回到这里。
    exit_point: Option<ScreenPos>,
    last_pointer_source: Option<DeviceId>,
    /// 本机作为源时换算位移的参考屏。
    ref_display: Option<String>,
    /// 当前外缘上的本机阻力累计（达标前焦点仍在本机）。
    resistance: Option<Resistance>,
    /// 本机所知的组内焦点：第 `epoch` 次移交后的焦点机。通常与 `holder` 相同；焦点机断线、
    /// 暂停、没有确认移交时本机只在本机收回焦点（「岛」），这时 `holder` 是本机而 `claim` 仍是它。
    /// 通告（`FocusState`）里发的是 `claim`，所以岛不会把焦点「抢」过来。
    claim: DeviceId,
    /// 本机是源、而焦点机通告的焦点落后于本机（它可能没收到 `FocusEnter`）：从何时开始。
    /// 超过心跳超时仍未确认就只在本机收回焦点。
    unconfirmed_since: Option<Micros>,
    /// 本机是因为连接问题（焦点机断线、超时、单向不通、没有确认移交）才成为岛的，并且之后本机没有
    /// 用过键鼠：焦点机恢复且仍持有焦点时重新加入（本机再次作为源）。
    rejoin: bool,
    /// 本机是从第三台设备的 `FocusState` 得知自己是第 `epoch` 次移交的焦点机、就此接过焦点的，交出方的
    /// `FocusEnter { epoch }` 还没到（直连慢了一点，别的设备的通告先到）。它到达时照常处理（光标放到进入点、
    /// 采用键盘状态、带着的拖拽开始），而不是当作已知的焦点忽略。本机用过自己的键鼠、焦点变化时清除。
    enter_due: Option<u64>,

    // ---- 紧急暂停 ----
    /// 本机因紧急暂停而暂停：触发它的设备。
    emergency: Option<DeviceId>,
    /// 本机键盘上的 Esc 从何时开始按住（长按触发）。
    esc_down_at: Option<Micros>,
    physical_keys: HashSet<KeyCode>,
    escape_consumed: bool,
    pause_all_consumed: HashSet<KeyCode>,
    volume_exit: Option<Arc<dyn Fn() -> Option<crate::input::VolumeRoute> + Send + Sync>>,
    volume_press_id: u64,
    volume_keep_local: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    volume_notifications:
        Option<Arc<dyn Fn() -> Option<oink_foundation::input::RecentAudioNotifications> + Send + Sync>>,
    local_volume_origin: Option<crate::input::VolumeKeyOrigin>,
    remote_volume_origins: HashMap<DeviceId, (crate::input::VolumeKey, crate::input::VolumeKeyOrigin)>,
    volume_targets: HashMap<(DeviceId, KeyCode), VolumeTarget>,
    trail_probe: Option<Arc<dyn Fn(Point) -> Option<crate::control::TrailContinuation> + Send + Sync>>,
    /// 全局快捷键：在本机键盘上识别，不论焦点在哪台设备。
    hotkeys: Hotkeys,

    // ---- 源端 ----
    keys: HashMap<KeyCode, Vec<Target>>,
    buttons: HashMap<MouseButton, Target>,
    button_diagnostics: button_diagnostics::State,
    scroll_target: Option<ScrollRoute<Target>>,
    /// 一次触摸板手势（`InputEvent::Gesture`）从开始到结束锁定的目标。
    gesture_lock: Option<Target>,
    source_gesture: Option<Gesture>,
    motion_seq: u64,
    /// 按着的键已发出的自动重复次数（`KeyRepeat::n`）。
    key_repeats: HashMap<KeyCode, u32>,
    caps_press: Option<caps::CapsPress>,
    mac_caps_switch: bool,

    // ---- 目标端 ----
    held: HashMap<DeviceId, OriginHeld>,
    key_refs: HashMap<KeyCode, u32>,
    input_transforms: HashMap<DeviceId, oink_foundation::input_transform::InputTransform>,
    local_mapped: HashMap<KeyCode, KeyCode>,
    local_rewrite: Option<oink_foundation::input::LocalRewrite>,
    button_refs: HashMap<MouseButton, u32>,
    /// 本机已不是焦点机时转发出去的按下：(源, 键/按钮) → 转发目标，抬起走同一路径。
    forwarded: HashMap<(DeviceId, HeldItem), DeviceId>,
    /// Actual previous hops for each remote press, including a direct modifier snapshot.
    press_routes: HashMap<(DeviceId, HeldItem), HashSet<DeviceId>>,
    /// Keep empty routes too: a lost forwarded release must heal on later HeldSyncs.
    forwarded_sync: HashSet<(DeviceId, DeviceId)>,
    /// 已开始且焦点未变化的滚动：源 → 实际接收端（包括本机）；换焦点即清空。
    scroll_fwd: HashMap<DeviceId, ScrollRoute<DeviceId>>,
    /// 触摸板 Gesture 仍保留原始转发目标，不采用滚动的丢弃规则。
    gesture_fwd: HashMap<DeviceId, DeviceId>,
    /// 为某个源注入、还没结束的手势（源断线时补一个取消，Dock 不会停在半途）。
    gesture_active: HashMap<DeviceId, Gesture>,
    touchpad_contacts: Option<u8>,
    accept_contacts: bool,
    multifinger: HashMap<DeviceId, contacts::MultiFinger>,
    pending_contacts: contacts::PendingContacts,
    contacts: crate::contacts::Source,
    contacts_target: Option<DeviceId>,
    contacts_received: Option<(DeviceId, u32)>,
    contacts_observed: bool,
    contacts_healthy: bool,

    // ---- 跨设备拖放 ----
    dnd: dnd::Dnd,

    // ---- 对端 ----
    peers: BTreeMap<DeviceId, Peer>,
    sync_seq: u64,
    next_sync: Option<Micros>,

    out: Vec<Output>,
    diagnostics: Option<Arc<dyn RoutingDiagnostics>>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("device", &self.cfg.device)
            .field("epoch", &self.epoch)
            .field("holder", &self.holder)
            .field("pointer", &self.pointer)
            .finish_non_exhaustive()
    }
}

impl Router {
    pub fn new(cfg: RouterConfig, layout: Arc<dyn LayoutQuery>) -> Self {
        let me = cfg.device;
        let mut session = [0; 16];
        getrandom::fill(&mut session).expect("routing session entropy");
        let admitted_boots = HashMap::from([(me, cfg.boot)]);
        let game = GameState::for_owner(cfg.settings.game_mode_preference);
        let mut router = Self {
            cfg,
            admitted_boots,
            layout,
            game,
            last_game_status: None,
            trail: Default::default(),
            gap: None,
            gap_input: Default::default(),
            last_user_activity: None,
            last_local_user_activity: None,
            update_remote_until: 0,
            focus_lock: None,
            paused: false,
            fully_paused: false,
            projection_gesture: false,
            capture_available: true,
            keyboard: Default::default(),
            screen_local: false,
            last_enabled_focus: None,
            displays: Vec::new(),
            display_revision: 0,
            feel: FeelProfile::default(),
            epoch: 0,
            holder: me,
            session,
            focus_scope: None,
            focus_joined: false,
            fresh_join: true,
            pointer: None,
            local_cursor: None,
            local_mouse_hidden: false,
            exit_point: None,
            last_pointer_source: None,
            ref_display: None,
            resistance: None,
            edge_regions: Vec::new(),
            claim: me,
            unconfirmed_since: None,
            rejoin: false,
            enter_due: None,
            emergency: None,
            esc_down_at: None,
            physical_keys: HashSet::new(),
            escape_consumed: false,
            pause_all_consumed: HashSet::new(),
            volume_exit: None,
            volume_press_id: 0,
            volume_keep_local: None,
            volume_notifications: None,
            local_volume_origin: None,
            remote_volume_origins: HashMap::new(),
            volume_targets: HashMap::new(),
            trail_probe: None,
            hotkeys: Hotkeys::default(),
            keys: HashMap::new(),
            buttons: HashMap::new(),
            button_diagnostics: Default::default(),
            scroll_target: None,
            gesture_lock: None,
            source_gesture: None,
            motion_seq: 0,
            key_repeats: HashMap::new(),
            caps_press: None,
            mac_caps_switch: false,
            held: HashMap::new(),
            key_refs: HashMap::new(),
            input_transforms: HashMap::new(),
            local_mapped: HashMap::new(),
            local_rewrite: None,
            button_refs: HashMap::new(),
            forwarded: HashMap::new(),
            press_routes: HashMap::new(),
            forwarded_sync: HashSet::new(),
            scroll_fwd: HashMap::new(),
            gesture_fwd: HashMap::new(),
            gesture_active: HashMap::new(),
            touchpad_contacts: None,
            accept_contacts: true,
            multifinger: HashMap::new(),
            pending_contacts: contacts::PendingContacts::default(),
            contacts: Default::default(),
            contacts_target: None,
            contacts_received: None,
            contacts_observed: false,
            contacts_healthy: false,
            dnd: dnd::Dnd::default(),
            peers: BTreeMap::new(),
            sync_seq: 0,
            next_sync: None,
            out: Vec::new(),
            diagnostics: None,
        };
        router.last_game_status = Some(router.game_mode());
        router
    }

    // ================= 公共接口 =================

    /// 本次路由进程实例，用于诊断与显式构造测试移交；重连时不变。
    pub fn routing_session(&self) -> [u8; 16] {
        self.session
    }

    pub fn device(&self) -> DeviceId {
        self.cfg.device
    }

    pub fn config(&self) -> &RouterConfig {
        &self.cfg
    }

    pub fn set_settings(&mut self, settings: crate::config::RoutingSettings) {
        self.resistance = None;
        let dnd_was = self.dnd_enabled();
        self.apply_source_modes(settings.mouse_mode, settings.keyboard_mode);
        self.set_accept_contacts(settings.accept_contacts);
        if self.cfg.settings.test_mac_scroll != settings.test_mac_scroll {
            self.retire_received_multifinger();
        }
        if self.cfg.settings.forward_gestures && !settings.forward_gestures {
            self.cancel_source_contacts();
            self.cancel_local_gesture();
            if self
                .scroll_target
                .is_some_and(|route| matches!(route.target, Target::Remote(_)))
            {
                self.cancel_source_scroll();
            }
        }
        let preference_changed = self.cfg.settings.game_mode_preference != settings.game_mode_preference;
        self.cfg.settings = settings;
        if preference_changed && self.holder == self.me() {
            self.apply_game_preference();
        }
        if self.dnd_enabled() != dnd_was {
            if !self.dnd_enabled() {
                self.dnd_abort("drag and drop turned off");
            }
            self.broadcast_dnd_caps();
        }
    }

    /// 使用调用方管理的布局（测试、模拟器）。之后路由不再维护布局数据。
    pub fn set_layout(&mut self, layout: Arc<dyn LayoutQuery>) {
        self.gap_cancel();
        self.resistance = None;
        self.layout = layout;
        self.validate_focus_lock_layout();
        self.enforce_screen_policy();
        self.diagnostic_state("layout_applied");
    }

    /// 本机显示器变化（启动时也调用一次）。会通告给已连接的设备。
    pub fn set_local_displays(&mut self, displays: Vec<DisplayInfo>) {
        self.resistance = None;
        if self.diagnostics.as_ref().is_some_and(|d| d.enabled()) && self.displays != displays {
            self.display_revision += 1;
        }
        self.displays = displays;
        if self.displays.is_empty() && self.locked_device() == Some(self.me()) {
            self.end_focus_lock();
        }
        self.gap_cancel();
        let me = self.me();
        let remap = |pos: &ScreenPos| {
            let display = self.displays.iter().find(|d| d.key == pos.screen.display).or_else(|| {
                self.displays.iter().min_by(|a, b| {
                    let distance = |d: &DisplayInfo| {
                        let p = d.bounds.clamp(pos.local);
                        (p.x - pos.local.x).hypot(p.y - pos.local.y)
                    };
                    distance(a).total_cmp(&distance(b))
                })
            })?;
            Some(ScreenPos::new(
                ScreenId::new(me, display.key.clone()),
                display.bounds.clamp(pos.local),
            ))
        };
        self.pointer = self.pointer.as_ref().and_then(remap);
        self.local_cursor = self.local_cursor.as_ref().and_then(remap);
        self.exit_point = self.exit_point.as_ref().and_then(remap);
        if self
            .ref_display
            .as_ref()
            .is_some_and(|key| !self.displays.iter().any(|d| &d.key == key))
        {
            self.ref_display = self
                .displays
                .iter()
                .find(|d| d.is_primary)
                .or(self.displays.first())
                .map(|d| d.key.clone());
        }
        if self.holder != me && (!self.mouse_local() || self.local_mouse_hidden) && !self.displays.is_empty() {
            // Refresh the native source park/owned clip too; its old display may
            // have disappeared or its center may be outside the new mode.
            self.out.push(Output::SetMode(CaptureMode::Remote {
                park: self.park_point(),
            }));
        }
        self.diagnostic_state("displays_applied");
        self.out.push(Output::Status(StatusEvent::LocalFactsChanged));
    }

    /// Query the local audio worker’s live forwarding destination.
    pub fn set_diagnostics(&mut self, diagnostics: Arc<dyn RoutingDiagnostics>) {
        self.button_diagnostics.end_episode();
        self.diagnostics = Some(diagnostics);
    }
    fn diagnostic_log(&self, line: u32, message: &'static str, epoch: u64) {
        if let Some(sink) = &self.diagnostics
            && sink.enabled()
        {
            sink.log(line, message, epoch);
        }
    }
    fn diagnostic_state(&self, phase: &'static str) {
        if let Some(sink) = &self.diagnostics
            && sink.enabled()
        {
            sink.state(
                phase,
                self.me(),
                self.epoch,
                self.display_revision,
                self.pointer.as_ref(),
                self.exit_point.as_ref(),
                self.cfg
                    .settings
                    .fixed_reference_display
                    .as_deref()
                    .or(self.ref_display.as_deref())
                    .or_else(|| self.pointer.as_ref().map(|p| p.screen.display.as_str()))
                    .or_else(|| self.displays.first().map(|d| d.key.as_str())),
                &self.displays,
                self.layout.as_ref(),
            );
        }
    }
    fn departure_evidence(&self, from: &ScreenPos) -> DepartureEvidence<'_> {
        DepartureEvidence {
            pointer_source: self.last_pointer_source,
            revision: self.display_revision,
            bounds: self
                .displays
                .iter()
                .find(|d| d.key == from.screen.display)
                .map(|d| d.bounds),
            point: self.pointer.as_ref(),
        }
    }
    fn diagnostic(&self, kind: DecisionKind, reason: DecisionReason, target: Option<&ScreenPos>) {
        if reason != DecisionReason::HeldButtons {
            self.button_diagnostics.end_episode();
        }
        if let Some(sink) = &self.diagnostics {
            if kind == DecisionKind::Refused && reason == DecisionReason::HeldButtons {
                self.diagnostic_button_refusal(sink.as_ref());
            }
            sink.decision(kind, reason, self.epoch, self.holder, target);
        }
    }
    fn refusal_reason(&self) -> DecisionReason {
        if self.locked() {
            DecisionReason::Locked
        } else if self.paused {
            DecisionReason::Paused
        } else if self.game_mode().active {
            DecisionReason::GameMode
        } else if self.handoff_blocked() {
            DecisionReason::HeldButtons
        } else {
            DecisionReason::NotControllable
        }
    }
    pub fn set_volume_exit_probe(&mut self, probe: Arc<dyn Fn() -> Option<crate::input::VolumeRoute> + Send + Sync>) {
        self.volume_exit = Some(probe);
    }

    /// 设置本机的全局快捷键（启动时与设置变化时调用，立即生效）。已经按下的快捷键不受影响：
    /// 它的抬起照常吞掉。
    pub fn set_hotkeys(&mut self, bindings: Vec<HotkeyBinding>) {
        self.hotkeys = Hotkeys::new(bindings);
    }

    /// Paused capture still observes every configured emergency method.
    pub fn emergency_observation_requested(&self) -> bool {
        self.cfg.settings.emergency_hold_ms > 0
            || self.cfg.settings.emergency_combo
            || self
                .hotkeys
                .bindings()
                .iter()
                .any(|binding| binding.action == "pause_all")
    }

    pub fn hotkeys(&self) -> &Hotkeys {
        &self.hotkeys
    }

    /// 本机手感档案变化（启动时也调用一次）。会通告给已连接的设备。
    pub fn set_local_feel(&mut self, feel: FeelProfile) {
        self.feel = feel;
        let msg = Message::Control(ControlMsg::Feel {
            profile: self.feel.clone(),
        });
        self.broadcast(msg);
    }

    /// 告诉路由本机光标当前位置（启动时，或平台实现发现光标被其他程序移动时）。
    pub fn set_cursor(&mut self, pos: Point) {
        let was_gap = self.gap.is_some();
        self.resistance = None;
        if let Some((key, bounds)) = self.locate(pos) {
            self.pointer = Some(ScreenPos::new(ScreenId::new(self.me(), key), bounds.clamp(pos)));
            self.local_cursor = self.pointer.clone();
        }
        if was_gap {
            self.gap_cancel();
        }
    }

    /// Whether the local source ledger still awaits a release, regardless of its target.
    /// Diagnostic only; this does not inspect or reconcile the platform's physical state.
    /// A native projection handle owns this gesture only; no persistent focus lock.
    pub fn set_projection_gesture(&mut self, active: bool) {
        self.projection_gesture = active;
    }

    /// All held input, including local pass-through keys and forwarded sequences.
    /// Used on the routing owner before update maintenance can release input.
    pub fn update_input_held(&self) -> bool {
        !self.keys.is_empty()
            || !self.buttons.is_empty()
            || !self.forwarded.is_empty()
            || self
                .held
                .values()
                .any(|held| !held.keys.is_empty() || !held.buttons.is_empty())
            || self.touchpad_contacts.is_some_and(|n| n > 0)
            || !self.gesture_active.is_empty()
            || self.scroll_target.is_some()
            || self.gesture_lock.is_some()
    }
    pub fn update_remote_control(&self, now: Micros) -> bool {
        self.holder != self.me() || self.claim != self.me() || now < self.update_remote_until
    }
    pub fn local_button_held(&self, button: MouseButton) -> bool {
        self.buttons.contains_key(&button)
    }

    pub fn focus(&self) -> FocusInfo {
        FocusInfo {
            epoch: self.epoch,
            holder: self.holder,
            pointer: if self.holder == self.me() {
                self.pointer.clone()
            } else {
                None
            },
        }
    }

    pub fn group_focus_status(&self) -> GroupFocusStatus {
        let valid =
            self.epoch > 0 && self.scope_allows(self.focus_scope.as_ref(), self.claim) && self.input_ready(self.claim);
        let participating = valid && self.focus_joined && !self.paused && self.holder == self.claim;
        let locked_device = self
            .focus_lock
            .as_ref()
            .filter(|lock| {
                let session = if lock.holder == self.me() {
                    Some(self.session)
                } else {
                    self.peers.get(&lock.holder).and_then(|peer| peer.session)
                };
                valid
                    && lock.locked
                    && lock.holder == self.claim
                    && lock.epoch <= self.epoch
                    && session == Some(lock.session)
                    && self.scope_allows(Some(&lock.scope), lock.holder)
            })
            .map(|lock| lock.holder);
        GroupFocusStatus {
            epoch: self.epoch,
            holder: (self.epoch > 0).then_some(self.claim),
            valid,
            participating,
            independent: !participating,
            locked_device,
        }
    }

    /// 某个对端是否已连接且心跳正常。
    pub fn peer_alive(&self, peer: DeviceId) -> bool {
        self.reachable(peer)
    }

    /// 暂停共享：本机自成一岛——焦点留在本机、不接受其他设备的焦点与输入（抬起除外），并通告
    /// 其他设备（session admission），它们把本机当作离线。
    pub fn set_paused(&mut self, paused: bool) {
        self.gap_cancel();
        self.resistance = None;
        if self.paused == paused {
            return;
        }
        if paused && self.locked_device() == Some(self.me()) {
            self.end_focus_lock();
        }
        if paused {
            self.cancel_caps();
        }
        self.paused = paused;
        self.last_user_activity = None;
        self.reset_game_focus();
        if !paused && self.holder == self.me() {
            self.apply_game_preference();
        }
        if paused {
            self.cancel_source_contacts();
            self.retire_received_multifinger();
            self.fresh_join = false;
            // 先取消拖拽：下面释放为其他设备按着的按钮时不能把拖拽放到光标下。
            self.dnd_abort("sharing paused");
            // 焦点在别处：只在本机收回（epoch 不变，不影响其他设备）。
            if self.holder != self.me() {
                let at = self.exit_point.clone().or_else(|| self.pointer.clone());
                let epoch = self.epoch;
                self.become_holder(epoch, at);
            }
            // 为其他设备按着的键和按钮全部抬起。
            let origins: Vec<DeviceId> = self.held.keys().copied().filter(|d| *d != self.me()).collect();
            for o in origins {
                self.release_origin(o);
            }
        }
    }

    /// Local owner pause destroys input ownership. Resuming this device cannot
    /// resume any other member.
    pub fn set_fully_paused(&mut self, paused: bool) {
        if self.fully_paused == paused {
            return;
        }
        self.fully_paused = paused;
        self.set_paused(paused);
        if paused {
            self.release_local_transformed();
            self.dnd_abort("device paused");
            let origins: std::collections::HashSet<_> =
                self.held.keys().chain(self.gesture_active.keys()).copied().collect();
            for origin in origins {
                self.release_origin(origin);
            }
            self.keys.clear();
            self.buttons.clear();
            self.button_diagnostics.release_origin(self.me());
            self.key_repeats.clear();
            self.forwarded.clear();
            self.press_routes.clear();
            self.forwarded_sync.clear();
            self.scroll_fwd.clear();
            self.gesture_fwd.clear();
            self.scroll_target = None;
            self.gesture_lock = None;
            self.enter_due = None;
            self.unconfirmed_since = None;
            self.rejoin = false;
            self.claim = self.me();
            self.holder = self.me();
            self.focus_scope = None;
            self.focus_joined = false;
            getrandom::fill(&mut self.session).expect("routing session entropy");
            self.out.retain(|out| !matches!(out, Output::Send { .. }));
        } else {
            self.emergency = None;
        }
    }

    pub fn fully_paused(&self) -> bool {
        self.fully_paused
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// 把指针召回本机（快捷键「把指针召回本机」）：焦点在其他设备上（包括本机正作为源、以及只在
    /// 本机收回了焦点的岛以外的一切情况）时收回到本机，光标出现在本机主屏中央；焦点已在本机时不动。
    /// 解除焦点锁后通告所有设备（epoch + 1）。
    pub fn return_pointer_home(&mut self) {
        if !self.paused && self.redirect_target(self.me()).is_some() {
            self.enforce_screen_policy();
            return;
        }
        self.end_focus_lock();
        self.diagnostic(DecisionKind::ReturnHome, DecisionReason::UserRequest, None);
        if self.pointer_in_gap() {
            self.pointer = self.primary_display_center().or_else(|| self.pointer.clone());
            self.gap_cancel();
            return;
        }
        if self.holder == self.me() {
            return;
        }
        self.resistance = None;
        self.dnd_abort("pointer called back to this device");
        let at = self
            .primary_display_center()
            .or_else(|| self.exit_point.clone())
            .or_else(|| self.pointer.clone());
        self.reclaim_focus(at);
    }

    /// Secure-desktop gaps end this keyboard/mouse's gestures immediately. This is
    /// a source gate, not sharing pause: other devices may still control this machine.
    pub fn set_capture_available(&mut self, available: bool) {
        if self.capture_available == available {
            return;
        }
        if !available {
            self.cancel_caps();
        }
        self.capture_available = available;
        if !available {
            if self.locked_device() == Some(self.me()) {
                self.end_focus_lock();
            }
            self.cancel_source_contacts();
        }
        if available {
            return;
        }
        let own_source =
            self.last_pointer_source == Some(self.me()) || !self.keys.is_empty() || !self.buttons.is_empty();
        if own_source || self.pointer_in_gap() {
            self.dnd_abort("local capture desktop unavailable");
        }
        self.release_local_transformed();
        let keys: Vec<_> = self.keys.drain().collect();
        for (code, targets) in keys {
            self.release_local_key(code, targets);
        }
        let buttons: Vec<_> = self.buttons.drain().collect();
        for (button, target) in buttons {
            self.release_local_button(button, target);
        }
        self.cancel_source_scroll();
        self.cancel_local_gesture();
        self.scroll_target = None;
        self.gesture_lock = None;
        self.esc_down_at = None;
        self.physical_keys.clear();
        self.escape_consumed = false;
        self.pause_all_consumed.clear();
        self.resistance = None;
        if (own_source || self.pointer_in_gap()) && !self.locked() {
            self.return_pointer_home();
        }
    }

    /// Platform reports a transition to all local displays asleep.
    pub fn on_displays_asleep(&mut self) {
        if self.locked_device() == Some(self.me()) {
            self.end_focus_lock();
        }
        if !self.cfg.settings.return_pointer_on_display_sleep
            || self.paused
            || self.holder != self.me()
            || self.claim != self.me()
        {
            return;
        }
        let Some(source) = self.last_pointer_source else {
            return;
        };
        if source == self.me() || !self.input_ready(source) {
            return;
        }
        let Some(peer) = self.peers.get(&source) else {
            return;
        };
        let Some(display) = peer
            .displays
            .iter()
            .find(|d| d.is_primary)
            .or_else(|| peer.displays.first())
        else {
            return;
        };
        let at = ScreenPos::new(ScreenId::new(source, display.key.clone()), display.bounds.center());
        if self.layout.source_requires_target(source) {
            return; // Pending geometry is allowed; an explicitly excluded physical source is not a target.
        }
        tracing::info!(
            ?source,
            epoch = self.epoch,
            "all displays asleep: returning pointer to source"
        );
        self.dnd_abort("all displays asleep");
        self.hand_off(at, self.trail_pointer());
    }

    /// 焦点在其他设备上时收回到本机（epoch + 1，通告所有设备），光标出现在 `at`。
    fn reclaim_focus(&mut self, at: Option<ScreenPos>) {
        let me = self.me();
        if self.holder == me && !self.fresh_join {
            return;
        }
        let epoch = self.epoch + 1;
        self.focus_scope = Some(self.new_focus_scope());
        self.focus_joined = true;
        self.fresh_join = false;
        let peers: Vec<DeviceId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(d, _)| *d)
            .collect();
        for d in peers {
            self.send(
                d,
                Message::Control(ControlMsg::FocusState {
                    lock_state: self.focus_lock.clone(),
                    epoch,
                    holder: me,
                    scope: self.focus_scope.clone(),
                    session: Some(self.session),
                }),
            );
        }
        self.claim = me;
        self.become_holder(epoch, at);
    }

    /// 本机所知的组内焦点 `(epoch, 焦点机)`。与 [`Router::focus`] 的 `holder` 不同时，本机是「岛」：
    /// 焦点机断线或暂停后只在本机收回了焦点。
    pub fn focus_claim(&self) -> (u64, DeviceId) {
        (self.epoch, self.claim)
    }

    // ================= 紧急暂停 =================

    /// 本机的用户要求紧急暂停（长按 Esc、组合键，或界面 / 测试直接调用）：本机立即暂停并回到本机，
    /// 不等网络；状态事件由 agent 配置 owner 发布为组暂停记录。
    pub fn trigger_emergency(&mut self) {
        let me = self.me();
        self.diagnostic_log(
            line!(),
            "emergency stop on this device: pausing sharing on every device",
            self.epoch,
        );
        tracing::warn!("emergency stop on this device: pausing sharing on every device");
        self.apply_emergency(me);
    }

    pub fn emergency(&self) -> Option<DeviceId> {
        self.emergency
    }

    /// 紧急暂停在本机生效：释放发往别处的键和按钮、释放为其他设备注入的键和按钮、暂停共享、回到本机
    /// 并显示光标。只改本机状态（「岛」：组内焦点 `claim` 不变），不依赖网络。
    fn apply_emergency(&mut self, origin: DeviceId) {
        self.cancel_source_contacts();
        self.cancel_local_gesture();
        self.retire_received_multifinger();
        self.cancel_caps();
        self.release_keyboard();
        self.end_focus_lock();
        self.fresh_join = false;
        let me = self.me();
        self.esc_down_at = None;
        // 源端：发往远端的抬起照常发送；本机补按的修饰键抬起；本机放行的键等物理抬起。之后的物理抬起吞掉。
        let keys: Vec<(KeyCode, Vec<Target>)> = self.keys.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (code, ts) in keys {
            let mut keep = Vec::new();
            for t in ts {
                match t {
                    Target::Local => keep.push(Target::Local),
                    Target::Remote(d) => self.send(d, Message::Input(InputMsg::Key { code, pressed: false })),
                    Target::LocalInjected => self.target_key(me, code, false),
                    Target::Dropped => {}
                }
            }
            if keep.is_empty() {
                keep.push(Target::Dropped);
            }
            self.keys.insert(code, keep);
        }
        let buttons: Vec<(MouseButton, Target)> = self.buttons.iter().map(|(b, t)| (*b, *t)).collect();
        for (button, t) in buttons {
            match t {
                Target::Local => continue,
                Target::Remote(d) => self.send(d, Message::Input(InputMsg::Button { button, pressed: false })),
                Target::LocalInjected | Target::Dropped => {}
            }
            self.buttons.insert(button, Target::Dropped);
        }
        self.scroll_target = None;
        // 转发表：补发抬起。
        let fwd: Vec<((DeviceId, HeldItem), DeviceId)> = self.forwarded.drain().collect();
        for ((o, item), dst) in fwd {
            self.press_routes.remove(&(o, item));
            self.send(dst, forwarded_release(self.admitted_boots[&o], item));
        }
        self.scroll_fwd.clear();
        // 目标端：为任何源注入的键和按钮全部抬起，停止重复。
        let origins: Vec<DeviceId> = self.held.keys().copied().collect();
        for o in origins {
            self.release_origin(o);
        }
        self.paused = true;
        self.emergency = Some(origin);
        // 回到本机、显示光标：焦点在别处（或状态已经分裂）就在本机收回；已经在本机也再切一次 Local，
        // 让 backend 在任何情况下都恢复并显示光标。
        if self.holder != me {
            let at = self
                .exit_point
                .clone()
                .or_else(|| self.pointer.clone())
                .or_else(|| self.display_center());
            let epoch = self.epoch;
            self.become_holder(epoch, at);
        } else {
            self.unconfirmed_since = None;
            self.rejoin = false;
            self.local_mouse_hidden = false;
            self.out.push(Output::SetMode(CaptureMode::Local));
        }
        self.out.push(Output::Status(StatusEvent::EmergencyStop { origin }));
    }

    /// 本机主屏（系统的主显示器；没有标记时取第一块）的中央。
    fn primary_display_center(&self) -> Option<ScreenPos> {
        let d = self
            .displays
            .iter()
            .find(|d| d.is_primary)
            .or_else(|| self.displays.first())?;
        Some(ScreenPos::new(
            ScreenId::new(self.me(), d.key.clone()),
            d.bounds.center(),
        ))
    }

    fn display_center(&self) -> Option<ScreenPos> {
        let d = self.displays.first()?;
        Some(ScreenPos::new(
            ScreenId::new(self.me(), d.key.clone()),
            d.bounds.center(),
        ))
    }

    /// 对端的手感档案。
    pub fn peer_feel(&self, peer: DeviceId) -> Option<&FeelProfile> {
        self.peers.get(&peer).map(|p| &p.feel)
    }

    /// 本机的手感档案。
    pub fn local_feel(&self) -> &FeelProfile {
        &self.feel
    }

    /// 本机显示器。
    pub fn local_displays(&self) -> &[DisplayInfo] {
        &self.displays
    }

    /// 对端通告的显示器列表。
    pub fn peer_displays(&self, peer: DeviceId) -> Option<&[DisplayInfo]> {
        self.peers.get(&peer).map(|p| p.displays.as_slice())
    }

    pub fn peer_name(&self, peer: DeviceId) -> Option<&str> {
        self.peers.get(&peer).map(|p| p.name.as_str())
    }

    /// Whether an authenticated group activity notification may be honoured.
    /// Display wake is a separate setting; network pacing also uses this gate.
    pub fn accepts_group_user_activity(&self, from: DeviceId) -> bool {
        !self.fully_paused && !self.paused && self.peers.get(&from).is_some_and(|peer| peer.connected && peer.sharing)
    }

    /// 取走待执行的输出（按产生顺序）。
    pub fn drain_outputs(&mut self) -> Vec<Output> {
        // 任何入口都可能改变焦点、按钮、游戏模式或设置：交出输出前统一同步拖尾状态。
        self.gap_sync();
        self.trail_sync();
        std::mem::take(&mut self.out)
    }

    /// 下一次需要调用 [`Router::tick`] 的时间。
    pub fn next_deadline(&self) -> Option<Micros> {
        if self.fully_paused {
            return self
                .keyboard
                .deadline()
                .into_iter()
                .chain(self.emergency_deadline())
                .min();
        }
        let timeout = self.timeout_us();
        let mut d = self.sync_deadline();
        if let Some(t) = self.keyboard.deadline() {
            d = Some(d.map_or(t, |x| x.min(t)));
        }
        let mut take = |t: Micros| d = Some(d.map_or(t, |x| x.min(t)));
        if let Some(t) = self.unconfirmed_since {
            take(t + timeout);
        }
        if let Some(t) = self.emergency_deadline() {
            take(t);
        }
        if let Some(t) = self.caps_press.as_ref().and_then(caps::CapsPress::deadline) {
            take(t);
        }
        if let Some(t) = self.trail_deadline() {
            take(t);
        }
        if let Some(t) = self.dnd_deadline() {
            take(t);
        }
        d
    }

    /// 截获回调的快路径：立即决定吞掉还是放行；其余动作进入输出队列。
    pub fn on_local_event(&mut self, ev: &CapturedEvent, now: Micros) -> Disposition {
        if ev.physical {
            self.update_remote_until = 0;
        }
        self.local_rewrite = None;
        self.dnd.now = now;
        self.enforce_screen_policy();
        let decision_start = self.out.len();
        let local_mouse_woke = self.reveal_local_mouse(ev);
        if self.observe_emergency_event(ev, now) {
            self.source_decision(&ev.event, Disposition::Swallow, decision_start);
            return Disposition::Swallow;
        }
        if let Some(disposition) = self.keyboard_event(ev, now) {
            self.source_decision(&ev.event, disposition, decision_start);
            return disposition;
        }
        if let Some(disposition) = self.contacts_route(ev) {
            self.source_decision(&ev.event, disposition, decision_start);
            return disposition;
        }
        if self.fully_paused || !self.capture_available {
            self.keyboard.record_pass(ev, Disposition::Pass);
            self.source_decision(&ev.event, Disposition::Pass, decision_start);
            return Disposition::Pass;
        }
        // Backends discard Oink-injected echoes before this entry point.
        let active = match ev.event {
            InputEvent::PointerDelta { dx, dy } | InputEvent::RawPointerDelta { dx, dy } => dx != 0. || dy != 0.,
            _ => true,
        };
        let source_local = self.source_is_local(&ev.event);
        if ev.physical && active && !source_local {
            self.last_pointer_source = Some(self.me());
        }
        // Own hardware must count as local activity even when the platform swallows
        // it for forwarding. This is independent of sharing and group wake settings.
        if ev.physical
            && active
            && self
                .last_local_user_activity
                .is_none_or(|t| now.saturating_sub(t) >= 1_000_000)
        {
            self.last_local_user_activity = Some(now);
            self.out.push(Output::LocalUserActivity);
            if !self.paused && self.cfg.settings.wake_displays_on_input {
                self.out.push(Output::ResetUserIdle);
            }
        }
        if ev.physical
            && !self.paused
            && active
            && self
                .last_user_activity
                .is_none_or(|t| now.saturating_sub(t) >= 1_000_000)
        {
            self.last_user_activity = Some(now);
            let peers: Vec<_> = self
                .peers
                .iter()
                .filter(|(_, p)| p.connected)
                .map(|(id, p)| (*id, p.name.clone()))
                .collect();
            for (to, device_name) in peers {
                tracing::debug!(?to, %device_name, "sending user activity");
                self.send(to, Message::Control(ControlMsg::UserActivity));
            }
        }
        // A real event still counts as user activity when gap travel drops it.
        // Reconciled modifiers and other synthetic input never enter that path.
        self.gap_sync();
        if !source_local && let Some(disposition) = self.gap_local_event(&ev.event) {
            self.source_decision(&ev.event, disposition, decision_start);
            return disposition;
        }
        if !source_local {
            self.fresh_join_local_input(ev);
        }
        if !source_local && self.holder == self.me() {
            let activity = match ev.event {
                InputEvent::PointerDelta { dx, dy } | InputEvent::RawPointerDelta { dx, dy } => dx != 0. || dy != 0.,
                _ => true,
            };
            if activity {
                self.last_pointer_source = Some(self.me());
            }
        }
        // 岛上用过本机键鼠：用户在操作本机，焦点机恢复后不再自动重新加入。
        if !source_local {
            self.rejoin = false;
        }
        // 用户在操作本机：迟到的 FocusEnter 不再把光标挪到进入点。
        if !source_local {
            self.enter_due = None;
        }
        let disposition = match ev.event {
            InputEvent::TouchpadSample(_) => Disposition::Pass,
            InputEvent::RawPointerDelta { dx, dy } => self.raw_motion(dx, dy),
            InputEvent::PointerDelta { dx, dy } => {
                if !self.mouse_local() {
                    self.trail_motion(dx, dy, now);
                }
                self.local_motion(dx, dy, if local_mouse_woke { None } else { ev.cursor }, now)
            }
            InputEvent::Button { button, pressed } => {
                let result = self.local_button(button, pressed);
                if pressed && self.buttons.contains_key(&button) {
                    self.button_diagnostics.press(
                        self.me(),
                        button,
                        now,
                        Some(ButtonSourcePress {
                            physical: ev.physical,
                            event_time_us: ev.time_us,
                        }),
                    );
                }
                if pressed
                    && button == MouseButton::Left
                    && self.holder == self.me()
                    && let Some(at) = ev.cursor
                    && let Some(display) = self.displays.iter().find(|d| d.bounds.contains(at))
                {
                    self.dnd.press_origin = Some(ScreenPos::new(ScreenId::new(self.me(), &display.key), at));
                }
                result
            }
            InputEvent::Key { code, pressed } => {
                if code.is_volume() && pressed {
                    self.capture_volume_origin(ev);
                }
                let mut disposition = self
                    .caps_event(code, pressed, ev.physical, now)
                    .unwrap_or_else(|| self.local_key(code, pressed, now));
                if disposition == Disposition::Pass && self.cfg.os == Os::MacOS {
                    let repeat = pressed && self.local_mapped.contains_key(&code);
                    let mapped = if pressed {
                        let mapped = self
                            .local_mapped
                            .get(&code)
                            .copied()
                            .unwrap_or_else(|| self.map_key(self.me(), code));
                        self.local_mapped.insert(code, mapped);
                        mapped
                    } else {
                        self.local_mapped.remove(&code).unwrap_or(code)
                    };
                    // Two physical keys can produce the same logical key. Keep
                    // the native press until its last contributor releases it,
                    // including an identity key that is absent from the table.
                    let another = self
                        .local_mapped
                        .iter()
                        .any(|(physical, logical)| *physical != code && *logical == mapped);
                    if another && (!pressed || !repeat) {
                        disposition = Disposition::Swallow;
                    } else if mapped != code {
                        self.local_rewrite = Some(oink_foundation::input::LocalRewrite::Key { code: mapped, pressed });
                    }
                }
                disposition
            }
            InputEvent::Scroll { dx, dy, unit, phase } => self.local_scroll(ev.source, dx, dy, unit, phase),
            InputEvent::Gesture(g) => self.local_gesture(ev.source, g),
        };
        let disposition = if local_mouse_woke && disposition == Disposition::Pass {
            self.replay_local_mouse_wake(ev)
        } else {
            disposition
        };
        self.keyboard.record_pass(ev, disposition);
        self.source_decision(&ev.event, disposition, decision_start);
        disposition
    }

    fn receive_message(&mut self, from: DeviceId, msg: Message, now: Micros) {
        self.dnd.now = now;
        if self.fully_paused || from == self.me() {
            return;
        }
        match msg {
            Message::Control(c) => self.on_control(from, c, now),
            Message::Input(i) | Message::TimedInput { input: i, .. } => self.on_input(from, from, 0, i, now),
            Message::Dnd(d) => self.on_dnd_msg(from, d),
        }
        self.enforce_screen_policy();
    }

    /// 定时器：心跳、超时检测、按键重复、滚动手势目标过期。
    pub fn tick(&mut self, now: Micros) {
        self.dnd.now = now;
        self.enforce_screen_policy();
        if self.keyboard.deadline().is_some_and(|due| now >= due) {
            self.release_keyboard();
        }
        self.dnd_expire();
        if self.emergency_deadline().is_some_and(|deadline| now >= deadline) {
            self.esc_down_at = None;
            self.trigger_emergency();
        }
        if self.fully_paused {
            return;
        }
        if self.sync_deadline().is_some_and(|t| now >= t) {
            self.sync_seq += self.sync_step() - 1;
            self.send_state_sync();
            self.next_sync = Some(now + self.interval_us());
        }

        let timeout = self.timeout_us();
        // 焦点机一直没有确认移交（它的通告落后于本机）：只在本机收回焦点。
        if self.unconfirmed_since.is_some_and(|t| now >= t + timeout) {
            self.unconfirmed_since = None;
            let me = self.me();
            if self.holder != me {
                self.diagnostic_log(
                    line!(),
                    "focus holder never confirmed the handoff: taking focus back locally",
                    self.epoch,
                );
                tracing::warn!(holder = ?self.holder, epoch = self.epoch, "focus holder never confirmed the handoff: taking focus back locally");
                self.island();
                self.rejoin = true;
            }
        }

        self.caps_tick(now);
        self.trail_tick(now);
    }

    fn emergency_hold_us(&self) -> Micros {
        u64::from(self.cfg.settings.emergency_hold_ms) * 1000
    }

    // ================= 本机事件（源端） =================

    fn local_motion(&mut self, dx: f64, dy: f64, cursor: Option<Point>, now: Micros) -> Disposition {
        if self.mouse_local() {
            if let Some(cursor) = cursor
                && let Some((key, bounds)) = self.locate(cursor)
            {
                let pos = ScreenPos::new(ScreenId::new(self.me(), key), bounds.clamp(cursor));
                self.local_cursor = Some(pos.clone());
                if self.has_pointer_focus() {
                    self.pointer = Some(pos);
                }
            }
            return Disposition::Pass;
        }
        if self.holder == self.me() && (dx != 0. || dy != 0.) {
            self.game.remote_capture.local_motion();
        }
        let me = self.me();
        if self.game_mode().active {
            return if self.holder == me {
                Disposition::Pass
            } else {
                Disposition::Swallow
            };
        }
        if self.holder != me {
            let s = self.reference_per_native_unit();
            let (mx, my) = (dx * s, dy * s);
            self.send_motion(mx, my);
            return Disposition::Swallow;
        }
        if let Some(from) = self.pointer.clone()
            && let Some(disposition) = self.gap_local_motion(&from, (dx, dy), cursor, now)
        {
            return disposition;
        }
        // 焦点在本机：光标由系统移动，路由只观察。注意在 macOS 上吞掉移动
        // 事件挡不住光标（系统在截获之前已经移动了光标），所以这里从不靠「吞掉」拦光标：
        // 移交时立即切到 `Remote`（解耦 + 停靠），需要把光标放到别处时注入 `MoveTo`。
        let Some(c) = cursor else {
            self.resistance = None;
            return Disposition::Pass;
        };
        let Some((key, bounds)) = self.locate(c) else {
            self.resistance = None;
            return Disposition::Pass;
        };
        let prev = self.pointer.clone();
        let cur = ScreenPos::new(ScreenId::new(me, key), bounds.clamp(c));
        self.pointer = Some(cur.clone());
        self.local_cursor = Some(cur.clone());
        // Gap geometry owns edge decisions even when this step stayed on the
        // screen. The legacy edge band below would otherwise hand off one pixel
        // early (or at the inclusive left/top edge), skipping the entire gap.
        // Keep OS cursor tracking above, including completed same-device moves;
        // held buttons and native drags still take the stitching path below.
        if self.gap_allowed() {
            self.retain_resistance(&cur, me, (dx, dy));
            self.dnd_off_edge();
            return Disposition::Pass;
        }
        self.retain_resistance(&cur, me, (dx, dy));

        // 1. 系统把光标移到了本机的另一块屏幕：按布局看这一步从原来的屏幕出发通往哪里。
        //    通往其他设备（布局里本机屏幕之间的排列与系统排列不同）→ 移交；否则尊重系统的跨屏。
        if let Some(prev) = prev.clone().filter(|p| p.screen != cur.screen && p.screen.device == me) {
            if let Some(dst) = self.layout.exit_through_edge(&prev, (dx, dy))
                && dst.screen.device != me
                && self.input_ready(dst.screen.device)
                && !self.leave_blocked()
            {
                // 系统已把光标移进本机另一块屏幕：光标不在外缘上，不经过阻力区。
                self.pointer = Some(prev);
                self.diagnostic_log(line!(), "local edge crossing: requesting handoff", self.epoch + 1);
                tracing::info!(origin = ?me, to = ?dst.screen.device, epoch = self.epoch + 1,
                    reason = "local pointer crossed display edge", "focus handoff requested");
                if let Some(d) = self.diagnostics.as_ref().filter(|d| d.enabled()) {
                    d.crossing_with_zone(
                        self.layout.clone(),
                        self.epoch + 1,
                        self.pointer.as_ref().unwrap(),
                        Point::new(dx, dy),
                        true,
                        false,
                        self.crossing_uses_zone(self.pointer.as_ref().unwrap(), (dx, dy), true),
                        &dst,
                        self.departure_evidence(self.pointer.as_ref().unwrap()),
                    );
                }
                self.hand_off(dst, self.trail_pointer());
                return Disposition::Swallow;
            }
            self.dnd_off_edge();
            return Disposition::Pass;
        }

        // 2. 光标停在系统排列的屏幕外缘（系统挡住了，`cursor` 是处理这个事件之后的位置）且继续向外移动。
        let c = cur.local;
        let at_edge = (dx > 0.0 && c.x >= bounds.right() - EDGE_UNITS)
            || (dx < 0.0 && c.x < bounds.x + EDGE_UNITS)
            || (dy > 0.0 && c.y >= bounds.bottom() - EDGE_UNITS)
            || (dy < 0.0 && c.y < bounds.y + EDGE_UNITS);
        if !at_edge {
            self.button_diagnostics.end_episode();
            if !self.dnd_follow_edge(me, &cur, (dx, dy)) {
                self.dnd_off_edge();
            }
            return Disposition::Pass;
        }
        // Native cursor coordinates already include this frame's on-screen
        // movement. Preserve the normal component as edge pressure, but do not
        // replay completed tangential travel after entering the target.
        // An OS/display placement can invalidate the cache; only subtract a
        // measured advance when it agrees with this delta after native clipping.
        let delta = prev
            .as_ref()
            .filter(|p| p.screen == cur.screen)
            .filter(|p| {
                let predicted = bounds.clamp(Point::new(p.local.x + dx, p.local.y + dy));
                (predicted.x - c.x).abs() <= EDGE_UNITS && (predicted.y - c.y).abs() <= EDGE_UNITS
            })
            .map_or((dx, dy), |p| {
                let pressure_x =
                    (dx > 0. && c.x >= bounds.right() - EDGE_UNITS) || (dx < 0. && c.x < bounds.x + EDGE_UNITS);
                let pressure_y =
                    (dy > 0. && c.y >= bounds.bottom() - EDGE_UNITS) || (dy < 0. && c.y < bounds.y + EDGE_UNITS);
                (
                    if pressure_x { dx } else { dx - (c.x - p.local.x) },
                    if pressure_y { dy } else { dy - (c.y - p.local.y) },
                )
            });
        let Some(dst) = self.layout.exit_through_edge(&cur, delta) else {
            // At a corner, a tangent to the probed edge is also outward at the
            // perpendicular edge. That wall must not retire the native offer:
            // unprobing revokes its deferred data, and recovery would require a
            // new native DragEnter rather than the ongoing DragOver callbacks.
            if self.dnd_follow_edge(me, &cur, (dx, dy)) {
                return Disposition::Pass;
            }
            self.diagnostic(DecisionKind::Refused, DecisionReason::NoTarget, Some(&cur));
            self.dnd_off_edge();
            return Disposition::Pass;
        };
        if dst.screen.device == me {
            self.dnd_off_edge();
            // 布局里这条边通往本机的另一块屏幕，而系统排列在这里是墙：把光标放过去（macOS 上是 warp）。
            if self.handoff_blocked() {
                return Disposition::Pass;
            }
            self.out.push(Output::Inject(InjectEvent::MoveTo { pos: dst.local }));
            self.pointer = Some(dst);
            return Disposition::Swallow;
        }
        if !self.input_ready(dst.screen.device) || self.leave_blocked() {
            self.diagnostic(DecisionKind::Refused, self.refusal_reason(), Some(&dst));
            self.resistance = None;
            if self.handoff_blocked() {
                // 拖动中推边：有系统拖拽就接住它交过去。
                let at = cur.local;
                self.dnd_push(me, &cur, dst, delta, at);
            } else {
                self.dnd_off_edge();
            }
            return Disposition::Pass;
        }
        if !self.push_resistance(me, &cur, prev.as_ref(), &dst, (dx, dy), now) {
            self.diagnostic(DecisionKind::Refused, DecisionReason::Resistance, Some(&dst));
            return Disposition::Pass;
        }
        self.diagnostic_log(line!(), "local edge crossing: requesting handoff", self.epoch + 1);
        tracing::info!(origin = ?me, to = ?dst.screen.device, epoch = self.epoch + 1,
            reason = "local pointer crossed edge", "focus handoff requested");
        if let Some(d) = self.diagnostics.as_ref().filter(|d| d.enabled()) {
            d.crossing_with_zone(
                self.layout.clone(),
                self.epoch + 1,
                &cur,
                Point::new(delta.0, delta.1),
                true,
                false,
                self.crossing_uses_zone(&cur, delta, true),
                &dst,
                self.departure_evidence(&cur),
            );
        }
        self.hand_off(dst, self.trail_pointer());
        Disposition::Swallow
    }

    fn local_button(&mut self, button: MouseButton, pressed: bool) -> Disposition {
        tracing::debug!(?button, pressed, held_target = ?self.buttons.get(&button), current_target = ?self.current_target(), "button trace: local routing");
        if pressed && self.cfg.settings.button_handoff == ButtonHandoff::Block {
            self.resistance = None;
        }
        if pressed {
            // 按钮没有自动重复：已按下的按钮再次按下，说明上一次的抬起丢了（截获中断期间）。
            if !self.mouse_local() {
                self.dnd_local_press();
            }
            // 先按记录的目标补发抬起，再当作新的按下。
            if let Some(old) = self.buttons.remove(&button) {
                tracing::warn!(?button, ?old, "button pressed again without release: releasing first");
                self.release_local_button(button, old);
            }
            let t = self.mouse_target();
            self.buttons.insert(button, t);
            if button == MouseButton::Left && t == Target::Local {
                self.dnd.press_origin = self.pointer.clone();
            }
            if let Target::Remote(d) = t {
                self.send(d, Message::Input(InputMsg::Button { button, pressed }));
                return Disposition::Swallow;
            }
            Disposition::Pass
        } else {
            self.button_diagnostics.release(self.me(), button);
            if button == MouseButton::Left {
                let me = self.me();
                if self.dnd_original_up(me) {
                    self.buttons.remove(&button);
                    return Disposition::Swallow;
                }
                self.dnd_left_released(me);
            }
            match self.buttons.remove(&button) {
                None => Disposition::Pass,
                Some(t) => disposition_for(self.release_local_button(button, t)),
            }
        }
    }

    /// 源端：把按钮的抬起发往按下时的目标。返回是否应放行给本机。
    fn release_local_button(&mut self, button: MouseButton, t: Target) -> bool {
        self.button_diagnostics.release(self.me(), button);
        tracing::debug!(?button, target = ?t, "button trace: releasing toward recorded target");
        match t {
            Target::Local => true,
            Target::Remote(d) => {
                self.send(d, Message::Input(InputMsg::Button { button, pressed: false }));
                false
            }
            Target::LocalInjected => {
                // 拖拽回到本机时由 Oink 注入的按下：注入抬起。
                let me = self.me();
                self.target_button(me, button, false);
                false
            }
            Target::Dropped => false,
        }
    }

    /// Diagnostic metadata from existing routing state; no key identity crosses the recorder boundary.
    pub fn diagnostic_key_state(&self, code: KeyCode, pressed: bool) -> (bool, u16) {
        let repeat = pressed && self.cfg.capture_reports_key_repeat && self.keys.contains_key(&code);
        let mut mask = 0;
        for modifier in self.keys.keys().filter(|key| key.is_modifier()) {
            mask |= 1u16 << (modifier.usage() - 0xe0);
        }
        (repeat, mask)
    }

    fn local_key(&mut self, code: KeyCode, pressed: bool, _now: Micros) -> Disposition {
        if pressed {
            if let Some(ts) = self.keys.get(&code) {
                if self.cfg.capture_reports_key_repeat {
                    // 系统的自动重复：重复只在源机产生，发往远端的照原样转发。
                    let ts = ts.clone();
                    self.forward_key_repeat(code, &ts);
                    return disposition_for(ts.contains(&Target::Local));
                }
                // 截获不上报自动重复：这是新的一次按下，上一次的抬起丢了。先补发抬起。
                tracing::warn!("key pressed again without release: releasing first");
                if let Some(ts) = self.keys.remove(&code) {
                    self.release_local_key(code, ts);
                }
            }
            if code == KeyCode::ESCAPE
                && (self.holder == self.me() || self.dnd_drop_released())
                && self.dnd_escape(self.cfg.device)
            {
                // Consume both edges: before release this cancels the drag;
                // after release it protects its pending native completion.
                self.keys.insert(code, vec![Target::Dropped]);
                return Disposition::Swallow;
            }
            if let Some(action) = self
                .hotkeys
                .matching(code, Mods::held(self.keys.keys().copied()))
                .filter(|action| *action != "pause_all")
            {
                // 全局快捷键：这个键不交给任何设备，它的重复与抬起也吞掉（`Dropped`）；已经交出去的
                // 修饰键照常在原目标抬起，不会卡键。
                let action = action.to_string();
                tracing::info!(%action, "hotkey pressed");
                self.keys.insert(code, vec![Target::Dropped]);
                self.key_repeats.remove(&code);
                self.mask_menu_keys();
                // Capture the focus/epoch at the physical key press, before later
                // input or an ordinary-thread hotkey dispatcher can change it.
                if action == "lock_focus" {
                    self.toggle_locked();
                }
                self.out.push(Output::Status(StatusEvent::Hotkey { action }));
                return Disposition::Swallow;
            }
            let mut t = if self.keyboard_local() {
                Target::Local
            } else {
                self.current_target()
            };
            if t == Target::Local
                && !self.keyboard_local()
                && code.is_volume()
                && let Some(exit) = self.active_volume_exit()
            {
                self.begin_volume(self.me(), code, exit);
                t = Target::LocalInjected;
            }
            // Windows hooks cannot modify native events. Queue every keyboard
            // event in the transformed local sequence, including identity keys:
            // letting an ordinary key pass could overtake its injected modifier.
            if t == Target::Local
                && self.cfg.os == Os::Windows
                && (!self.resolved_transform(self.me()).keyboard.table.is_empty() || self.local_caps_mapping())
            {
                self.target_key(self.me(), code, true);
                t = Target::LocalInjected;
            }
            self.keys.insert(code, vec![t]);
            self.key_repeats.remove(&code);
            if let Target::Remote(d) = t {
                if let Some(action) = crate::input::VolumeKey::from_code(code)
                    && let Some(origin) = self.local_volume_origin
                {
                    self.send(d, Message::Input(InputMsg::VolumeKeyOrigin { action, origin }));
                }
                self.send(d, Message::Input(InputMsg::Key { code, pressed }));
                return Disposition::Swallow;
            }
            if t == Target::Local {
                self.volume_decision(self.me(), code, None, false);
            }
            disposition_for(t == Target::Local)
        } else {
            // 没见过按下（路由启动前按下的）：交给本机。
            let Some(ts) = self.keys.remove(&code) else {
                return Disposition::Pass;
            };
            disposition_for(self.release_local_key(code, ts))
        }
    }

    /// 源端：把键的抬起发往按下时记录的所有目标。返回是否应放行给本机。
    fn release_local_key(&mut self, code: KeyCode, targets: Vec<Target>) -> bool {
        self.key_repeats.remove(&code);
        let mut pass = false;
        for t in targets {
            match t {
                Target::Local => pass = true,
                Target::LocalInjected => self.target_key(self.me(), code, false),
                Target::Remote(d) => self.send(d, Message::Input(InputMsg::Key { code, pressed: false })),
                Target::Dropped => {}
            }
        }
        pass
    }

    /// 源端：系统对按着的键产生了一次自动重复。发往远端的键发 `KeyRepeat`（带序号，目标端据此去重）；
    /// 修饰键、Caps Lock 不转发重复（目标系统自己也不会重复它们）。
    fn forward_key_repeat(&mut self, code: KeyCode, targets: &[Target]) {
        if !repeatable(code) {
            return;
        }
        if targets.contains(&Target::LocalInjected) {
            if self.send_volume(self.me(), code, true, true) {
                // Physical volume shortcut already owns this sequence.
            } else if let Some(mapped) = self.held.get(&self.me()).and_then(|h| h.keys.get(&code)).copied()
                && repeatable(mapped)
            {
                self.out.push(Output::Inject(InjectEvent::Key {
                    code: mapped,
                    pressed: true,
                }));
            }
        }
        let remote: Vec<DeviceId> = targets
            .iter()
            .filter_map(|t| match t {
                Target::Remote(d) => Some(*d),
                _ => None,
            })
            .collect();
        if remote.is_empty() {
            return;
        }
        let n = self.key_repeats.entry(code).or_insert(0);
        *n += 1;
        let n = *n;
        for d in remote {
            self.send(d, Message::Input(InputMsg::KeyRepeat { code, n }));
        }
    }

    fn local_scroll(
        &mut self,
        source: SourceKind,
        dx: f64,
        dy: f64,
        unit: ScrollUnit,
        phase: ScrollPhase,
    ) -> Disposition {
        // A focus change poisons the whole remaining finger/momentum sequence.
        // Unphased wheel steps are independent and do not reset a trackpad pan.
        let destination = if source == SourceKind::Touchpad
            && !self.cfg.settings.forward_gestures
            && matches!(self.mouse_target(), Target::Remote(_))
        {
            Target::Dropped
        } else {
            self.mouse_target()
        };
        let target = match phase {
            ScrollPhase::None => destination,
            ScrollPhase::Began => {
                let target = destination;
                if self.scroll_target.is_none_or(|r| r.target != target) {
                    self.scroll_target = Some(ScrollRoute::new(target));
                }
                target
            }
            _ => self.scroll_target.map_or(Target::Dropped, |r| r.target),
        };
        if self.scroll_target.as_mut().is_some_and(|r| r.phase(phase)) {
            self.scroll_target = None;
        }
        let d = match target {
            Target::Remote(d) => d,
            Target::Local => {
                if source == SourceKind::Mouse
                    && phase == ScrollPhase::None
                    && self.resolved_transform(self.me()).reverse_wheel
                {
                    if self.cfg.os == Os::MacOS {
                        self.local_rewrite = Some(oink_foundation::input::LocalRewrite::ReverseWheel);
                    } else if self.cfg.os == Os::Windows {
                        self.out.push(Output::Inject(InjectEvent::Scroll {
                            dx: -dx,
                            dy: -dy,
                            unit,
                            phase,
                        }));
                        return Disposition::Swallow;
                    }
                }
                return Disposition::Pass;
            }
            Target::LocalInjected | Target::Dropped => return Disposition::Swallow,
        };
        let (k, amount) = match unit {
            ScrollUnit::Pixel => (self.reference_per_native_unit(), ScrollAmount::Reference),
            ScrollUnit::Line => (self.feel.wheel_lines_per_notch.unwrap_or(1.0), ScrollAmount::Lines),
        };
        self.send(
            d,
            Message::Input(InputMsg::Scroll {
                source,
                dx: dx * k,
                dy: dy * k,
                unit: amount,
                phase,
            }),
        );
        Disposition::Swallow
    }

    /// 本机触摸板手势：一次手势从开始到结束发往开始时的目标（独立于滚动的焦点离开丢弃规则）。
    fn local_gesture(&mut self, source: SourceKind, gesture: Gesture) -> Disposition {
        let target = match self.gesture_lock {
            Some(t) if !gesture.starts() => t,
            _ if !self.cfg.settings.forward_gestures && matches!(self.mouse_target(), Target::Remote(_)) => {
                Target::Dropped
            }
            _ => self.mouse_target(),
        };
        self.gesture_lock = (!gesture.ends()).then_some(target);
        self.source_gesture = (!gesture.ends()).then_some(gesture);
        match target {
            Target::Local => Disposition::Pass,
            Target::Remote(d) => {
                if self.cfg.settings.forward_gestures {
                    self.send(d, Message::Input(InputMsg::Gesture { source, gesture }));
                }
                // 关闭转发时照样吞掉：焦点在其他设备上，本机不响应手势（开关之前的行为）。
                Disposition::Swallow
            }
            Target::LocalInjected | Target::Dropped => Disposition::Swallow,
        }
    }

    // ================= 对端消息 =================

    fn on_control(&mut self, from: DeviceId, msg: ControlMsg, now: Micros) {
        match msg {
            ControlMsg::UserActivity => {
                if let Some(peer) = self.peers.get(&from) {
                    let respond = self.accepts_group_user_activity(from) && self.cfg.settings.wake_displays_on_input;
                    tracing::debug!(?from, device_name = %peer.name, respond, "received user activity");
                    if respond {
                        self.out.push(Output::UserActivity {
                            from,
                            device_name: peer.name.clone(),
                        });
                        self.out.push(Output::ResetUserIdle);
                    }
                }
            }
            ControlMsg::CursorTrail { active } => self.trail_msg(from, active),
            m @ ControlMsg::CursorTrailHandoff { .. } => self.trail_receive(from, m, now),
            // Consumed by the input module's adapter (it becomes the peer's
            // `sharing` flag when the pair attaches); the router itself does not act on it.
            ControlMsg::InputCapability { .. } => {}
            ControlMsg::Feel { profile } => {
                if let Some(p) = self.peers.get_mut(&from) {
                    p.feel = profile;
                }
            }
            ControlMsg::IdentifyScreens { .. } => {}
            ControlMsg::FocusEnter {
                epoch,
                display,
                pos,
                scope,
                pointer_source,
            } => {
                self.diagnostic_log(line!(), "cursor latency: FocusEnter received", epoch);
                tracing::debug!(target: "oink_cursor_timing", diagnostic = true, epoch, ?from, ?pos, "cursor latency: FocusEnter received");
                let me = self.me();
                if self.layout.source_requires_target(me) {
                    self.send_focus_state(from);
                    self.dnd_focus_enter_ignored(from, epoch, "target excluded by layout policy");
                    return;
                }
                if self.locked() {
                    self.send_focus_state(from);
                    self.dnd_focus_enter_ignored(from, epoch, "focus locked");
                    return;
                }
                if !self.scope_allows(scope.as_ref(), me) {
                    self.diagnostic_log(line!(), "ignoring FocusEnter from a previous routing instance", epoch);
                    self.diagnostic(DecisionKind::Refused, DecisionReason::PreviousInstance, None);
                    self.on_focus_state(from, epoch, me, scope, now);
                    self.dnd_focus_enter_ignored(from, epoch, "previous routing session");
                    return;
                }
                // 本机已从别的设备的通告得知这次移交、接过了焦点，交出方的 FocusEnter 现在才到：照常进入。
                let late = self.enter_due == Some(epoch)
                    && (epoch, me) == (self.epoch, self.claim)
                    && self.holder == me
                    && !self.paused;
                if late {
                    self.diagnostic_log(
                        line!(),
                        "FocusEnter arrived after another device announced this handoff: entering now",
                        epoch,
                    );
                    tracing::info!(
                        ?from,
                        epoch,
                        "FocusEnter arrived after another device announced this handoff: entering now"
                    );
                } else if !claim_newer((epoch, me), (self.epoch, self.claim)) {
                    if (epoch, me) != (self.epoch, self.claim) {
                        // 发送方以为本机接过了焦点，而本机已知更新的焦点（epoch 分叉）：告诉它，它随之改正。
                        tracing::info!(
                            epoch,
                            known = self.epoch,
                            ?from,
                            "ignoring stale FocusEnter; sending our focus back"
                        );
                        self.send_focus_state(from);
                    }
                    self.diagnostic_log(line!(), "ignoring stale or already known FocusEnter", epoch);
                    self.diagnostic(DecisionKind::Refused, DecisionReason::Stale, None);
                    self.dnd_focus_enter_ignored(from, epoch, "stale or already known");
                    return;
                }
                if self.paused {
                    self.diagnostic_log(line!(), "sharing paused: ignoring FocusEnter", epoch);
                    tracing::info!(?from, "sharing paused: ignoring FocusEnter");
                    self.diagnostic(DecisionKind::Refused, DecisionReason::Paused, None);
                    self.dnd_focus_enter_ignored(from, epoch, "sharing paused");
                    // 发送方随之把本机当作离线、在它那里收回焦点。
                    self.send_focus_state(from);
                    return;
                }
                self.diagnostic_state("entry_before_clamp");
                let pos = match self.displays.iter().find(|d| d.key == display) {
                    Some(d) => d.bounds.clamp(pos),
                    None => pos,
                };
                let at = ScreenPos::new(ScreenId::new(self.me(), display), pos);
                self.diagnostic(DecisionKind::FocusAccepted, DecisionReason::Accepted, Some(&at));
                if self.epoch == 0 && from != me && self.layout.contains_device(from) {
                    self.last_enabled_focus = Some(from);
                }
                self.claim = me;
                self.focus_scope = scope;
                self.focus_joined = true;
                self.fresh_join = false;
                // Motion may have arrived after gossip but before this reliable enter.
                let source = if late {
                    self.last_pointer_source.or(pointer_source)
                } else {
                    pointer_source
                };
                self.become_holder(epoch, Some(at.clone()));
                self.diagnostic_state("entry_applied");
                self.last_pointer_source = source;
                self.trail_focus_entered(from, epoch, &at, now);
                // 带着拖拽进来的：光标放好之后发起本地拖拽。
                self.dnd_focus_entered(from, epoch, at.local);
            }
            ControlMsg::FocusState {
                lock_state,
                epoch,
                holder,
                scope,
                session,
            } => {
                if let Some(p) = self.peers.get_mut(&from) {
                    if let Some(session) = session {
                        if p.retired_sessions.contains(&session) {
                            return; // Delayed gossip from an incarnation already replaced.
                        }
                        if let Some(previous) = p.session.filter(|previous| *previous != session) {
                            p.retired_sessions.insert(previous);
                        }
                    }
                    p.session = session;
                    p.focus_ready = true;
                }
                if !self.scope_allows(self.focus_scope.as_ref(), self.claim) {
                    if self.holder != self.me() {
                        self.diagnostic_log(
                            line!(),
                            "focus belongs to a previous routing session: staying local",
                            self.epoch,
                        );
                        tracing::info!(
                            ?from,
                            epoch = self.epoch,
                            "focus belongs to a previous routing session: staying local"
                        );
                    }
                    self.island();
                    self.rejoin = false;
                }
                if let Some(state) = lock_state {
                    self.receive_focus_lock(state);
                }
                self.on_focus_state(from, epoch, holder, scope, now);
                self.validate_focus_lock_session();
            }
            ControlMsg::FocusResume { epoch, holder, scope } => {
                self.receive_focus_resume(from, epoch, holder, scope);
            }
            ControlMsg::GameMode {
                epoch,
                generation,
                mode,
                detected,
                active,
                cursor_hiding_cause_available,
            } => {
                self.receive_game_state(
                    from,
                    epoch,
                    generation,
                    mode,
                    detected,
                    active,
                    cursor_hiding_cause_available,
                );
            }
            ControlMsg::FocusLock { state } => self.receive_focus_lock(state),
            ControlMsg::SetFocusLock { epoch, locked, target } => {
                if self.holder == self.me()
                    && self.group_focus_status().participating
                    && (epoch == self.epoch
                        || self
                            .focus_lock
                            .as_ref()
                            .is_some_and(|s| s.locked && s.holder == self.me() && s.epoch == epoch))
                {
                    if let Some(target) = target {
                        if locked == Some(true) {
                            let _ = self.lock_focus_to(target);
                        }
                    } else {
                        self.change_focus_lock(locked);
                    }
                }
            }
            ControlMsg::SetGameMode { epoch, mode } => {
                if self.holder == self.me() && self.epoch == epoch && !self.paused {
                    self.set_game_mode(mode);
                }
            }
        }
    }

    /// 对端通告的焦点（移交时、连接建立 / 恢复时、每 500 ms 一次）。
    fn on_focus_state(&mut self, from: DeviceId, epoch: u64, holder: DeviceId, scope: Option<FocusScope>, now: Micros) {
        if self.layout.source_requires_target(holder) {
            return;
        }
        let theirs = (epoch, holder);
        let mine = (self.epoch, self.claim);
        if claim_newer(theirs, mine) {
            if let Some(pinned) = self.locked_device() {
                if pinned == self.me() && self.holder == self.me() {
                    self.renew_focus_lock(epoch);
                    return;
                }
                if holder != pinned || from != pinned {
                    // Let the holder serialize the conflict instead of echoing
                    // stale focus claims between two non-authoritative sources.
                    self.send(
                        pinned,
                        Message::Control(ControlMsg::FocusState {
                            epoch,
                            holder,
                            scope,
                            session: Some(self.session),
                            lock_state: self.focus_lock.clone(),
                        }),
                    );
                    return;
                }
            }
            self.focus_scope = scope;
            self.adopt_claim(epoch, holder);
            return;
        }
        if theirs == mine {
            // A later proof for the same claim may admit this incarnation.
            if !self.focus_joined && scope.is_some() && self.scope_allows(scope.as_ref(), holder) {
                self.focus_scope = scope;
                self.adopt_claim(epoch, holder);
                return;
            }
            let me = self.me();
            if self.holder == from {
                // 焦点机确认了。
                self.unconfirmed_since = None;
            } else if self.rejoin
                && self.holder == me
                && self.claim == from
                && !self.paused
                && self.input_ready(from)
                && self.scope_allows(self.focus_scope.as_ref(), from)
            {
                // 岛：焦点机恢复了、仍持有焦点，本机期间没用过键鼠：重新作为源。
                self.diagnostic_log(line!(), "focus holder is back and still holds focus: rejoining", epoch);
                tracing::info!(?from, epoch, "focus holder is back and still holds focus: rejoining");
                self.become_source(epoch, from);
            }
            return;
        }
        if !claim_newer(mine, theirs) {
            // 无从比较（两边都还没有发生过移交，各自是岛）：各自保持。
            return;
        }
        // 对方落后（没收到某次移交），或同一 epoch 的另一种说法输给了本机的：把本机的告诉它。
        self.send_focus_state(from);
        if self.holder == from && from != self.me() {
            // 本机以为焦点在它那里，它却不这么认为：它收到上面的通告后会接过焦点；
            // 超过心跳超时仍未确认就在本机收回（`tick`）。
            self.unconfirmed_since.get_or_insert(now);
        }
    }

    /// 采纳更新的组内焦点 `(epoch, holder)`。
    fn adopt_claim(&mut self, epoch: u64, holder: DeviceId) {
        if self.focus_joined && self.epoch > 0 && self.claim != holder && self.layout.contains_device(self.claim) {
            self.last_enabled_focus = Some(self.claim);
        }
        self.diagnostic_log(line!(), "adopting focus claim", epoch);
        let me = self.me();
        self.claim = holder;
        let admitted = self.scope_allows(self.focus_scope.as_ref(), holder);
        if admitted {
            self.focus_joined = true;
            self.fresh_join = false;
        } else {
            self.diagnostic_log(
                line!(),
                "historical focus learned without joining: routing session differs",
                epoch,
            );
            tracing::info!(
                epoch,
                ?holder,
                "historical focus learned without joining: routing session differs"
            );
        }
        if !admitted || holder == me || self.paused || !self.input_ready(holder) {
            // 焦点在本机；或本机暂停中、焦点机不可达（岛）：焦点留在本机，只记下组内的焦点。
            if self.holder == me {
                self.last_pointer_source = None;
                self.epoch = epoch;
                self.unconfirmed_since = None;
                self.push_focus_status();
            } else {
                let at = self.exit_point.clone().or_else(|| self.pointer.clone());
                self.become_holder(epoch, at);
            }
            // 焦点机只是暂时不可达：它恢复后可以重新加入。
            self.rejoin = admitted && holder != me && !self.paused;
            // 通告说本机是这次移交的焦点机：交出方的 FocusEnter 可能还在路上（它带着进入点、拖拽）。
            self.enter_due = (admitted && holder == me && !self.paused).then_some(epoch);
        } else {
            self.become_source(epoch, holder);
        }
    }

    fn on_input(&mut self, from: DeviceId, origin: DeviceId, hops: u8, msg: InputMsg, now: Micros) {
        if !self.admitted_boots.contains_key(&origin) {
            return;
        }
        // A receiver can hold local focus while another device supplies input.
        // Keep recent remote control busy through the full safe-idle interval.
        if origin != self.me() {
            self.update_remote_until = now.saturating_add(60_000_000);
        }
        self.gap_sync();
        if self.gap_remote_event(from, origin, &msg) {
            return;
        }
        let me = self.me();
        if let InputMsg::Volume {
            route,
            press_id,
            code,
            pressed,
            repeat,
            modifiers,
        } = msg
        {
            if code.is_volume()
                && press_id > 0
                && hops == 0
                && route.target == self.cfg.boot
                && self.admitted_boots.get(&origin) == Some(&route.source)
                && (!self.paused || !pressed)
            {
                self.out.push(Output::ExitVolumeKey {
                    source: route.source,
                    route,
                    press_id,
                    code,
                    pressed,
                    repeat,
                    modifiers,
                });
            }
            return;
        }
        // macOS sends Ended/Cancelled after focus retirement has already cleared
        // scroll_fwd (observed with macOS 26.6.2). Consume the retired filter's
        // tail before the focus/scope and scroll-route gates discard it. It must
        // never inject or forward, but leaving it live blocks future contacts too.
        if let InputMsg::Scroll {
            source: SourceKind::Touchpad,
            phase,
            ..
        } = &msg
            && self.multifinger.get(&origin).is_some_and(|state| state.retired)
        {
            self.filter_multifinger(origin, contacts::MultiEvent::Scroll(*phase));
            return;
        }
        if self.paused || self.epoch == 0 || !self.scope_allows(self.focus_scope.as_ref(), self.claim) {
            // 尚未接受移交、共享已暂停或旧实例焦点：远端的移动、按下、滚动不生效；抬起仍处理。
            let drop = match &msg {
                InputMsg::Motion { .. }
                | InputMsg::RelativeMotion { .. }
                | InputMsg::Scroll { .. }
                | InputMsg::KeyRepeat { .. } => true,
                // 手势的结束照常处理（Dock 不会停在半途）。
                InputMsg::Gesture { gesture, .. } => !gesture.ends(),
                InputMsg::TouchpadContacts(frame) => !frame.phase.ends(),
                InputMsg::Key { pressed, .. }
                | InputMsg::CapsKey { pressed, .. }
                | InputMsg::Button { pressed, .. } => *pressed,
                _ => false,
            };
            if drop {
                return;
            }
        }
        match msg {
            InputMsg::VolumeKeyOrigin {
                action,
                origin: context,
            } => {
                if self.holder != me {
                    self.forward(origin, hops, msg);
                } else {
                    self.remote_volume_origins.insert(origin, (action, context));
                }
            }
            InputMsg::TouchpadContacts(frame) => self.receive_contacts(origin, frame),
            InputMsg::Forwarded { origin, hops, msg } => {
                if hops == 0
                    || hops > self.cfg.settings.max_forward_hops
                    || self.admitted_boots.get(&origin.device_id) != Some(&origin)
                    || matches!(*msg, InputMsg::Forwarded { .. })
                {
                    return;
                }
                self.on_input(from, origin.device_id, hops, *msg, now)
            }
            InputMsg::RelativeMotion {
                epoch,
                generation,
                seq,
                x,
                y,
            } => {
                self.receive_relative(origin, epoch, generation, seq, x, y);
            }
            InputMsg::Motion { seq, dx, dy } => {
                tracing::debug!(target: "oink_cursor_timing",
                    seq,
                    ?origin,
                    hops,
                    epoch = self.epoch,
                    dx,
                    dy,
                    "cursor latency: Motion received"
                );
                // These are our own deltas sent while a peer still held focus, reflected after
                // handoff. The local OS now owns our pointer. Replaying an absolute position
                // derived from them (possibly queued behind Dnd::Begin) rewinds newer local motion.
                if origin == me && self.holder == me {
                    tracing::debug!(
                        dx,
                        dy,
                        "discarding reflected motion after returning to its input source"
                    );
                    return;
                }
                if self.game_mode().active {
                    return;
                }
                if self.holder == me && self.claim != me {
                    // 岛：发来位移的源以为本机是焦点机，而本机只是在本机收回了焦点。不移动、不据此
                    // 移交——否则迟到的位移（链路停顿后一起到达）会让岛在同一个 epoch 上再移交一次，焦点分叉。
                    tracing::debug!(?origin, "island: ignoring remote motion");
                } else if self.holder == me {
                    self.remote_motion(origin, dx, dy, now);
                } else {
                    self.forward(origin, hops, msg);
                }
            }
            InputMsg::CapsKey { code, pressed } => {
                // The source already chose the key and target. Never transform or
                // forward it again when focus has moved since physical Caps DOWN.
                self.target_mapped_key(origin, KeyCode::CAPS_LOCK, code, pressed);
            }
            InputMsg::Key { code, pressed } => {
                self.remote_press(from, origin, hops, HeldItem::Key(code), pressed, msg);
            }
            InputMsg::Button { button, pressed } => {
                tracing::debug!(?origin, ?button, pressed, hops, holder = ?self.holder, "button trace: received network button");
                self.remote_press(from, origin, hops, HeldItem::Button(button), pressed, msg);
            }
            InputMsg::Scroll {
                source,
                dx,
                dy,
                unit,
                phase,
            } => {
                // Retain a forwarding route only while its actual holder has
                // not changed. Focus changes clear it, so late finger/momentum
                // phases cannot be injected at the old holder or start at a new one.
                let target = match phase {
                    ScrollPhase::None => self.holder,
                    ScrollPhase::Began => {
                        self.scroll_fwd
                            .entry(origin)
                            .or_insert_with(|| ScrollRoute::new(self.holder));
                        self.holder
                    }
                    _ => match self.scroll_fwd.get(&origin).copied() {
                        Some(route) if route.target == self.holder => route.target,
                        _ => return,
                    },
                };
                if self.scroll_fwd.get_mut(&origin).is_some_and(|r| r.phase(phase)) {
                    self.scroll_fwd.remove(&origin);
                }
                if target != me {
                    self.forward(origin, hops, msg);
                    return;
                }
                if source == SourceKind::Touchpad
                    && !self.filter_multifinger(origin, contacts::MultiEvent::Scroll(phase))
                {
                    return;
                }
                self.inject_scroll(origin, source, dx, dy, unit, phase);
            }
            InputMsg::HeldSync { keys, buttons } => self.held_sync(from, origin, &keys, &buttons),
            InputMsg::KeyRepeat { code, n } => self.remote_repeat(origin, hops, code, n, msg),
            InputMsg::Gesture { gesture, .. } => self.remote_gesture(origin, hops, gesture, msg),
            InputMsg::Volume { .. } => {}
        }
    }

    fn is_held(&self, origin: DeviceId, item: HeldItem) -> bool {
        self.held.get(&origin).is_some_and(|h| match item {
            HeldItem::Key(k) => h.keys.contains_key(&k),
            HeldItem::Button(b) => h.buttons.contains(&b),
        })
    }

    /// Remove one path's claim; another path (e.g. a modifier snapshot) can still hold it.
    fn forget_press_route(&mut self, from: DeviceId, origin: DeviceId, item: HeldItem) -> bool {
        let Some(routes) = self.press_routes.get_mut(&(origin, item)) else {
            return from == origin; // Locally established state, e.g. drag handoff.
        };
        routes.remove(&from) && routes.is_empty()
    }

    fn release_press(&mut self, origin: DeviceId, item: HeldItem) {
        self.press_routes.remove(&(origin, item));
        if let Some(dst) = self.forwarded.remove(&(origin, item)) {
            self.send(dst, forwarded_release(self.admitted_boots[&origin], item));
        } else {
            match item {
                HeldItem::Key(code) => self.target_key(origin, code, false),
                HeldItem::Button(button) => self.target_button(origin, button, false),
            }
        }
    }

    /// 键或按钮的按下/抬起到达目标端。本机已不是焦点机时，按下转发给焦点机，抬起走同一路径。
    fn remote_press(
        &mut self,
        from: DeviceId,
        origin: DeviceId,
        hops: u8,
        item: HeldItem,
        pressed: bool,
        msg: InputMsg,
    ) {
        // A relay may synthesize this release after losing its upstream. It only
        // relinquishes that path, just as an empty HeldSync does; a direct modifier
        // snapshot can still hold the same key through a healthy source connection.
        if !pressed && self.press_routes.contains_key(&(origin, item)) && !self.forget_press_route(from, origin, item) {
            return;
        }
        // Preserve native drag ownership while applying source button transitions.
        match (item, pressed) {
            (HeldItem::Button(MouseButton::Left), false) => {
                if self.dnd_original_up(origin) {
                    return;
                }
                self.dnd_left_released(origin);
            }
            (HeldItem::Key(KeyCode::ESCAPE), true)
                if (self.holder == self.me() || self.dnd_drop_released()) && self.dnd_escape(origin) =>
            {
                return;
            }
            _ => {}
        }
        if let Some(&dst) = self.forwarded.get(&(origin, item)) {
            if !pressed {
                self.forwarded.remove(&(origin, item));
                self.press_routes.remove(&(origin, item));
            } else {
                self.press_routes.entry((origin, item)).or_default().insert(from);
            }
            self.send(
                dst,
                Message::Input(InputMsg::Forwarded {
                    origin: self.admitted_boots[&origin],
                    hops: hops.saturating_add(1),
                    msg: Box::new(msg),
                }),
            );
            return;
        }
        let already = self.is_held(origin, item);
        if pressed && !already && self.holder != self.me() {
            let dst = self.holder;
            if self.forward(origin, hops, msg) {
                self.forwarded.insert((origin, item), dst);
                self.forwarded_sync.insert((origin, dst));
                self.press_routes.entry((origin, item)).or_default().insert(from);
                return;
            }
        }
        match item {
            HeldItem::Key(code) => self.target_key(origin, code, pressed),
            HeldItem::Button(b) => self.target_button(origin, b, pressed),
        }
        if pressed && self.is_held(origin, item) {
            // A stale-view press A -> B -> C belongs to B's ordered stream at C.
            // A's direct HeldSync describes only A -> C and may legitimately omit it.
            // Modifiers can also arrive directly after FocusState, so retain both paths.
            self.press_routes.entry((origin, item)).or_default().insert(from);
        }
    }

    /// 源机的按键重复到达目标端：按下时转发出去的键跟着转发；本机为该源按着这个键时注入一次重复。
    /// 从不据此按下（键已被释放，例如心跳超时之后），序号不比已注入的大时忽略（重复投递）。
    fn remote_repeat(&mut self, origin: DeviceId, hops: u8, code: KeyCode, n: u32, msg: InputMsg) {
        if let Some(&dst) = self.forwarded.get(&(origin, HeldItem::Key(code))) {
            self.send(
                dst,
                Message::Input(InputMsg::Forwarded {
                    origin: self.admitted_boots[&origin],
                    hops: hops.saturating_add(1),
                    msg: Box::new(msg),
                }),
            );
            return;
        }
        if origin == self.me() || !repeatable(code) {
            return;
        }
        let Some(h) = self.held.get_mut(&origin) else {
            return;
        };
        let Some(&mapped) = h.keys.get(&code) else {
            tracing::debug!(?origin, "KeyRepeat for a key that is not held: ignored");
            return;
        };
        let last = h.repeats.entry(code).or_insert(0);
        if n <= *last {
            tracing::debug!(?origin, n, last = *last, "duplicate KeyRepeat: ignored");
            return;
        }
        *last = n;
        if !repeatable(mapped) {
            return;
        }
        if self.send_volume(origin, code, true, true) {
            return;
        }
        self.out.push(Output::Inject(InjectEvent::Key {
            code: mapped,
            pressed: true,
        }));
    }

    /// 源机的触摸板 Gesture 到达目标端：开始时转发出去的，后续阶段仍跟着转发；
    /// 源机锁定目标后发来的后续阶段，即使本机已不是焦点机也在本机注入。
    fn remote_gesture(&mut self, origin: DeviceId, hops: u8, gesture: Gesture, msg: InputMsg) {
        let continuing = !gesture.starts();
        if continuing && let Some(&dst) = self.gesture_fwd.get(&origin) {
            if gesture.ends() {
                self.gesture_fwd.remove(&origin);
            }
            self.send(
                dst,
                Message::Input(InputMsg::Forwarded {
                    origin: self.admitted_boots[&origin],
                    hops: hops.saturating_add(1),
                    msg: Box::new(msg),
                }),
            );
            return;
        }
        if !continuing {
            self.gesture_fwd.remove(&origin);
            let dst = self.holder;
            if dst != self.me() && self.forward(origin, hops, msg) {
                if !gesture.ends() {
                    self.gesture_fwd.insert(origin, dst);
                }
                return;
            }
        }
        if !self.filter_multifinger(origin, contacts::MultiEvent::Gesture(gesture)) {
            return;
        }
        if gesture.ends() {
            self.gesture_active.remove(&origin);
        } else {
            self.gesture_active.insert(origin, gesture);
        }
        self.out.push(Output::Inject(InjectEvent::Gesture(gesture)));
    }

    /// 转发给本机所知的焦点机；返回是否已转发。
    fn forward(&mut self, origin: DeviceId, hops: u8, msg: InputMsg) -> bool {
        let dst = self.holder;
        if dst == self.me() || dst == origin && hops > 0 || hops >= self.cfg.settings.max_forward_hops {
            return false;
        }
        if !self.reachable(dst) {
            return false;
        }
        self.send(
            dst,
            Message::Input(InputMsg::Forwarded {
                origin: self.admitted_boots[&origin],
                hops: hops + 1,
                msg: Box::new(msg),
            }),
        );
        true
    }

    fn remote_motion(&mut self, origin: DeviceId, dx: f64, dy: f64, now: Micros) {
        if dx != 0. || dy != 0. {
            self.last_pointer_source = Some(origin);
            if self.cfg.os == Os::MacOS && self.game.mode == GameMode::Auto && dx.is_finite() && dy.is_finite() {
                self.game.remote_capture.motion(now);
            }
        }
        let me = self.me();
        let from = match self.pointer.clone() {
            Some(p) => p,
            None => match self.displays.first() {
                Some(d) => ScreenPos::new(ScreenId::new(me, d.key.clone()), d.bounds.center()),
                None => return,
            },
        };
        let scale = self
            .layout
            .reference_per_native_unit(&from.screen)
            .unwrap_or(self.reference_per_native_unit());
        let gain = self.layout.remote_pointer_gain(&from.screen, origin);
        let delta = (dx * gain / scale, dy * gain / scale);
        if let Some(disposition) = self.gap_motion(&from, (dx, dy), origin, now) {
            if disposition == Disposition::Pass {
                let at = self.pointer.as_ref().unwrap();
                self.out.push(Output::Inject(self.pushed_move(at, dx, dy, origin)));
            }
            return;
        }
        let r = self.layout.step_from_source(&from, (dx, dy), origin);
        if r.to.screen.device == me {
            self.button_diagnostics.end_episode();
            self.retain_resistance(&r.to, origin, delta);
            self.pointer = Some(r.to.clone());
            let ev = if r.blocked_x || r.blocked_y || self.cfg.os == Os::Windows {
                // Walls need the original delta (bug 8); Windows also uses it as
                // capture motion evidence, even when an application clips the cursor.
                self.pushed_move(&r.to, dx, dy, origin)
            } else {
                InjectEvent::MoveTo { pos: r.to.local }
            };
            self.out.push(Output::Inject(ev));
            if !self.dnd_follow_edge(origin, &r.to, delta) {
                self.dnd_off_edge();
            }
            return;
        }
        if self.leave_blocked() || !self.input_ready(r.to.screen.device) {
            self.diagnostic(DecisionKind::Refused, self.refusal_reason(), Some(&r.to));
            self.resistance = None;
            // 当作墙：留在当前屏幕内。
            if let Some(p) = self.clamp_move(&from, dx, dy, origin) {
                self.out.push(Output::Inject(self.pushed_move(&p, dx, dy, origin)));
                self.pointer = Some(p.clone());
                if self.handoff_blocked() {
                    // 拖动中推边。
                    if let Some(scale) = self.layout.reference_per_native_unit(&from.screen) {
                        self.dnd_push(origin, &from, r.to, (dx * gain / scale, dy * gain / scale), p.local);
                    }
                } else {
                    self.dnd_off_edge();
                }
            }
            return;
        }
        if let Some(p) = self.clamp_move(&from, dx, dy, origin) {
            self.retain_resistance(&p, origin, delta);
            if !self.push_resistance(origin, &p, Some(&from), &r.to, delta, now) {
                self.diagnostic(DecisionKind::Refused, DecisionReason::Resistance, Some(&r.to));
                self.out.push(Output::Inject(self.pushed_move(&p, dx, dy, origin)));
                self.pointer = Some(p);
                return;
            }
            self.pointer = Some(p);
        }
        self.diagnostic_log(line!(), "remote edge crossing: requesting handoff", self.epoch + 1);
        tracing::info!(?origin, to = ?r.to.screen.device, epoch = self.epoch + 1,
            reason = "pointer crossed edge", "focus handoff requested");
        if let Some(d) = self.diagnostics.as_ref().filter(|d| d.enabled()) {
            d.crossing_with_zone(
                self.layout.clone(),
                self.epoch + 1,
                &from,
                Point::new(dx, dy),
                false,
                false,
                self.crossing_uses_zone(&from, (dx, dy), false),
                &r.to,
                self.departure_evidence(&from),
            );
        }
        // The native departure position is clamped for freezing. The trail
        // must retain the full crossing step, including its off-screen part;
        // clipping only x/y here would turn a shallow diagonal into a kink.
        let trail_head = Point::new(from.local.x + delta.0, from.local.y + delta.1);
        self.hand_off(r.to, Some(trail_head));
    }

    fn inject_scroll(
        &mut self,
        origin: DeviceId,
        source: SourceKind,
        dx: f64,
        dy: f64,
        unit: ScrollAmount,
        phase: ScrollPhase,
    ) {
        let s = &self.cfg.settings;
        let invert =
            source == SourceKind::Mouse && phase == ScrollPhase::None && self.resolved_transform(origin).reverse_wheel;
        let sign = if invert { -1.0 } else { 1.0 };
        let out_unit = s.scroll_output.unwrap_or(match self.cfg.os {
            Os::Windows => ScrollUnit::Line,
            _ => ScrollUnit::Pixel,
        });
        let mpu = self.pointer_reference_per_native_unit();
        let lh = s.line_height_ratio;
        let lpn = self.feel.wheel_lines_per_notch.filter(|v| *v > 0.0).unwrap_or(1.0);
        let k = match (unit, out_unit) {
            (ScrollAmount::Reference, ScrollUnit::Pixel) => 1.0 / mpu,
            (ScrollAmount::Lines, ScrollUnit::Pixel) => lh / mpu,
            (ScrollAmount::Reference, ScrollUnit::Line) => 1.0 / lh / lpn,
            (ScrollAmount::Lines, ScrollUnit::Line) => 1.0 / lpn,
        };
        let wheel = oink_foundation::input::ScrollInjection {
            dx: dx * k * sign * s.remote_scroll_speed,
            dy: dy * k * sign * s.remote_scroll_speed,
            unit: out_unit,
            phase,
        };
        let event = if self.cfg.os == Os::Windows
            && source == SourceKind::Touchpad
            && unit == ScrollAmount::Reference
            && phase != ScrollPhase::None
        {
            InjectEvent::TouchpadScroll {
                dx: dx * sign * s.remote_scroll_speed,
                dy: dy * sign * s.remote_scroll_speed,
                pixels_per_reference: 1.0 / mpu,
                wheel,
            }
        } else {
            InjectEvent::Scroll {
                dx: wheel.dx,
                dy: wheel.dy,
                unit: wheel.unit,
                phase,
            }
        };
        self.out.push(Output::Inject(event));
    }

    fn held_sync(&mut self, from: DeviceId, origin: DeviceId, keys: &[KeyCode], buttons: &[MouseButton]) {
        if let Some(h) = self.held.get(&origin) {
            let stale_keys: Vec<KeyCode> = h.keys.keys().filter(|k| !keys.contains(k)).copied().collect();
            let mut stale_buttons: Vec<MouseButton> =
                h.buttons.iter().filter(|b| !buttons.contains(b)).copied().collect();
            if self.dnd_keep_left(origin, buttons) {
                // 拖拽刚交到本机，输入源还没改目标。
                stale_buttons.retain(|b| *b != MouseButton::Left);
            }
            for k in stale_keys {
                if self.forget_press_route(from, origin, HeldItem::Key(k)) {
                    tracing::warn!(?origin, "HeldSync: releasing key the source no longer holds");
                    self.target_key(origin, k, false);
                }
            }
            for b in stale_buttons {
                if self.forget_press_route(from, origin, HeldItem::Button(b)) {
                    tracing::warn!(?origin, ?b, "HeldSync: releasing button the source no longer holds");
                    self.target_button(origin, b, false);
                }
            }
        }
        let stale_fwd: Vec<HeldItem> = self
            .forwarded
            .keys()
            .filter(|(o, item)| {
                *o == origin
                    && match item {
                        HeldItem::Key(k) => !keys.contains(k),
                        HeldItem::Button(b) => !buttons.contains(b),
                    }
            })
            .map(|(_, item)| *item)
            .collect();
        for item in stale_fwd {
            if self.forget_press_route(from, origin, item) {
                self.release_press(origin, item);
            }
        }
    }

    fn send_forwarded_sync(&mut self) {
        // Snapshot our outgoing paths, never a source's per-destination list. Sending on
        // our own heartbeat also heals a lost release after the upstream peer goes away.
        // Retain empty routes and refresh them every four heartbeats, just like direct
        // HeldSync: a one-off empty snapshot could itself be lost with the release.
        let routes: Vec<_> = self.forwarded_sync.iter().copied().collect();
        for (origin, dst) in routes {
            let mut keys = Vec::new();
            let mut buttons = Vec::new();
            for ((o, item), d) in &self.forwarded {
                if *o == origin && *d == dst {
                    match item {
                        HeldItem::Key(k) => keys.push(*k),
                        HeldItem::Button(b) => buttons.push(*b),
                    }
                }
            }
            if !keys.is_empty() || !buttons.is_empty() || self.sync_seq.is_multiple_of(4) {
                self.send(
                    dst,
                    Message::Input(InputMsg::Forwarded {
                        origin: self.admitted_boots[&origin],
                        hops: 1,
                        msg: Box::new(InputMsg::HeldSync { keys, buttons }),
                    }),
                );
            }
        }
    }

    // ================= 目标端注入 =================

    fn active_volume_exit(&self) -> Option<crate::input::VolumeRoute> {
        self.volume_exit.as_ref().and_then(|probe| probe()).filter(|route| {
            route.source == self.cfg.boot
                && route.target.device_id != self.me()
                && self.admitted_boots.get(&route.target.device_id) == Some(&route.target)
                && self.reachable(route.target.device_id)
        })
    }

    fn begin_volume(&mut self, origin: DeviceId, code: KeyCode, route: crate::input::VolumeRoute) {
        let exit = route.target.device_id;
        // Use source HID modifiers before the destination's Cmd/Ctrl remapping.
        let meta = self.volume_modifiers(origin) & 0x88 != 0;
        let target = if meta {
            VolumeTarget::Forwarding
        } else {
            self.volume_press_id = self
                .volume_press_id
                .checked_add(1)
                .expect("volume press sequence exhausted");
            VolumeTarget::Exit {
                route,
                press_id: self.volume_press_id,
            }
        };
        self.volume_decision(origin, code, Some(exit), meta);
        self.volume_targets.insert((origin, code), target);
        self.held.entry(origin).or_default().keys.insert(code, code);
        if meta {
            if origin == self.me() {
                self.mask_menu_keys();
            } else {
                self.inject_menu_mask();
            }
        }
        self.send_volume(origin, code, true, false);
    }
    fn volume_modifiers(&self, origin: DeviceId) -> u8 {
        let mut modifiers = 0u8;
        if origin == self.me() {
            for key in self.keys.keys().filter(|k| k.is_modifier()) {
                modifiers |= 1 << (key.usage() - 0xe0);
            }
        } else if let Some(held) = self.held.get(&origin) {
            for key in held.keys.keys().filter(|k| k.is_modifier()) {
                modifiers |= 1 << (key.usage() - 0xe0);
            }
        }
        modifiers
    }
    fn send_volume(&mut self, origin: DeviceId, code: KeyCode, pressed: bool, repeat: bool) -> bool {
        let Some(target) = self.volume_targets.get(&(origin, code)).cloned() else {
            return false;
        };
        match target {
            VolumeTarget::Forwarding => {
                // Mute is a toggle: auto-repeat must never toggle it again.
                let mute = code == KeyCode::new(12, 0xe2) || code == KeyCode::keyboard(0x7f);
                if pressed && !(mute && repeat) {
                    self.out.push(Output::Status(StatusEvent::AudioVolumeKey { code }));
                }
            }
            VolumeTarget::Exit { route, press_id } => self.send(
                route.target.device_id,
                Message::Input(InputMsg::Volume {
                    route,
                    press_id,
                    code,
                    pressed,
                    repeat,
                    modifiers: self.volume_modifiers(origin),
                }),
            ),
        }
        if !pressed {
            self.volume_targets.remove(&(origin, code));
        }
        true
    }

    fn target_key(&mut self, origin: DeviceId, code: KeyCode, pressed: bool) {
        if !pressed {
            self.press_routes.remove(&(origin, HeldItem::Key(code)));
        }
        if code.is_volume() && !(origin == self.me() && self.keyboard_local()) {
            if pressed
                && !self.held.get(&origin).is_some_and(|h| h.keys.contains_key(&code))
                && self.active_volume_exit().is_none()
            {
                self.volume_decision(origin, code, None, false);
            }
            if pressed
                && !self.held.get(&origin).is_some_and(|h| h.keys.contains_key(&code))
                && let Some(exit) = self.active_volume_exit()
            {
                self.begin_volume(origin, code, exit);
                return;
            }
            if !pressed && self.send_volume(origin, code, false, false) {
                if let Some(h) = self.held.get_mut(&origin) {
                    h.keys.remove(&code);
                    h.repeats.remove(&code);
                }
                return;
            }
        }
        if code == KeyCode::MENU_MASK && self.cfg.os != Os::Windows {
            // 菜单遮罩键只对 Windows 有意义，其他系统忽略。
            return;
        }
        let mapped = self.map_key(origin, code);
        self.target_mapped_key(origin, code, mapped, pressed);
    }

    fn target_mapped_key(&mut self, origin: DeviceId, code: KeyCode, mapped: KeyCode, pressed: bool) {
        let h = self.held.entry(origin).or_default();
        if pressed {
            if h.keys.contains_key(&code) {
                return;
            }
            h.keys.insert(code, mapped);
            let rc = self.key_refs.entry(mapped).or_insert(0);
            *rc += 1;
            if *rc == 1 {
                self.out.push(Output::Inject(InjectEvent::Key {
                    code: mapped,
                    pressed: true,
                }));
            }
        } else {
            let Some(mapped) = h.keys.remove(&code) else {
                return;
            };
            h.repeats.remove(&code);
            self.release_key_ref(mapped);
        }
    }

    fn target_button(&mut self, origin: DeviceId, button: MouseButton, pressed: bool) {
        if !pressed {
            self.press_routes.remove(&(origin, HeldItem::Button(button)));
        }
        tracing::debug!(
            ?origin,
            ?button,
            pressed,
            held = self.held.get(&origin).is_some_and(|h| h.buttons.contains(&button)),
            "button trace: received at target"
        );
        let h = self.held.entry(origin).or_default();
        if pressed {
            if !h.buttons.insert(button) {
                return;
            }
            self.button_diagnostics.press(origin, button, self.dnd.now, None);
            let rc = self.button_refs.entry(button).or_insert(0);
            *rc += 1;
            if *rc == 1 {
                if button == MouseButton::Left {
                    self.dnd.press_origin = self.pointer.clone();
                }
                self.out
                    .push(Output::Inject(InjectEvent::Button { button, pressed: true }));
            }
        } else {
            if !h.buttons.remove(&button) {
                return;
            }
            self.button_diagnostics.release(origin, button);
            self.release_button_ref(button);
        }
    }

    fn release_key_ref(&mut self, code: KeyCode) {
        if let Some(rc) = self.key_refs.get_mut(&code) {
            *rc -= 1;
            if *rc == 0 {
                self.key_refs.remove(&code);
                self.out.push(Output::Inject(InjectEvent::Key { code, pressed: false }));
            }
        }
    }

    fn release_button_ref(&mut self, button: MouseButton) {
        if let Some(rc) = self.button_refs.get_mut(&button) {
            *rc -= 1;
            if *rc == 0 {
                self.button_refs.remove(&button);
                self.out
                    .push(Output::Inject(InjectEvent::Button { button, pressed: false }));
            }
        }
    }

    /// 释放为某个源按下的所有键和按钮（断线、心跳超时）。
    fn release_origin(&mut self, origin: DeviceId) {
        self.button_diagnostics.release_origin(origin);
        self.multifinger.remove(&origin);
        if self.pending_contacts.origin == Some(origin) {
            self.pending_contacts.clear();
        }
        if self.contacts_received.is_some_and(|(peer, _)| peer == origin) {
            self.cancel_received_contacts();
        }
        let volume: Vec<_> = self
            .volume_targets
            .keys()
            .filter(|(o, _)| *o == origin)
            .map(|(_, k)| *k)
            .collect();
        for code in volume {
            self.target_key(origin, code, false);
        }
        if let Some(g) = self.gesture_active.remove(&origin) {
            // 源断线时手势停在半途（Dock 轻扫停在中间）：补一个取消。
            let cancel = match g {
                Gesture::DockSwipe { axis, progress, .. } => Gesture::DockSwipe {
                    axis,
                    phase: GesturePhase::Cancelled,
                    progress,
                    velocity: 0.0,
                },
                Gesture::Magnify { .. } => Gesture::Magnify {
                    phase: GesturePhase::Cancelled,
                    amount: 0.0,
                },
                Gesture::Rotate { .. } => Gesture::Rotate {
                    phase: GesturePhase::Cancelled,
                    degrees: 0.0,
                },
                Gesture::SmartMagnify => Gesture::SmartMagnify,
            };
            if cancel != Gesture::SmartMagnify {
                self.out.push(Output::Inject(InjectEvent::Gesture(cancel)));
            }
        }
        if let Some(h) = self.held.remove(&origin) {
            for (code, mapped) in h.keys {
                self.press_routes.remove(&(origin, HeldItem::Key(code)));
                self.release_key_ref(mapped);
            }
            for b in h.buttons {
                self.press_routes.remove(&(origin, HeldItem::Button(b)));
                self.release_button_ref(b);
            }
        }
    }

    pub(super) fn release_local_transformed(&mut self) {
        for (physical, mapped) in std::mem::take(&mut self.local_mapped) {
            if physical != mapped {
                self.out.push(Output::Inject(InjectEvent::Key {
                    code: mapped,
                    pressed: false,
                }));
            }
        }
        self.local_rewrite = None;
    }
    pub fn local_rewrite(&self) -> Option<oink_foundation::input::LocalRewrite> {
        self.local_rewrite
    }
    pub fn local_modifier_mapping(&self) -> (Vec<(KeyCode, KeyCode)>, Vec<KeyCode>) {
        (
            self.local_mapped
                .iter()
                .chain(self.held.get(&self.me()).into_iter().flat_map(|h| h.keys.iter()))
                .map(|(&from, &to)| (from, to))
                .collect(),
            self.key_refs.keys().filter(|k| k.is_modifier()).copied().collect(),
        )
    }
    pub fn set_local_input_transform(&mut self, settings: oink_foundation::input_transform::InputTransform) {
        self.input_transforms.insert(self.me(), settings);
    }

    pub fn set_peer_input_transform(
        &mut self,
        boot: oink_foundation::session::Boot,
        transform: Option<oink_foundation::input_transform::InputTransform>,
    ) {
        if boot.device_id == self.me() || self.admitted_boots.get(&boot.device_id) != Some(&boot) {
            return;
        }
        if let Some(transform) = transform {
            self.input_transforms.insert(boot.device_id, transform);
        } else {
            self.input_transforms.remove(&boot.device_id);
        }
    }
    pub fn set_input_transforms(
        &mut self,
        settings: HashMap<DeviceId, oink_foundation::input_transform::InputTransform>,
    ) {
        self.input_transforms = settings;
    }
    fn resolved_transform(&self, origin: DeviceId) -> oink_foundation::input_transform::TargetTransform {
        let source_os = if origin == self.me() {
            self.cfg.os
        } else {
            self.peers.get(&origin).map_or(Os::Other, |p| p.os)
        };
        self.input_transforms.get(&origin).map_or_else(
            || {
                oink_foundation::input_transform::InputTransform::initial(source_os.as_str())
                    .resolve(self.me(), self.cfg.os.as_str())
            },
            |s| s.resolve(self.me(), self.cfg.os.as_str()),
        )
    }
    fn map_key(&self, origin: DeviceId, code: KeyCode) -> KeyCode {
        let keyboard = self.resolved_transform(origin).keyboard;
        if origin == self.me()
            && self.cfg.os == Os::MacOS
            && keyboard
                .table
                .iter()
                .find(|m| m.from == code)
                .is_some_and(|m| oink_foundation::input_transform::mac_local_mapping_hint(*m).is_some())
        {
            // Caps Lock owns HID/TSM lock state, NX media keys own system events,
            // and Fn owns its separate modifier state. A session keycode rewrite
            // cannot transform those paths correctly; skip this entry locally.
            return code;
        }
        keyboard.map(code)
    }

    // ================= 焦点 =================

    /// 本机是焦点机，把焦点交给 `dst` 所在的设备，并立即冻结源光标。
    fn hand_off(&mut self, dst: ScreenPos, trail_head: Option<Point>) {
        self.diagnostic_state("departing");
        self.diagnostic(DecisionKind::Handoff, DecisionReason::EdgeCrossed, Some(&dst));
        let epoch = self.epoch + 1;
        oink_foundation::handoff_timing::emit(oink_foundation::handoff_timing::Event::new(
            oink_foundation::handoff_timing::Stage::RoutingDecision,
            oink_foundation::handoff_timing::Token { epoch, sequence: 0 },
        ));
        self.focus_scope = Some(self.new_focus_scope());
        self.focus_joined = true;
        self.fresh_join = false;
        let to = dst.screen.device;
        self.diagnostic_log(line!(), "focus handoff", epoch);
        tracing::info!(epoch, ?to, display = %dst.screen.display, "focus handoff");
        if let Some(m) = self.take_dnd_enter(to, epoch) {
            self.send(to, m);
        }
        self.trail_handoff(&dst, epoch, trail_head);
        self.send(
            to,
            Message::Control(ControlMsg::FocusEnter {
                epoch,
                display: dst.screen.display.clone(),
                pos: dst.local,
                scope: self.focus_scope.clone(),
                pointer_source: self.last_pointer_source,
            }),
        );
        let others: Vec<DeviceId> = self
            .peers
            .iter()
            .filter(|(d, p)| **d != to && p.connected)
            .map(|(d, _)| *d)
            .collect();
        for d in others {
            self.send(
                d,
                Message::Control(ControlMsg::FocusState {
                    lock_state: self.focus_lock.clone(),
                    epoch,
                    holder: to,
                    scope: self.focus_scope.clone(),
                    session: Some(self.session),
                }),
            );
        }
        self.claim = to;
        self.become_source(epoch, to);
    }

    pub(super) fn cancel_source_scroll(&mut self) {
        let Some(route) = self.scroll_target else { return };
        // macOS can deliver finger and momentum phases independently, including
        // an Ended with no momentum. Before discarding physical tails at a focus
        // or capture boundary, close both live parts at the ORIGINAL target.
        // Otherwise that target's shared multifinger filter waits for an end the
        // source has swallowed, and rejects later native contacts as well.
        for phase in [
            Some(ScrollPhase::Cancelled),
            route.momentum.then_some(ScrollPhase::MomentumEnded),
        ]
        .into_iter()
        .flatten()
        {
            match route.target {
                Target::Remote(to) => self.send(
                    to,
                    Message::Input(InputMsg::Scroll {
                        source: SourceKind::Touchpad,
                        dx: 0.,
                        dy: 0.,
                        unit: ScrollAmount::Reference,
                        phase,
                    }),
                ),
                Target::Local => self.out.push(Output::Inject(InjectEvent::Scroll {
                    dx: 0.,
                    dy: 0.,
                    unit: ScrollUnit::Pixel,
                    phase,
                })),
                _ => {}
            }
        }
        self.scroll_target = Some(ScrollRoute {
            target: Target::Dropped,
            ..route
        });
    }

    fn drop_scroll_remainder(&mut self) {
        if !self.mouse_local() {
            self.cancel_source_contacts();
            self.cancel_local_gesture();
        }
        self.retire_received_multifinger();
        if !self.mouse_local() {
            self.cancel_source_scroll();
        }
        self.scroll_fwd.clear();
    }

    fn become_source(&mut self, epoch: u64, holder: DeviceId) {
        let was_gap = self.gap.take().is_some();
        self.gap_input.modifiers.clear();
        let me = self.me();
        let was_holder = self.holder == me;
        let same_holder = self.holder == holder;
        if !same_holder && !self.keyboard_local() {
            self.cancel_caps();
        }
        if !same_holder && self.layout.contains_device(self.holder) {
            self.last_enabled_focus = Some(self.holder);
        }
        if was_holder {
            self.dnd_focus_left();
        }
        if self.holder != holder {
            self.drop_scroll_remainder();
        }
        self.epoch = epoch;
        self.holder = holder;
        if same_holder && !was_holder && self.locked_device() == Some(holder) {
            self.keep_game_focus_epoch();
        }
        self.resistance = None;
        self.unconfirmed_since = None;
        self.rejoin = false;
        self.enter_due = None;
        if was_holder {
            if !was_gap {
                self.local_cursor = self.pointer.clone();
            }
            self.exit_point = self.pointer.clone();
            self.ref_display = self.pointer.as_ref().map(|p| p.screen.display.clone());
            // Leaving a shared destination hides its cursor regardless of its
            // physical source mode. An independent cursor revealed during gap
            // travel already belongs to the local mouse and stays usable.
            if !self.mouse_local() || !was_gap || self.local_mouse_hidden {
                self.local_mouse_hidden = self.mouse_local();
                let park = self.park_point();
                self.out.push(Output::SetMode(CaptureMode::Remote { park }));
            }
        }
        self.retarget_modifiers();
        self.push_focus_status();
    }

    fn become_holder(&mut self, epoch: u64, at: Option<ScreenPos>) {
        if self.holder != self.me() && !self.keyboard_local() {
            self.cancel_caps();
        }
        let was_gap = self.gap.take().is_some();
        let me = self.me();
        let was_holder = self.holder == me;
        if !was_holder && self.layout.contains_device(self.holder) {
            self.last_enabled_focus = Some(self.holder);
        }
        if !was_holder {
            self.drop_scroll_remainder();
        }
        self.epoch = epoch;
        self.holder = me;
        self.local_mouse_hidden = false;
        self.last_pointer_source = None;
        self.unconfirmed_since = None;
        self.rejoin = false;
        self.enter_due = None;
        self.resistance = None;
        if !was_holder || was_gap {
            // 光标落点交给 backend 与模式切换一起原子地执行（先在隐藏状态下放到进入点、再切模式、再显示）：
            // 分开输出「注入 MoveTo」与「切模式」时，两者在不同的队列里执行，中间的物理移动会把光标拉回
            // 停靠点（屏幕中央），光标先在中央露出来、再从中央出发。
            match &at {
                Some(p) => self.out.push(Output::RestoreLocal { at: p.local }),
                None => self.out.push(Output::SetMode(CaptureMode::Local)),
            }
        } else if let Some(p) = &at {
            self.out.push(Output::Inject(InjectEvent::MoveTo { pos: p.local }));
        }
        if let Some(p) = at {
            self.pointer = Some(p);
        }
        self.exit_point = None;
        if was_gap {
            self.gap_resume_modifiers();
        } else {
            self.retarget_modifiers();
        }
        self.push_focus_status();
    }

    /// 只在本机收回焦点（岛）：epoch 与组内焦点 `claim` 不变，光标回到离开本机时的位置。
    fn island(&mut self) {
        if self.holder == self.me() {
            return;
        }
        let at = self.exit_point.clone().or_else(|| self.pointer.clone());
        let epoch = self.epoch;
        self.become_holder(epoch, at);
    }

    /// 焦点变化后，把本机键盘仍按着的修饰键同步到新焦点机（「修饰键状态快照」）。
    /// 已按下的普通键不跟随（按下时的目标即抬起时的目标）。
    fn retarget_modifiers(&mut self) {
        if self.keyboard_local() {
            return;
        }
        let me = self.me();
        let holder = self.holder;
        let mods: Vec<KeyCode> = self.keys.keys().filter(|k| k.is_modifier()).copied().collect();
        let mut menu_key = false;
        for code in mods {
            let ts = self.keys.get(&code).cloned().unwrap_or_default();
            if holder == me {
                if !ts.contains(&Target::Local) && !ts.contains(&Target::LocalInjected) {
                    self.target_key(me, code, true);
                    self.keys.entry(code).or_default().push(Target::LocalInjected);
                    menu_key |= opens_menu(code);
                }
            } else if !ts.contains(&Target::Remote(holder)) {
                self.send(holder, Message::Input(InputMsg::Key { code, pressed: true }));
                self.keys.entry(code).or_default().push(Target::Remote(holder));
                menu_key |= opens_menu(code);
            }
        }
        // 跟过去的 Alt / Win 在新焦点机上没有配上任何普通键：用户在那边直接松开时，Windows 会把它当作单独按了
        // 一下 Alt（激活菜单栏）/ Win（打开开始菜单）。补一次菜单遮罩键。
        if menu_key {
            if holder == me {
                self.inject_menu_mask();
            } else {
                self.send_menu_mask(holder);
            }
        }
    }

    /// 快捷键吞掉了普通键：按着的 Alt / Win 已经交给了某些目标，它们在那里只剩「按下、松开」，
    /// Windows 会当作单独按了一下 Alt / Win。在松开之前给每个这样的目标补一次菜单遮罩键。
    fn mask_menu_keys(&mut self) {
        let mut local = false;
        let mut remote = BTreeSet::new();
        for (code, ts) in &self.keys {
            if !opens_menu(*code) {
                continue;
            }
            for t in ts {
                match t {
                    Target::Local | Target::LocalInjected => local = true,
                    Target::Remote(d) => {
                        remote.insert(*d);
                    }
                    Target::Dropped => {}
                }
            }
        }
        if local {
            self.inject_menu_mask();
        }
        for d in remote {
            self.send_menu_mask(d);
        }
    }

    /// 在本机注入一次菜单遮罩键（只有 Windows 需要）。
    fn inject_menu_mask(&mut self) {
        if self.cfg.os != Os::Windows {
            return;
        }
        for pressed in [true, false] {
            self.out.push(Output::Inject(InjectEvent::Key {
                code: KeyCode::MENU_MASK,
                pressed,
            }));
        }
    }

    /// 给远端发一次菜单遮罩键（接收端按自己的系统决定注入还是忽略）。
    fn send_menu_mask(&mut self, to: DeviceId) {
        for pressed in [true, false] {
            self.send(
                to,
                Message::Input(InputMsg::Key {
                    code: KeyCode::MENU_MASK,
                    pressed,
                }),
            );
        }
    }

    /// 对端断开或心跳超时。
    fn peer_lost(&mut self, peer: DeviceId) {
        self.cancel_caps_peer(peer);
        self.remote_volume_origins.remove(&peer);
        if self.locked_device() == Some(peer) {
            self.end_focus_lock();
        }
        self.gap_peer_lost(peer);
        self.diagnostic_log(line!(), "peer lost: releasing its keys and buttons", self.epoch);
        tracing::info!(?peer, "peer lost: releasing its keys and buttons");
        self.trail_peer_lost(peer);
        // Retire the drag first: releasing its left button here would drop it under the cursor.
        self.dnd_peer_lost(peer);
        // 目标端：释放它按下的一切，停止重复。
        self.release_origin(peer);
        // The input source may still be online when an intermediate forwarder is lost.
        // Release only paths owned by that peer; direct modifier snapshots can survive.
        let via_peer: Vec<_> = self
            .press_routes
            .iter()
            .filter(|(_, routes)| routes.contains(&peer))
            .map(|(key, _)| *key)
            .collect();
        for (origin, item) in via_peer {
            if self.forget_press_route(peer, origin, item) {
                self.release_press(origin, item);
            }
        }
        // 转发表：它发起的按下补发抬起；发往它的无法送达，丢弃。
        let from_peer: Vec<((DeviceId, HeldItem), DeviceId)> = self
            .forwarded
            .iter()
            .filter(|((o, _), _)| *o == peer)
            .map(|(k, v)| (*k, *v))
            .collect();
        for ((o, item), dst) in from_peer {
            self.forwarded.remove(&(o, item));
            self.press_routes.remove(&(o, item));
            self.send(dst, forwarded_release(self.admitted_boots[&o], item));
        }
        self.forwarded.retain(|key, dst| {
            if *dst == peer {
                self.press_routes.remove(key);
                false
            } else {
                true
            }
        });
        self.forwarded_sync
            .retain(|(origin, dst)| *origin != peer && *dst != peer);
        self.scroll_fwd.retain(|o, route| *o != peer && route.target != peer);
        self.gesture_fwd.retain(|o, dst| *o != peer && *dst != peer);
        // 源端：发往它的按键抬起时静默吞掉。
        for ts in self.keys.values_mut() {
            for t in ts.iter_mut() {
                if *t == Target::Remote(peer) {
                    *t = Target::Dropped;
                }
            }
        }
        for t in self.buttons.values_mut() {
            if *t == Target::Remote(peer) {
                *t = Target::Dropped;
            }
        }
        if self.scroll_target.is_some_and(|r| r.target == Target::Remote(peer)) {
            self.scroll_target = None;
        }
        if self.gesture_lock == Some(Target::Remote(peer)) {
            // 手势剩下的阶段吞掉：半个手势不交给本机。
            self.gesture_lock = Some(Target::Dropped);
        }
        // 焦点在它那里：收回焦点，光标回到离开本机时的位置（epoch、组内焦点不变，不影响其他设备；岛）。
        if self.holder == peer {
            self.island();
            self.rejoin = true;
        }
    }

    // ================= 辅助 =================

    fn me(&self) -> DeviceId {
        self.cfg.device
    }

    fn sync_step(&self) -> u64 {
        // Empty intermediate turns send nothing. Preserve the 500 ms focus/game
        // gossip and 1 s empty HeldSync cadence, including forwarded releases.
        // A new held key or relative-input mode immediately restores every turn.
        if self.sync_seq.is_multiple_of(FOCUS_GOSSIP_EVERY)
            && self.keys.is_empty()
            && self.buttons.is_empty()
            && self.forwarded.is_empty()
            && !self.game_mode().active
            && !self.peers.values().any(|p| p.held_sync_nonempty)
        {
            FOCUS_GOSSIP_EVERY
        } else {
            1
        }
    }
    fn sync_deadline(&self) -> Option<Micros> {
        self.next_sync
            .map(|at| at + (self.sync_step() - 1) * self.interval_us())
    }
    fn interval_us(&self) -> Micros {
        u64::from(self.cfg.settings.heartbeat_interval_ms.max(1)) * 1000
    }

    fn timeout_us(&self) -> Micros {
        u64::from(
            self.cfg
                .settings
                .heartbeat_timeout_ms
                .max(crate::config::MIN_HEARTBEAT_TIMEOUT_MS),
        ) * 1000
    }

    fn reachable(&self, d: DeviceId) -> bool {
        d == self.me() || self.peers.get(&d).is_some_and(|p| p.connected)
    }

    /// Input-session readiness, independent of geometry or display facts.
    /// Screen destinations come from layout queries; focus gossip is membership state.
    fn input_ready(&self, d: DeviceId) -> bool {
        d == self.me()
            || self
                .peers
                .get(&d)
                .is_some_and(|p| p.connected && p.sharing && p.focus_ready)
    }

    /// 在线设备：本机 + 已连接、心跳正常、没有暂停共享的对端。
    fn online_set(&self) -> BTreeSet<DeviceId> {
        let mut s: BTreeSet<DeviceId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected && p.sharing && p.focus_ready)
            .map(|(d, _)| *d)
            .collect();
        s.insert(self.me());
        s
    }

    /// Local input readiness, independent of the configuration topology snapshot.
    pub fn input_online(&self) -> BTreeSet<DeviceId> {
        self.online_set()
    }

    fn current_target(&self) -> Target {
        if self.holder == self.me() {
            Target::Local
        } else {
            Target::Remote(self.holder)
        }
    }

    fn handoff_blocked(&self) -> bool {
        self.cfg.settings.button_handoff == ButtonHandoff::Block
            && (!self.button_refs.is_empty()
                || self.buttons.values().any(|t| *t == Target::Local)
                // 正在把拖拽交出去：左键已由平台抬起，但移交要等本机的拖拽结束。
                || self.dnd_leaving())
    }

    /// 通往其他设备的边是墙：拖动中（`handoff_blocked`）、锁定当前焦点、共享暂停。
    fn leave_blocked(&self) -> bool {
        self.projection_gesture || self.focus_pinned() || self.paused || self.handoff_blocked()
    }

    fn send(&mut self, to: DeviceId, msg: Message) {
        if self.fully_paused {
            return;
        }
        if !self.peers.get(&to).is_some_and(|p| p.connected) {
            return;
        }
        let delivery = match &msg {
            Message::Input(i) if i.is_datagram() => Delivery::Datagram,
            _ => Delivery::Reliable,
        };
        self.out.push(Output::Send { to, msg, delivery });
    }

    fn broadcast(&mut self, msg: Message) {
        let peers: Vec<DeviceId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(d, _)| *d)
            .collect();
        for d in peers {
            self.send(d, msg.clone());
        }
    }

    fn new_focus_scope(&self) -> FocusScope {
        let mut members = vec![(self.me(), self.session)];
        members.extend(
            self.peers
                .iter()
                .filter_map(|(id, peer)| peer.session.map(|s| (*id, s))),
        );
        FocusScope { members }
    }

    fn scope_allows(&self, scope: Option<&FocusScope>, holder: DeviceId) -> bool {
        let Some(scope) = scope else {
            return false;
        };
        if !scope.members.contains(&(self.me(), self.session)) {
            return false;
        }
        if holder == self.me() {
            return true;
        }
        self.peers
            .get(&holder)
            .and_then(|p| p.session)
            .is_some_and(|session| scope.members.contains(&(holder, session)))
    }

    fn send_focus_state(&mut self, peer: DeviceId) {
        let msg = ControlMsg::FocusState {
            lock_state: self.focus_lock.clone(),
            epoch: self.epoch,
            holder: self.claim,
            scope: self.focus_scope.clone(),
            session: Some(self.session),
        };
        self.send(peer, Message::Control(msg));
    }

    fn send_motion(&mut self, dx: f64, dy: f64) {
        self.motion_seq += 1;
        let msg = InputMsg::Motion {
            seq: self.motion_seq,
            dx,
            dy,
        };
        self.send(self.holder, Message::Input(msg));
    }

    fn send_state_sync(&mut self) {
        self.send_relative_total();
        self.sync_seq += 1;
        let peers: Vec<DeviceId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(d, _)| *d)
            .collect();
        let gossip = self.sync_seq.is_multiple_of(FOCUS_GOSSIP_EVERY);
        for d in peers {
            if gossip {
                self.send_game_state(d);
                // 焦点自愈：漏掉的移交、分叉的 epoch 在一两次通告内收敛。
                self.send_focus_state(d);
            }
            let mut keys: Vec<KeyCode> = self
                .keys
                .iter()
                .filter(|(_, ts)| ts.contains(&Target::Remote(d)))
                .map(|(k, _)| *k)
                .collect();
            if self.caps_press.is_some_and(|c| c.held_on(d)) {
                keys.push(KeyCode::CAPS_LOCK);
            }
            let buttons: Vec<MouseButton> = self
                .buttons
                .iter()
                .filter(|(_, t)| **t == Target::Remote(d))
                .map(|(b, _)| *b)
                .collect();
            let nonempty = !keys.is_empty() || !buttons.is_empty();
            let was = self.peers.get(&d).is_some_and(|p| p.held_sync_nonempty);
            // 非空时每次都发；变空后补发一次；另外每 4 次心跳发一次（自愈：目标端释放源端已不再按着的键）。
            if nonempty || was || self.sync_seq.is_multiple_of(4) {
                self.send(d, Message::Input(InputMsg::HeldSync { keys, buttons }));
            }
            if let Some(p) = self.peers.get_mut(&d) {
                p.held_sync_nonempty = nonempty;
            }
        }
        self.send_forwarded_sync();
    }

    fn push_focus_status(&mut self) {
        self.reset_game_focus();
        let info = self.focus();
        self.out.push(Output::Status(StatusEvent::Focus(info)));
    }

    /// 本机全局坐标 `p` 所在的显示器；不在任何显示器内时取指针当前所在的显示器。
    fn locate(&self, p: Point) -> Option<(String, Rect)> {
        if let Some(d) = self.displays.iter().find(|d| d.bounds.contains(p)) {
            return Some((d.key.clone(), d.bounds));
        }
        let cur = self.pointer.as_ref().map(|q| q.screen.display.as_str());
        self.displays
            .iter()
            .find(|d| Some(d.key.as_str()) == cur)
            .or_else(|| self.displays.first())
            .map(|d| (d.key.clone(), d.bounds))
    }

    /// 焦点在远端时本机光标停靠的位置：参考屏中央。
    fn park_point(&self) -> Point {
        let key = self.ref_display.as_deref();
        self.displays
            .iter()
            .find(|d| Some(d.key.as_str()) == key)
            .or_else(|| self.displays.first())
            .map_or(Point::default(), |d| d.bounds.center())
    }

    fn display_reference_per_native_unit(&self, key: &str) -> f64 {
        let id = ScreenId::new(self.me(), key);
        self.layout
            .source_reference_per_native_unit(&id)
            .or_else(|| {
                self.displays
                    .iter()
                    .find(|d| d.key == key)
                    .and_then(|d| d.reference_per_native_unit())
            })
            .filter(|v| *v > 0.0)
            .unwrap_or(self.cfg.settings.fallback_reference_per_native_unit)
    }

    /// 本机作为源时，本机单位 → 视平面 plane 的系数（参考屏）。
    fn reference_per_native_unit(&self) -> f64 {
        let key = self
            .cfg
            .settings
            .fixed_reference_display
            .clone()
            .or_else(|| self.ref_display.clone())
            .or_else(|| self.pointer.as_ref().map(|p| p.screen.display.clone()))
            .or_else(|| self.displays.first().map(|d| d.key.clone()));
        match key {
            Some(k) => self.display_reference_per_native_unit(&k),
            None => self.cfg.settings.fallback_reference_per_native_unit,
        }
    }

    fn pointer_reference_per_native_unit(&self) -> f64 {
        match &self.pointer {
            Some(p) => self.display_reference_per_native_unit(&p.screen.display),
            None => self.reference_per_native_unit(),
        }
    }

    /// 被墙挡住的一步：移到 `to`，同时给出这一步没被裁剪时的位移（手部 plane 换成 `to` 所在屏幕的本机单位）。
    fn pushed_move(&self, to: &ScreenPos, dx: f64, dy: f64, source: DeviceId) -> InjectEvent {
        let k = self.display_reference_per_native_unit(&to.screen.display)
            / self.layout.remote_pointer_gain(&to.screen, source);
        InjectEvent::MoveToWithDelta {
            pos: to.local,
            dx: dx / k,
            dy: dy / k,
        }
    }

    fn exit_normal(&self, from: &ScreenPos, delta: (f64, f64)) -> Option<(f64, f64)> {
        self.exit_edge(from, delta).map(|edge| edge.normal)
    }

    /// 在当前屏幕内移动并裁剪到屏幕边缘（不能移交时用）。
    fn clamp_move(&self, from: &ScreenPos, dx: f64, dy: f64, source: DeviceId) -> Option<ScreenPos> {
        let gain = self.layout.remote_pointer_gain(&from.screen, source);
        let plane = self.layout.to_plane(from)?;
        let local = self
            .layout
            .from_plane(&from.screen, Point::new(plane.x + dx * gain, plane.y + dy * gain))?;
        let bounds = self.displays.iter().find(|d| d.key == from.screen.display)?.bounds;
        Some(ScreenPos::new(from.screen.clone(), bounds.clamp(local)))
    }
}

/// 组内焦点的先后：`epoch` 大的新；`epoch` 相同（两台设备在同一 epoch 上各自移交了焦点）时
/// 设备 ID 大的胜出，所有设备按同一规则得到同一结果。`epoch` 为 0（还没有发生过移交）时不比较。
fn claim_newer(a: (u64, DeviceId), b: (u64, DeviceId)) -> bool {
    a.0 > b.0 || (a.0 == b.0 && a.0 > 0 && a.1 > b.1)
}

/// 单独按下又松开时 Windows 会有反应的修饰键：Alt（激活菜单栏）、Win（开始菜单）。
fn opens_menu(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::LEFT_ALT | KeyCode::RIGHT_ALT | KeyCode::LEFT_GUI | KeyCode::RIGHT_GUI
    )
}

fn disposition_for(pass: bool) -> Disposition {
    if pass { Disposition::Pass } else { Disposition::Swallow }
}

/// 转发自动重复的键：键盘页除修饰键与 Caps Lock 之外的键，以及 Consumer 页（音量按住时系统重复）。
fn repeatable(code: KeyCode) -> bool {
    match code.page() {
        KeyCode::PAGE_KEYBOARD => !code.is_modifier() && code != KeyCode::CAPS_LOCK,
        KeyCode::PAGE_CONSUMER => true,
        _ => false,
    }
}

fn forwarded_release(origin: oink_foundation::session::Boot, item: HeldItem) -> Message {
    let msg = match item {
        HeldItem::Key(code) => InputMsg::Key { code, pressed: false },
        HeldItem::Button(button) => InputMsg::Button { button, pressed: false },
    };
    Message::Input(InputMsg::Forwarded {
        origin,
        hops: 1,
        msg: Box::new(msg),
    })
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod volume_tests;

#[cfg(test)]
mod scroll_tests;

#[cfg(test)]
mod transform_tests;

#[cfg(test)]
mod trigger_tests;

impl Router {
    fn crossing_uses_zone(&self, from: &ScreenPos, delta: (f64, f64), local: bool) -> bool {
        let hand = if local {
            if let Some(t) = self.layout.gap_topology() {
                let id = oink_display::layout::ScreenId::new(from.screen.device, from.screen.display.clone());
                let Some(s) = t.screen(&id) else {
                    return false;
                };
                let (sx, sy) = s.physical_scale();
                let gain = t.plane_gain(&id).unwrap();
                (delta.0 * sx / gain, delta.1 * sy / gain)
            } else {
                return false;
            }
        } else {
            delta
        };
        let source = if local {
            self.me()
        } else {
            self.last_pointer_source.unwrap_or(self.me())
        };
        self.layout
            .first_portal_from_source(from, hand, source)
            .is_some_and(|p| p.trigger_zone)
    }
}

#[cfg(test)]
mod local_modes_tests;

#[cfg(test)]
mod caps_tests;
