//! Connects the authoritative game, native overlay, A0 stream, and local API.
//! Shared positions use reference ratios; native overlay coordinates use the
//! display mapping. Pause selects native input and does not restart the game.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::streams::{StreamModule, aim::AimStreams};
use oink_display::layout::{Layout, OwnerLayoutPreferences};
use oink_foundation::session::Boot;
use oink_foundation::{DeviceId, Point, Rect};
use oink_locator::CursorTrail;
use oink_locator::aim::game::{Click, Config, Game, Micros, Region};
use oink_locator::aim::scene::hud_reserved;
use oink_locator::aim::{AimEvent, AimSurface, CloseReason, OpenSpec, SceneDisplay, View, ViewTarget};
use oink_network::link::{ModuleEvent, ModulePort, PostError};

use oink_locator::aim::model::{AimMsg, AimPhase, AimState, GameEvent, GameInstance};
use tokio::task::JoinHandle;

mod path;
pub(crate) mod wire;
use path::{accept_event, obsolete_game};
#[derive(Debug, PartialEq)]
enum SendResult {
    Accepted,
    Full,
    Closed,
}

use crate::api::{self, ApiError, ApiEvent, ApiResult};
use crate::node::{Inner, Node, lock};

/// 权威每隔这么久至少发一次状态（也是参与设备判断「权威还在」的心跳）。
const HEARTBEAT: Micros = 250_000;
/// 权威推进规则的间隔。
const TICK: Duration = Duration::from_millis(20);
/// 时间到之后结果在叠加层上保留多久，然后自动收起。
const RESULT_LINGER: Micros = 8_000_000;
/// 叠加层多久没有收到新状态就自己收起（心跳的 12 倍）。
const STALE_AFTER: Duration = Duration::from_secs(3);
/// 叠加层打开后最长保留多久，到时无条件收起：倒计时 + 一局 + 结果保留，再加余量。
const LIFETIME: Duration = Duration::from_secs(90);
/// 保留最近多少局的结果。
const MAX_RESULTS: usize = 10;
/// 暂停时拖尾跟随本机光标的采样间隔。
const FOLLOW_INTERVAL: Duration = Duration::from_millis(4);

/// 进行期间拖尾的常驻方式，由本机是否暂停决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrailMode {
    /// 共享开着：仍由路由驱动（指针在本机时一直画，跨设备的连续交接照常），只是不需要画圈触发、
    /// 不受开关和「按钮按着」的限制（`Router::set_trail_pinned`）。
    Pinned,
    /// 共享暂停：小游戏自己的一条拖尾，直接按本机光标位置画。不经过键鼠截获与路由（暂停时它们可能
    /// 已经停掉），也不用服务的那条拖尾（暂停时它的绘制可能被停驻）；不跨设备交接。
    Follow,
}

/// 小游戏自己的拖尾（[`TrailMode::Follow`] 用）。
#[derive(Clone)]
struct Follow {
    trail: CursorTrail,
    /// 每次开始 / 停止跟随递增；跟随线程发现不是自己那一次就退出。
    generation: Arc<AtomicU64>,
}

#[derive(Default)]
pub(crate) struct Aim {
    state: Mutex<State>,
    outgoing: Mutex<Option<tokio::sync::mpsc::Sender<(DeviceId, AimMsg)>>>,
    game_counter: AtomicU64,
    event_counter: AtomicU64,
    send_diagnostics: Arc<Mutex<SendDiagnostics>>,
    #[cfg(debug_assertions)]
    pub(crate) test_clock: std::sync::OnceLock<Arc<AtomicU64>>,
}

#[derive(Default)]
struct State {
    surface: Option<Arc<dyn AimSurface>>,
    session: Option<Session>,
    closed: BTreeMap<DeviceId, GameInstance>,
    results: VecDeque<api::AimResult>,
    tick: Option<JoinHandle<()>>,
    /// 规则参数（测试里把一局缩短；平时用默认值）。
    config: Option<Config>,
    /// 暂停时用的拖尾（平台实现才有）。
    follow: Option<Follow>,
}

struct Session {
    id: GameInstance,
    role: Role,
    /// 已经生效的拖尾常驻方式。
    trail: Option<TrailMode>,
    /// 最近一次显示的状态与时刻（给界面）。
    last: Option<(AimState, Micros)>,
}

enum Role {
    Host(Box<Host>),
    Guest { authority: DeviceId, seq: u64 },
}

struct Host {
    game: Game,
    seq: u64,
    processed: BTreeMap<DeviceId, (Boot, u64)>,
    /// 开局时本机处于暂停共享。
    native_input: bool,
    /// 进行中暂停状态变过。
    mode_changed: bool,
    /// 邀请了、还没应答的设备。
    invited: BTreeSet<DeviceId>,
    declined: BTreeSet<DeviceId>,
    /// 已盖上叠加层的设备。
    joined: BTreeSet<DeviceId>,
    sent_at: Micros,
    recorded: bool,
}

/// 本机的一块屏幕在布局里的位置：本机全局坐标 ↔ 视平面毫米。
#[derive(Clone, Debug, PartialEq)]
struct LocalScreen {
    key: String,
    local: Rect,
    plane: Rect,
}

/// 本机是否处于「暂停」（全组暂停）。**唯一**的查询入口。
fn paused(node: &Node) -> bool {
    node.link_config()
        .map_or_else(|_| node.fully_paused(), |group| group.applied().paused)
}

fn boot(node: &Node) -> Boot {
    node.links().map_or_else(|_| node.boot(), |links| links.boot())
}
fn now(node: &Node) -> Micros {
    #[cfg(debug_assertions)]
    if let Some(clock) = node.inner.aim.test_clock.get() {
        return clock.load(Ordering::Acquire);
    }
    node.now()
}
fn layout(node: &Node) -> Layout {
    node.link_config()
        .map_or_else(|_| node.layout().0, |group| (*group.applied().layout).clone())
}
fn local_displays(node: &Node) -> Vec<oink_display::DisplayInfo> {
    node.link_config()
        .map(|g| g.local_displays().displays.clone())
        .unwrap_or_default()
}
fn online_peers(node: &Node) -> Vec<DeviceId> {
    node.link_config().map_or_else(
        |_| Vec::new(),
        |group| {
            group
                .members()
                .members
                .iter()
                .filter(|m| m.online)
                .map(|m| m.device)
                .collect()
        },
    )
}

#[derive(Default, serde::Serialize)]
struct SendDiagnostics {
    refused: u64,
    last_peer: Option<DeviceId>,
    last_refusal: Option<String>,
}
impl SendDiagnostics {
    fn record(&mut self, peer: DeviceId, result: SendResult) {
        if result == SendResult::Accepted {
            return;
        }
        self.refused = self.refused.saturating_add(1);
        self.last_peer = Some(peer);
        self.last_refusal = Some(format!("{result:?}"));
        tracing::warn!(%peer, ?result, refused = self.refused, "aim event was not admitted");
    }
}

/// All game sends share one bounded FIFO. The path token is queried at actual
/// submission; migration is entirely the network owner's concern.
fn send(node: &Node, to: DeviceId, msg: AimMsg) {
    if let Some(tx) = lock(&node.inner.aim.outgoing).as_ref() {
        enqueue(tx, to, msg, &node.inner.aim.send_diagnostics);
    }
}

fn enqueue(
    tx: &tokio::sync::mpsc::Sender<(DeviceId, AimMsg)>,
    peer: DeviceId,
    message: AimMsg,
    diagnostics: &Mutex<SendDiagnostics>,
) {
    if let Err(error) = tx.try_send((peer, message)) {
        let result = match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => SendResult::Full,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => SendResult::Closed,
        };
        lock(diagnostics).record(peer, result);
    }
}

fn event(node: &Node, game_instance: GameInstance) -> GameEvent {
    let event_seq = node
        .inner
        .aim
        .event_counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .expect("game event counter exhausted")
        + 1;
    GameEvent {
        game_instance,
        sender_boot: boot(node),
        event_seq,
    }
}

/// 接上本机的全屏叠加层（平台实现或测试用的假实现）。不接时本机不能发起、也不参加小游戏。
pub(crate) fn set_surface(node: &Node, surface: Option<Arc<dyn AimSurface>>) {
    lock(&node.inner.aim.state).surface = surface;
}

/// 换一组规则参数（下一局起生效）。
pub(crate) fn set_config(node: &Node, config: Config) {
    lock(&node.inner.aim.state).config = Some(config);
}

fn emit(node: &Node) {
    node.emit(ApiEvent::Aim(status(node)));
}

pub(crate) fn status(node: &Node) -> api::AimStatus {
    let idle_native = paused(node);
    let now = now(node);
    let st = lock(&node.inner.aim.state);
    let mut out = api::AimStatus {
        available: st.surface.is_some(),
        phase: api::AimPhase::Idle,
        host: false,
        native_input: idle_native,
        remaining_ms: 0,
        score: 0,
        clicks: 0,
        devices: 0,
        target_diameter: st.config.unwrap_or_default().radius * 2.0,
        duration_ms: (st.config.unwrap_or_default().duration / 1000) as u32,
        results: st.results.iter().cloned().collect(),
    };
    let phase = |p| match p {
        AimPhase::Countdown => api::AimPhase::Countdown,
        AimPhase::Running => api::AimPhase::Running,
        AimPhase::Finished => api::AimPhase::Finished,
    };
    match st.session.as_ref() {
        None => {}
        Some(Session {
            role: Role::Host(h), ..
        }) => {
            out.phase = phase(h.game.phase());
            out.host = true;
            out.native_input = h.native_input;
            out.remaining_ms = (h.game.remaining(now) / 1000) as u32;
            out.score = h.game.score();
            out.clicks = h.game.clicks();
            out.devices = 1 + h.joined.len() as u32;
        }
        Some(Session { last, .. }) => {
            // 其它设备发起的一局：按最近收到的状态。
            out.phase = api::AimPhase::Countdown;
            if let Some((s, at)) = last {
                out.phase = phase(s.phase);
                out.native_input = s.native_input;
                out.remaining_ms = s.remaining_ms.saturating_sub((now.saturating_sub(*at) / 1000) as u32);
                out.score = s.score;
                out.clicks = s.clicks;
            }
        }
    }
    out
}

/// 开始一局（本机做权威）。
pub(crate) fn start(node: &Node) -> ApiResult<api::AimStatus> {
    let me = node.id();
    let native_input = paused(node);
    let peers: BTreeSet<DeviceId> = online_peers(node).into_iter().filter(|p| *p != me).collect();
    {
        let mut st = lock(&node.inner.aim.state);
        if st.session.is_some() {
            return Err(ApiError::InvalidState {
                message: "已有一局瞄准练习在进行".into(),
            });
        }
        let surface = st.surface.clone().ok_or_else(|| ApiError::Unsupported {
            message: "这台设备不能显示全屏叠加层".into(),
        })?;

        let game_counter = node
            .inner
            .aim
            .game_counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| ApiError::InvalidState {
                message: "game counter exhausted".into(),
            })?
            + 1;
        let id = GameInstance {
            authority_boot: boot(node),
            game_counter,
        };
        // 上一局的叠加层可能还在收起（它的绘制线程最多再跑一帧）：稍等再开。
        let mut opened = false;
        for _ in 0..25 {
            if open_surface(node, &surface, id) {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !opened {
            return Err(ApiError::Internal {
                message: "全屏叠加层没有打开".into(),
            });
        }
        let now = now(node);
        let config = st.config.unwrap_or_default();
        let regions = regions_now(node, &BTreeSet::from([me]), config.radius);
        st.session = Some(Session {
            id,
            role: Role::Host(Box::new(Host {
                game: Game::new(config, regions, now, id.game_counter ^ now),
                seq: 0,
                processed: BTreeMap::new(),
                native_input,
                mode_changed: false,
                invited: peers.clone(),
                declined: BTreeSet::new(),
                joined: BTreeSet::new(),
                sent_at: 0,
                recorded: false,
            })),
            trail: None,
            last: None,
        });
        for p in &peers {
            send(node, *p, AimMsg::Open { event: event(node, id) });
        }
        publish(node, &mut st);
        let weak = Arc::downgrade(&node.inner);
        st.tick = Some(node.inner.rt.spawn(tick_loop(weak, id)));
        tracing::info!(game = ?id, peers = peers.len(), native_input, "aim game started");
    }
    emit(node);
    Ok(status(node))
}

/// 结束当前这一局（界面的「结束」、服务退出）。
pub(crate) fn stop(node: &Node) {
    let ended = {
        let mut st = lock(&node.inner.aim.state);
        end_session(node, &mut st, true, "stopped")
    };
    if ended {
        emit(node);
    }
}

fn open_surface(node: &Node, surface: &Arc<dyn AimSurface>, id: GameInstance) -> bool {
    let (displays, _) = local_screens(node);
    let weak = Arc::downgrade(&node.inner);
    surface.open(
        OpenSpec {
            displays,
            lifetime: LIFETIME,
            stale_after: STALE_AFTER,
        },
        Arc::new(move |ev| {
            if let Some(inner) = weak.upgrade() {
                on_surface_event(&Node { inner }, id, ev);
            }
        }),
    )
}

/// 本机的显示器（叠加层要盖的全部屏幕）与它们在布局里的位置。
fn local_screens(node: &Node) -> (Vec<SceneDisplay>, Vec<LocalScreen>) {
    let me = node.id();
    let layout = layout(node);
    let displays = local_displays(node);
    let screens: Vec<LocalScreen> = layout
        .screens
        .iter()
        .filter(|s| s.id.device == me && s.rect.w > 0.0 && s.rect.h > 0.0 && s.local.w > 0.0 && s.local.h > 0.0)
        .map(|s| LocalScreen {
            key: s.id.display.clone(),
            local: s.local,
            plane: s.rect,
        })
        .collect();
    let scene = displays
        .iter()
        .map(|d| SceneDisplay {
            bounds: d.bounds,
            pixels_per_unit: d.pixels_per_unit(),
            // Native units per reference-plane unit, using the shared display estimate when unplaced.
            units_per_ratio: screens
                .iter()
                .find(|s| s.key == d.key)
                .map(|s| s.local.w / s.plane.w)
                .unwrap_or_else(|| d.bounds.w / oink_display::layout::estimate_reference_extent(d).0.w),
            builtin: d.is_builtin,
        })
        .collect();
    (scene, screens)
}

/// 可以放标靶圆心的范围：参与设备的每块启用的屏幕，从四边缩进一个半径，顶部再让出计分栏。
fn regions_now(node: &Node, devices: &BTreeSet<DeviceId>, radius: f64) -> Vec<Region> {
    let Ok(group) = node.link_config() else {
        return Vec::new();
    };
    let applied = group.applied();
    regions(&applied.layout, &applied.owners, devices, radius)
}

fn regions(
    layout: &Layout,
    owners: &BTreeMap<DeviceId, OwnerLayoutPreferences>,
    devices: &BTreeSet<DeviceId>,
    radius: f64,
) -> Vec<Region> {
    layout
        .screens
        .iter()
        .filter(|s| {
            // Explicit game participation is independent of pointer sharing and
            // pause. Per-screen preferences still exclude hidden game targets.
            devices.contains(&s.id.device)
                && owners
                    .get(&s.id.device)
                    .and_then(|owner| owner.screens.get(&s.id.display))
                    .is_none_or(|preference| preference.enabled)
        })
        .filter_map(|s| {
            let top = hud_reserved(s.builtin);
            let rect = Rect::new(
                s.rect.x + radius,
                s.rect.y + top + radius,
                s.rect.w - 2.0 * radius,
                s.rect.h - top - 2.0 * radius,
            );
            (rect.w > 0.0 && rect.h > 0.0).then(|| Region {
                device: s.id.device,
                display: s.id.display.clone(),
                rect,
            })
        })
        .collect()
}

/// 权威的状态换算成本机叠加层要显示的内容：只留本机屏幕上的标靶，坐标换成本机全局坐标。
fn view_of(state: &AimState, me: DeviceId, screens: &[LocalScreen]) -> View {
    View {
        phase: state.phase,
        remaining: Duration::from_millis(u64::from(state.remaining_ms)),
        score: state.score,
        clicks: state.clicks,
        native_input: state.native_input,
        targets: state
            .targets
            .iter()
            .filter(|t| t.device == me)
            .filter_map(|t| {
                let s = screens.iter().find(|s| s.key == t.display)?;
                let (kx, ky) = (s.local.w / s.plane.w, s.local.h / s.plane.h);
                Some(ViewTarget {
                    id: t.id,
                    center: Point::new(
                        s.local.x + (t.pos.x - s.plane.x) * kx,
                        s.local.y + (t.pos.y - s.plane.y) * ky,
                    ),
                    // 两个方向的比例通常相同；不同时取几何平均（圆仍画成圆，与拖尾的做法一致）。
                    radius: f64::from(state.radius) * (kx * ky).sqrt(),
                })
            })
            .collect(),
    }
}

/// 本机全局坐标 → 所在屏幕与视平面毫米。不在布局里任何一块本机屏幕上时为 `None`。
fn to_plane(screens: &[LocalScreen], p: Point) -> Option<(String, Point)> {
    let s = screens.iter().find(|s| s.local.contains(p))?;
    Some((
        s.key.clone(),
        Point::new(
            s.plane.x + (p.x - s.local.x) * s.plane.w / s.local.w,
            s.plane.y + (p.y - s.local.y) * s.plane.h / s.local.h,
        ),
    ))
}

/// Apply the shared lifetime to the aim-game trail renderer.
pub(crate) fn set_follow_trail_duration(node: &Node, secs: f64) {
    if let Some(follow) = lock(&node.inner.aim.state).follow.as_ref() {
        follow.trail.set_trail_duration(secs);
    }
}

pub(crate) fn set_follow_trail_color(node: &Node, rgb: [u8; 3]) {
    if let Some(follow) = lock(&node.inner.aim.state).follow.as_ref() {
        follow.trail.set_trail_color(rgb);
    }
}

/// 接上暂停时用的拖尾（平台实现在启动时接上）。
pub(crate) fn set_follow_trail(node: &Node, trail: Option<CursorTrail>) {
    lock(&node.inner.aim.state).follow = trail.map(|trail| Follow {
        trail,
        generation: Arc::new(AtomicU64::new(0)),
    });
}

/// 换一种拖尾常驻方式（`None`：结束，恢复原来的规则与开关）。
fn set_trail(node: &Node, follow: Option<&Follow>, from: Option<TrailMode>, to: Option<TrailMode>) {
    if from == to {
        return;
    }
    if (from == Some(TrailMode::Pinned) || to == Some(TrailMode::Pinned))
        && let Ok(input) = node.input_thread()
    {
        input.send(crate::threads::input::InputCommand::TrailPinned(
            to == Some(TrailMode::Pinned),
        ));
    }
    let Some(f) = follow else { return };
    if from == Some(TrailMode::Follow) {
        // 跟随线程退出；拖尾按原来的方式淡出。
        f.generation.fetch_add(1, Ordering::AcqRel);
        f.trail.set_active(false);
    }
    if to == Some(TrailMode::Follow) {
        let generation = f.generation.fetch_add(1, Ordering::AcqRel) + 1;
        f.trail.set_displays(local_displays(node));
        f.trail.set_active(true);
        let input = node.input_backend().clone();
        let f = f.clone();
        let spawned = std::thread::Builder::new()
            .name("oink-aim-trail".into())
            .spawn(move || {
                while f.generation.load(Ordering::Acquire) == generation {
                    if let Ok(p) = input.cursor_position() {
                        f.trail.pointer(p);
                    }
                    std::thread::sleep(FOLLOW_INTERVAL);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(%e, "cannot start the aim trail thread");
        }
    }
}

/// 拖尾常驻：暂停时按本机光标位置画，否则由路由驱动。暂停状态变了就跟着换。
fn sync_trail(node: &Node, follow: Option<&Follow>, session: &mut Session) {
    let want = if paused(node) {
        TrailMode::Follow
    } else {
        TrailMode::Pinned
    };
    set_trail(node, follow, session.trail, Some(want));
    session.trail = Some(want);
}

/// 把状态交给本机叠加层。
fn show(node: &Node, surface: Option<&Arc<dyn AimSurface>>, state: &AimState) {
    if let Some(surface) = surface {
        let (_, screens) = local_screens(node);
        surface.update(view_of(state, node.id(), &screens));
    }
}

/// 权威：把当前状态发给已加入的设备并交给本机叠加层。
fn publish(node: &Node, st: &mut State) {
    let me = node.id();
    let surface = st.surface.clone();
    let follow = st.follow.clone();
    let Some(session) = st.session.as_mut() else { return };
    sync_trail(node, follow.as_ref(), session);
    let now = now(node);
    let Role::Host(h) = &mut session.role else { return };
    if paused(node) != h.native_input {
        h.mode_changed = true;
    }
    // 设备加入 / 离开、显示器或布局变化后，标靶只放在仍然可用的屏幕上。
    let online = online_peers(node);
    h.joined.retain(|peer| online.contains(peer));
    h.invited.retain(|peer| online.contains(peer));
    let mut devices = h.joined.clone();
    devices.insert(me);
    h.game.set_regions(regions_now(node, &devices, h.game.config().radius));
    h.seq = h.seq.checked_add(1).expect("game state counter exhausted");
    // Heartbeats keep real time even when a fixture controls game-rule time.
    h.sent_at = node.now();
    let state = h.game.state(session.id, h.seq, now, h.native_input);
    // Invitation and snapshot are normal periodic state, never reconnect work.
    for p in online_peers(node) {
        if p != me && !h.joined.contains(&p) && !h.declined.contains(&p) {
            h.invited.insert(p);
        }
    }
    for p in &h.invited {
        send(
            node,
            *p,
            AimMsg::Open {
                event: event(node, session.id),
            },
        );
    }
    for p in h.joined.iter().chain(&h.invited) {
        send(node, *p, AimMsg::State(state.clone()));
    }
    show(node, surface.as_ref(), &state);
    session.last = Some((state, now));
}

async fn tick_loop(weak: Weak<Inner>, id: GameInstance) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        if !host_tick(&Node { inner }, id) {
            return;
        }
    }
}

/// 权威的定时推进。返回这一局是否还在。
fn host_tick(node: &Node, id: GameInstance) -> bool {
    let (alive, changed) = {
        let mut st = lock(&node.inner.aim.state);
        let now = now(node);
        let Some(Session {
            id: current,
            role: Role::Host(h),
            ..
        }) = st.session.as_mut()
        else {
            return false;
        };
        if *current != id {
            return false;
        }
        let changed = h.game.tick(now);
        let finished = h.game.finished_at();
        if finished.is_some_and(|at| now >= at + RESULT_LINGER) {
            end_session(node, &mut st, true, "result shown");
            (false, true)
        } else {
            if finished.is_some() && !h.recorded {
                h.recorded = true;
                let result = result_of(h, now);
                push_result(&mut st, result);
            }
            let due = match st.session.as_ref() {
                Some(Session {
                    role: Role::Host(h), ..
                }) => node.now().saturating_sub(h.sent_at) >= HEARTBEAT,
                _ => false,
            };
            if changed || due {
                publish(node, &mut st);
            }
            (true, changed || due)
        }
    };
    if changed {
        emit(node);
    }
    alive
}

fn result_of(h: &Host, now: Micros) -> api::AimResult {
    let s = h.game.summary(now);
    api::AimResult {
        native_input: h.native_input,
        mode_changed: h.mode_changed,
        score: s.score,
        clicks: s.clicks,
        accuracy: s.accuracy(),
        hits_per_sec: s.hits_per_sec(),
        avg_hit_interval_ms: s.avg_hit_interval_ms(),
        played_ms: (s.played / 1000) as u32,
        completed: s.completed,
        devices: 1 + h.joined.len() as u32,
        ended_at_ms: crate::layout_sync::now_ms() as i64,
    }
}

fn push_result(st: &mut State, result: api::AimResult) {
    tracing::info!(
        score = result.score,
        clicks = result.clicks,
        played_ms = result.played_ms,
        completed = result.completed,
        native_input = result.native_input,
        devices = result.devices,
        "aim game result"
    );
    st.results.push_front(result);
    st.results.truncate(MAX_RESULTS);
}

fn repeat_close(node: &Node, id: GameInstance, peers: Vec<DeviceId>) {
    let weak = Arc::downgrade(&node.inner);
    node.inner.rt.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_micros(HEARTBEAT));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let until = tokio::time::Instant::now() + STALE_AFTER;
        while tokio::time::Instant::now() < until {
            interval.tick().await;
            let Some(inner) = weak.upgrade() else { return };
            let node = Node { inner };
            if lock(&node.inner.aim.state).closed.get(&id.authority_boot.device_id) != Some(&id) {
                return;
            }
            for peer in &peers {
                send(
                    &node,
                    *peer,
                    AimMsg::Close {
                        event: event(&node, id),
                    },
                );
            }
        }
    });
}

/// 结束当前这一局：收起本机叠加层、恢复拖尾、（`notify`）通知其它设备。返回是否有一局被结束。
fn end_session(node: &Node, st: &mut State, notify: bool, why: &'static str) -> bool {
    let Some(session) = st.session.take() else {
        return false;
    };
    let id = session.id;
    st.closed.insert(id.authority_boot.device_id, id);
    match session.role {
        Role::Host(mut h) => {
            let now = now(node);
            h.game.abort(now);
            // 提前结束的一局：进行过（过了倒计时）才记结果。
            if !h.recorded && h.game.summary(now).played > 0 {
                let result = result_of(&h, now);
                push_result(st, result);
            }
            if notify {
                let peers: Vec<_> = h.invited.union(&h.joined).copied().collect();
                repeat_close(node, id, peers);
            }
        }
        Role::Guest { authority, .. } => {
            if notify {
                send(node, authority, AimMsg::Close { event: event(node, id) });
            }
        }
    }
    if let Some(s) = &st.surface {
        s.close();
    }
    set_trail(node, st.follow.as_ref(), session.trail, None);
    if let Some(t) = st.tick.take() {
        t.abort();
    }
    tracing::info!(game = ?id, why, "aim game ended on this device");
    true
}

/// 权威：一次点击（本机的或参与设备上报的）。
fn host_click(node: &Node, st: &mut State, device: DeviceId, display: &str, pos: Point) {
    let now = now(node);
    let Some(Session {
        role: Role::Host(h), ..
    }) = st.session.as_mut()
    else {
        return;
    };
    let result = h.game.click(device, display, pos, now);
    // （`display` 在 tracing 的宏里是它自己的函数名，换个名字再记。）
    let screen = display;
    tracing::debug!(?device, screen, x = pos.x, y = pos.y, ?result, "aim click");
    match result {
        Click::Hit { .. } | Click::Miss => publish(node, st),
        Click::Dismiss => {
            end_session(node, st, true, "result dismissed");
        }
        Click::Ignored => {}
    }
}

/// 本机叠加层的事件（在它的绘制线程上）。
fn on_surface_event(node: &Node, id: GameInstance, ev: AimEvent) {
    {
        let mut st = lock(&node.inner.aim.state);
        let Some(session) = st.session.as_ref().filter(|s| s.id == id) else {
            // 这一局已经在本机结束了（收起是我们自己要求的）。
            return;
        };
        match ev {
            AimEvent::Click(p) => {
                let (_, screens) = local_screens(node);
                // 不在布局里的屏幕上的点击：没有位置可报，按「没点中」算（屏幕标识为空）。
                let (display, pos) = to_plane(&screens, p).unwrap_or_default();
                match &session.role {
                    Role::Host(_) => host_click(node, &mut st, node.id(), &display, pos),
                    Role::Guest { authority, .. } => {
                        let screen = display.as_str();
                        tracing::debug!(screen, x = pos.x, y = pos.y, "aim click reported to the authority");
                        send(
                            node,
                            *authority,
                            AimMsg::Click {
                                event: event(node, id),
                                display,
                                pos,
                            },
                        )
                    }
                }
            }
            AimEvent::Closed(reason) => {
                // 叠加层自己收起了（Esc、太久没有状态、超时、窗口创建失败）：这一局在本机结束，
                // 并告诉权威 / 其它设备。
                let why = match reason {
                    CloseReason::Escape => "escape",
                    CloseReason::Stale => "no state from the authority",
                    CloseReason::Lifetime => "lifetime",
                    CloseReason::Failed => "overlay failed",
                    CloseReason::Requested => "closed",
                };
                end_session(node, &mut st, true, why);
            }
        }
    }
    emit(node);
}

/// 收到一条小游戏消息。
fn on_message_from(node: &Node, from: DeviceId, sender: Boot, msg: AimMsg, current: impl Fn() -> bool) {
    let id = msg.game();
    let changed = {
        let mut st = lock(&node.inner.aim.state);
        if !current() {
            return;
        }
        match msg {
            AimMsg::Open { .. } => guest_open(node, &mut st, from, id),
            AimMsg::Joined { event: incoming_event } => match st.session.as_mut() {
                Some(Session {
                    id: current,
                    role: Role::Host(h),
                    ..
                }) if *current == id && (h.invited.contains(&from) || h.joined.contains(&from)) => {
                    // 已经加入的设备再次应答（周期邀请的重复消息）：把当前状态整份再发一次。
                    h.invited.remove(&from);
                    h.joined.insert(from);
                    if h.processed
                        .get(&from)
                        .is_none_or(|(boot, _)| *boot != incoming_event.sender_boot)
                    {
                        h.processed
                            .insert(from, (incoming_event.sender_boot, incoming_event.event_seq));
                    }
                    publish(node, &mut st);
                    true
                }
                _ => {
                    // 这一局已经不在了（或不是我们邀请的）：让它收起。
                    send(node, from, AimMsg::Close { event: event(node, id) });
                    false
                }
            },
            AimMsg::Declined { .. } => {
                if let Some(Session {
                    id: current,
                    role: Role::Host(h),
                    ..
                }) = st.session.as_mut()
                    && *current == id
                {
                    h.invited.remove(&from);
                    h.declined.insert(from);
                }
                false
            }
            AimMsg::State(state) => {
                if st.session.as_ref().is_none_or(|session| session.id != id) {
                    guest_open(node, &mut st, from, id);
                }
                let follow = st.follow.clone();
                match st.session.as_mut() {
                    Some(session) if session.id == id => {
                        let fresh = match &mut session.role {
                            Role::Guest { authority, seq } if *authority == from && state.seq > *seq => {
                                *seq = state.seq;
                                true
                            }
                            _ => false,
                        };
                        if fresh {
                            sync_trail(node, follow.as_ref(), session);
                            let changed = session.last.as_ref().is_none_or(|(l, _)| {
                                (l.phase, l.score, l.clicks) != (state.phase, state.score, state.clicks)
                            });
                            session.last = Some((state.clone(), now(node)));
                            let surface = st.surface.clone();
                            show(node, surface.as_ref(), &state);
                            changed
                        } else {
                            false
                        }
                    }
                    _ => false,
                }
            }
            AimMsg::Click { event, display, pos } => {
                let ours = match st.session.as_mut() {
                    Some(Session {
                        id: current,
                        role: Role::Host(h),
                        ..
                    }) if *current == id && h.joined.contains(&from) => accept_event(&mut h.processed, event),
                    _ => false,
                };
                if ours {
                    host_click(node, &mut st, from, &display, pos);
                }
                ours
            }
            AimMsg::Close { .. } => match st.session.as_ref() {
                // 参与设备上按了 Esc（或它的叠加层自己收起了）：整局结束。
                Some(Session {
                    id: current,
                    role: Role::Host(h),
                    ..
                }) if *current == id && (h.joined.contains(&from) || h.invited.contains(&from)) => {
                    end_session(node, &mut st, true, "closed on a participant")
                }
                Some(Session {
                    id: current,
                    role: Role::Guest { authority, .. },
                    ..
                }) if *current == id && *authority == from => {
                    end_session(node, &mut st, false, "closed by the authority")
                }
                _ => {
                    if id.authority_boot == sender
                        && st
                            .closed
                            .get(&id.authority_boot.device_id)
                            .is_none_or(|closed| !obsolete_game(*closed, id))
                    {
                        st.closed.insert(id.authority_boot.device_id, id);
                    }
                    false
                }
            },
        }
    };
    if changed {
        emit(node);
    }
}

/// 另一台设备发起了一局：盖上本机的叠加层并应答。返回本机状态是否变了。
fn guest_open(node: &Node, st: &mut State, from: DeviceId, id: GameInstance) -> bool {
    if st
        .closed
        .get(&id.authority_boot.device_id)
        .is_some_and(|closed| obsolete_game(*closed, id))
    {
        send(node, from, AimMsg::Declined { event: event(node, id) });
        return false;
    }
    if st
        .session
        .as_ref()
        .is_some_and(|session| session.id != id && obsolete_game(session.id, id))
    {
        return false;
    }
    if let Some(Session {
        id: current,
        role: Role::Guest { authority, .. },
        ..
    }) = &st.session
        && *authority == from
    {
        if *current == id {
            // 同一局的周期重复邀请：叠加层已经盖着，再应答一次即可。
            send(node, from, AimMsg::Joined { event: event(node, id) });
            return false;
        }
        // 同一台设备的上一局还留着（它没来得及通知）：先收掉。
        end_session(node, st, false, "replaced by a new game");
    }
    let surface = match (&st.session, &st.surface) {
        (None, Some(s)) => s.clone(),
        // 本机正在自己发起的（或第三台发起的）一局里，或平台没有叠加层：不参加。
        _ => {
            send(node, from, AimMsg::Declined { event: event(node, id) });
            return false;
        }
    };
    if !open_surface(node, &surface, id) {
        send(node, from, AimMsg::Declined { event: event(node, id) });
        return false;
    }
    let mut session = Session {
        id,
        role: Role::Guest {
            authority: from,
            seq: 0,
        },
        trail: None,
        last: None,
    };
    sync_trail(node, st.follow.as_ref(), &mut session);
    st.session = Some(session);
    send(node, from, AimMsg::Joined { event: event(node, id) });
    tracing::info!(game = ?id, ?from, "aim game opened by another device");
    true
}

fn link_lost(node: &Node, peer: DeviceId) {
    let mut state = lock(&node.inner.aim.state);
    let guest = state.session.as_ref().is_some_and(|session| {
        matches!(session.role,
        Role::Guest { authority, .. } if authority == peer)
    });
    if guest {
        end_session(node, &mut state, false, "authority disconnected");
    } else if let Some(Session {
        role: Role::Host(host), ..
    }) = state.session.as_mut()
    {
        host.joined.remove(&peer);
        host.invited.remove(&peer);
        host.declined.remove(&peer);
        host.processed.remove(&peer);
        // publish replaces the game's regions and moves any targets off the lost peer.
        publish(node, &mut state);
    }
    drop(state);
    emit(node);
}

pub(crate) fn start_link_runtime(node: Node, mut port: ModulePort) -> anyhow::Result<JoinHandle<()>> {
    let links = node.links()?;
    let group = node.link_config()?;
    let mut changes = group.subscribe();
    let mut stop_rx = node.stop_receiver();
    let mut streams = AimStreams::new(links.clone(), group.clone());
    let wake = streams.wake();
    let (tx, mut commands) = tokio::sync::mpsc::channel::<(DeviceId, AimMsg)>(64);
    *lock(&node.inner.aim.outgoing) = Some(tx);
    Ok(tokio::spawn(async move {
        loop {
            let deadline = streams
                .deadline()
                .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(3600));
            tokio::select! {
                _ = stop_rx.changed() => break,
                event = port.next() => match event {
                    Some(ModuleEvent::LinkReady { link, peer }) => streams.link_ready(link, peer),
                    Some(ModuleEvent::LinkLost { link, peer, reason }) => {
                        streams.link_lost(link, peer, reason);
                        link_lost(&node, peer);
                    }
                    Some(ModuleEvent::Inbound(stream)) => streams.inbound(stream),
                    Some(ModuleEvent::InboundLow(op)) => streams.inbound_low(*op),
                    Some(ModuleEvent::StreamClosed { stream, reason }) => streams.stream_closed(stream, reason),
                    Some(ModuleEvent::PathChanged { .. }) | Some(ModuleEvent::ProtocolViolation { .. }) => {},
                    None => break,
                },
                change = changes.next() => match change {
                    Some(crate::config_module::ConfigChange::Facts { device, capabilities_changed }) => streams.facts_changed(device, capabilities_changed),
                    Some(_) => {},
                    None => break,
                },
                command = commands.recv() => if let Some((peer, message)) = command {
                    let Ok(message) = wire::encode(&message) else { continue; };
                    if matches!(message, oink_protocol::v1::Message::AimState(_)) {
                        streams.set_opening(peer, vec![message.clone()]);
                    }
                    let result = streams.send(peer, &message, std::time::Instant::now());
                    let result = match result { Ok(()) => SendResult::Accepted, Err(PostError::Full) => SendResult::Full, Err(_) => SendResult::Closed };
                    lock(&node.inner.aim.send_diagnostics).record(peer, result);
                },
                _ = wake.notified() => {},
                _ = tokio::time::sleep_until(deadline.into()) => {},
            }
            for (peer, message) in streams.poll(std::time::Instant::now()) {
                if let Some(link) = links.links().links.iter().find(|link| link.peer == peer) {
                    let sender = link.peer_boot;
                    let valid = message.valid()
                        && path::message_event(&message).is_none_or(|event| event.sender_boot == sender)
                        && (!matches!(message, AimMsg::Open { .. } | AimMsg::State(_))
                            || message.game().authority_boot == sender);
                    if valid {
                        on_message_from(&node, peer, sender, message, || true);
                    }
                }
            }
            streams.take_closed();
        }
        *lock(&node.inner.aim.outgoing) = None;
        stop(&node);
    }))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    fn plane_rect(x: f64, y: f64, w: f64, h: f64) -> oink_foundation::Rect {
        oink_foundation::Rect::new(x / 686.0, y / 686.0, w / 686.0, h / 686.0)
    }
    use super::*;
    use oink_display::layout::{ScreenId, ScreenPlacement, SizeSource};
    use oink_locator::aim::model::AimTarget;

    const A: DeviceId = DeviceId([1; 32]);
    const B: DeviceId = DeviceId([2; 32]);
    fn test_game(game_counter: u64) -> GameInstance {
        GameInstance {
            authority_boot: Boot {
                device_id: A,
                nonce: [1; 16],
            },
            game_counter,
        }
    }

    #[test]
    fn local_game_queue_refusal_and_network_refusal_are_observable() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let diagnostics = Mutex::new(SendDiagnostics::default());
        let peer = DeviceId([2; 32]);
        let message = AimMsg::Open {
            event: GameEvent {
                game_instance: test_game(1),
                sender_boot: test_game(1).authority_boot,
                event_seq: 1,
            },
        };
        enqueue(&tx, peer, message.clone(), &diagnostics);
        enqueue(&tx, peer, message.clone(), &diagnostics);
        assert_eq!(lock(&diagnostics).refused, 1);
        assert_eq!(lock(&diagnostics).last_refusal.as_deref(), Some("Full"));
        assert_eq!(rx.try_recv().unwrap(), (peer, message));
        assert!(rx.try_recv().is_err());
        lock(&diagnostics).record(peer, SendResult::Closed);
        let state = serde_json::to_value(&*lock(&diagnostics)).unwrap();
        assert_eq!(state["refused"], 2);
        assert_eq!(state["last_refusal"], "Closed");
        lock(&diagnostics).record(peer, SendResult::Accepted);
        assert_eq!(lock(&diagnostics).refused, 2);
    }

    #[test]
    fn duplicate_click_and_old_game_are_rejected() {
        let mut processed = BTreeMap::new();
        let mut event = GameEvent {
            game_instance: test_game(2),
            sender_boot: Boot {
                device_id: B,
                nonce: [2; 16],
            },
            event_seq: 1,
        };
        assert!(accept_event(&mut processed, event));
        assert!(!accept_event(&mut processed, event));
        event.event_seq = 3;
        assert!(accept_event(&mut processed, event));
        event.event_seq = 2;
        assert!(!accept_event(&mut processed, event));
        assert!(obsolete_game(test_game(2), test_game(1)));
        assert!(obsolete_game(test_game(2), test_game(2)));
        assert!(!obsolete_game(test_game(2), test_game(3)));
    }

    fn placement(device: DeviceId, key: &str, local: Rect, rect: Rect, builtin: bool) -> ScreenPlacement {
        ScreenPlacement {
            id: ScreenId::new(device, key),
            name: key.into(),
            local,
            rect: plane_rect(rect.x, rect.y, rect.w, rect.h),
            size_source: SizeSource::Manual,
            primary: true,
            builtin,
        }
    }

    /// A：16 寸 Mac 笔记本（内建屏，1728×1117 点 = 345×223 mm）；B：4K 28 寸（3840×2160 像素 = 620×349 mm），
    /// 在 A 右边；B 还有一块没启用的虚拟屏。
    fn layout() -> Layout {
        Layout {
            screens: vec![
                placement(
                    A,
                    "a",
                    Rect::new(0., 0., 1728., 1117.),
                    Rect::new(0., 100., 345., 223.),
                    true,
                ),
                placement(
                    B,
                    "b",
                    Rect::new(0., 0., 3840., 2160.),
                    Rect::new(345., 0., 620., 349.),
                    false,
                ),
                placement(
                    B,
                    "virtual",
                    Rect::new(3840., 0., 1920., 1080.),
                    Rect::new(965., 0., 500., 280.),
                    false,
                ),
            ],
            ..Layout::default()
        }
    }

    fn owners() -> BTreeMap<DeviceId, OwnerLayoutPreferences> {
        BTreeMap::from([(
            B,
            OwnerLayoutPreferences {
                screen_shared: false,
                screens: BTreeMap::from([(
                    "virtual".into(),
                    oink_display::layout::ScreenPreference {
                        enabled: false,
                        gain: 1.0,
                    },
                )]),
                ..Default::default()
            },
        )])
    }

    fn screens_of(device: DeviceId) -> Vec<LocalScreen> {
        layout()
            .screens
            .iter()
            .filter(|s| s.id.device == device)
            .map(|s| LocalScreen {
                key: s.id.display.clone(),
                local: s.local,
                plane: s.rect,
            })
            .collect()
    }

    #[test]
    fn regions_cover_participating_enabled_screens_inset_by_a_radius_and_the_hud() {
        let all = regions(&layout(), &owners(), &BTreeSet::from([A, B]), 7.0 / 686.0);
        assert_eq!(
            all.len(),
            2,
            "screen sharing does not exclude participants, but the disabled virtual screen is left out"
        );
        // 内建屏：顶部让出刘海 + 计分栏（25 mm），四边缩进一个半径。
        assert_eq!((all[0].device, all[0].display.as_str()), (A, "a"));
        let near = |actual: Rect, expected: Rect| {
            for (actual, expected) in [
                (actual.x, expected.x),
                (actual.y, expected.y),
                (actual.w, expected.w),
                (actual.h, expected.h),
            ] {
                assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
            }
        };
        near(
            all[0].rect,
            plane_rect(7., 100. + 25. + 7., 345. - 14., 223. - 25. - 14.),
        );
        // 外接屏：顶部让出 19 mm。
        near(all[1].rect, plane_rect(352., 19. + 7., 620. - 14., 349. - 19. - 14.));
        // 只有本机参加（其它设备没加入）：只有本机的屏幕。
        let only_a = regions(&layout(), &owners(), &BTreeSet::from([A]), 7.0 / 686.0);
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].device, A);
        // 屏幕小到放不下：没有区域，不出现负的宽高。
        let mut tiny = layout();
        tiny.screens[0].rect = plane_rect(0., 0., 40., 30.);
        assert!(regions(&tiny, &owners(), &BTreeSet::from([A]), 7.0 / 686.0).is_empty());
    }

    #[test]
    fn a_target_has_the_same_reference_ratio_size_on_every_screen() {
        let state = AimState {
            game: test_game(1),
            seq: 1,
            phase: AimPhase::Running,
            remaining_ms: 30_000,
            score: 3,
            clicks: 4,
            radius: (7.0 / 686.0) as f32,
            native_input: false,
            targets: vec![
                AimTarget {
                    id: 1,
                    device: A,
                    display: "a".into(),
                    pos: Point::new(172.5 / 686.0, 211.5 / 686.0),
                },
                AimTarget {
                    id: 2,
                    device: B,
                    display: "b".into(),
                    pos: Point::new((345. + 310.) / 686.0, 174.5 / 686.0),
                },
                // 屏幕标识不认识：不显示（不猜）。
                AimTarget {
                    id: 3,
                    device: B,
                    display: "gone".into(),
                    pos: Point::new(400. / 686.0, 100. / 686.0),
                },
            ],
        };
        let a = view_of(&state, A, &screens_of(A));
        assert_eq!(a.targets.len(), 1);
        // A 屏幕的正中间（点）。
        assert!((a.targets[0].center.x - 864.0).abs() < 1e-9 && (a.targets[0].center.y - 558.5).abs() < 1e-9);
        let b = view_of(&state, B, &screens_of(B));
        assert_eq!(b.targets.len(), 1);
        assert!((b.targets[0].center.x - 1920.0).abs() < 1e-9 && (b.targets[0].center.y - 1080.0).abs() < 1e-6);
        // Convert each native radius back to the same reference-plane radius.
        let ratio_a = a.targets[0].radius / (1728.0 / (345.0 / 686.0));
        let ratio_b = b.targets[0].radius / (3840.0 / (620.0 / 686.0));
        assert!(
            (ratio_a - 7.0 / 686.0).abs() < 0.01 / 686.0 && (ratio_b - 7.0 / 686.0).abs() < 0.01 / 686.0,
            "{ratio_a} {ratio_b}"
        );
        assert_eq!((a.score, a.clicks, a.phase), (3, 4, AimPhase::Running));
        assert_eq!(a.remaining, Duration::from_secs(30));
    }

    #[test]
    fn clicks_map_back_to_the_plane_position_the_authority_placed() {
        for (device, key) in [(A, "a"), (B, "b")] {
            let screens = screens_of(device);
            let s = screens.iter().find(|s| s.key == key).unwrap();
            let plane = Point::new(s.plane.x + s.plane.w * 0.3, s.plane.y + s.plane.h * 0.8);
            let state = AimState {
                game: test_game(1),
                seq: 1,
                phase: AimPhase::Running,
                remaining_ms: 1,
                score: 0,
                clicks: 0,
                radius: (7.0 / 686.0) as f32,
                native_input: true,
                targets: vec![AimTarget {
                    id: 9,
                    device,
                    display: key.into(),
                    pos: plane,
                }],
            };
            let local = view_of(&state, device, &screens).targets[0].center;
            let (display, back) = to_plane(&screens, local).unwrap();
            assert_eq!(display, key);
            assert!((back.x - plane.x).abs() < 1e-9 && (back.y - plane.y).abs() < 1e-9);
        }
        // 不在任何一块布局里的本机屏幕上。
        assert_eq!(to_plane(&screens_of(A), Point::new(-10., 5.)), None);
    }
}
