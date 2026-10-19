#![cfg(debug_assertions)]
//! Two real loopback nodes run the aiming game with fake input and overlay
//! backends. This checks game messages and closure paths without native windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oink_agent as test_agent;
mod common;

use oink_agent::api::{AgentApi, AimPhase};
use oink_agent::node::{Node, NodeOptions};
use oink_foundation::display::DisplayInfo;
use oink_foundation::input::{CaptureMode, CapturedEvent, Disposition, FeelProfile, InjectEvent, InputPermissions};
use oink_foundation::{Point, Rect, Size};
use oink_input::{EventHandler, InputBackend, InputError};
use oink_locator::aim::game::Config;
use oink_locator::aim::model::AimPhase as WirePhase;
use oink_locator::aim::{AimEvent, AimEvents, AimSurface, CloseReason, OpenSpec, View};
use oink_test_support::input::FakeInput;

#[derive(Default)]
struct FakeState {
    open: bool,
    events: Option<AimEvents>,
    view: Option<View>,
    opened: u32,
    closed: u32,
    spec: Option<OpenSpec>,
}

/// 假的叠加层：记下最新的状态；测试调用 `click` / `escape` 模拟本机的鼠标与键盘。
#[derive(Default)]
struct FakeSurface {
    state: Mutex<FakeState>,
}

impl AimSurface for FakeSurface {
    fn open(&self, spec: OpenSpec, events: AimEvents) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.open {
            return false;
        }
        s.open = true;
        s.opened += 1;
        s.view = None;
        s.spec = Some(spec);
        s.events = Some(events);
        true
    }

    fn update(&self, view: View) {
        let mut s = self.state.lock().unwrap();
        if s.open {
            s.view = Some(view);
        }
    }

    fn close(&self) {
        let events = {
            let mut s = self.state.lock().unwrap();
            if !s.open {
                return;
            }
            s.open = false;
            s.closed += 1;
            s.events.take()
        };
        // 真实的叠加层在它自己的线程上报告「已收起」。
        if let Some(events) = events {
            std::thread::spawn(move || events(AimEvent::Closed(CloseReason::Requested)));
        }
    }
}

impl FakeSurface {
    fn is_open(&self) -> bool {
        self.state.lock().unwrap().open
    }

    fn view(&self) -> Option<View> {
        self.state.lock().unwrap().view.clone()
    }

    fn click(&self, p: Point) {
        let events = self.state.lock().unwrap().events.clone();
        events.expect("overlay open")(AimEvent::Click(p));
    }

    /// 本机按 Esc：叠加层自己收起并报告。
    fn escape(&self) {
        let events = {
            let mut s = self.state.lock().unwrap();
            assert!(s.open);
            s.open = false;
            s.closed += 1;
            s.events.take()
        };
        events.unwrap()(AimEvent::Closed(CloseReason::Escape));
    }
}

fn display(key: &str) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds: Rect::new(0.0, 0.0, 1600.0, 1000.0),
        pixel_width: 1600,
        pixel_height: 1000,
        reference_extent: Some(Size::new(400.0 / 686.0, 250.0 / 686.0)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}

struct Dev {
    node: Node,
    surface: Arc<FakeSurface>,
}

#[derive(Default)]
struct InputTurnGate {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    ready: std::sync::Condvar,
}
impl InputTurnGate {
    fn hold(self: &Arc<Self>) -> HeldInputTurn {
        *self.released.lock().unwrap() = false;
        self.armed.store(true, Ordering::Release);
        HeldInputTurn(self.clone())
    }
    fn before_poll(&self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.entered.notify_one();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.ready.wait(released).unwrap();
            }
        }
    }
}
struct HeldInputTurn(Arc<InputTurnGate>);
impl Drop for HeldInputTurn {
    fn drop(&mut self) {
        // Assertions must not strand capture-loop shutdown on the test gate.
        *self.0.released.lock().unwrap() = true;
        self.0.ready.notify_one();
    }
}
struct PausedLoop {
    owner: Box<dyn oink_input::capture_loop::CaptureLoop>,
    gate: Arc<InputTurnGate>,
}
impl oink_input::capture_loop::CaptureLoop for PausedLoop {
    fn set_wake(&mut self, wake: oink_input::capture_loop::Wake) {
        self.owner.set_wake(wake);
    }
    fn event(&mut self, event: &CapturedEvent) -> Disposition {
        self.owner.event(event)
    }
    fn poll(&mut self, _: Instant) -> Option<Instant> {
        self.gate.before_poll();
        self.owner.poll(Instant::now())
    }
    fn stopped(&self) -> bool {
        self.owner.stopped()
    }
    fn capture_requested(&self) -> bool {
        self.owner.capture_requested()
    }
}
struct PausedInput {
    fake: FakeInput,
    gate: Arc<InputTurnGate>,
}
impl InputBackend for PausedInput {
    fn start_loop(&self, owner: Box<dyn oink_input::capture_loop::CaptureLoop>) -> Result<(), InputError> {
        self.fake.start_loop(Box::new(PausedLoop {
            owner,
            gate: self.gate.clone(),
        }))
    }
    fn start(&self, handler: EventHandler) -> Result<(), InputError> {
        self.fake.start(handler)
    }
    fn stop(&self) {
        self.fake.stop();
    }
    fn capture_active(&self) -> Option<bool> {
        self.fake.capture_active()
    }
    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        self.fake.set_mode(mode)
    }
    fn restore_local(&self, at: Point) -> Result<(), InputError> {
        self.fake.restore_local(at)
    }
    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        self.fake.inject(event)
    }
    fn cursor_position(&self) -> Result<Point, InputError> {
        self.fake.cursor_position()
    }
    fn feel_profile(&self) -> FeelProfile {
        self.fake.feel_profile()
    }
    fn permissions(&self) -> InputPermissions {
        self.fake.permissions()
    }
    fn display_power_observation(&self) -> Result<Box<dyn oink_input::display_power::Observation>, InputError> {
        self.fake.display_power_observation()
    }
    fn declare_user_activity(&self) -> Result<bool, InputError> {
        self.fake.declare_user_activity()
    }
    fn reset_user_idle(&self) -> Result<bool, InputError> {
        self.fake.reset_user_idle()
    }
    fn diagnostic_key_details(&self) -> bool {
        self.fake.diagnostic_key_details()
    }
    fn held_session_keys(&self) -> Vec<oink_input::KeyCode> {
        self.fake.held_session_keys()
    }
}

async fn start(name: &str, port: u16) -> Dev {
    let input = FakeInput::new();
    input.set_cursor(Point::new(800.0, 500.0));
    start_with_input(name, port, Arc::new(input)).await
}
async fn start_with_input(name: &str, port: u16, input: Arc<dyn InputBackend>) -> Dev {
    let dir = std::env::temp_dir().join(format!("oink-aim-game-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let node = Node::start_with_fake_platform(NodeOptions {
        net: oink_test_support::network_config(dir, name, port),
        input,
        displays: vec![display(name)],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap();
    let surface = Arc::new(FakeSurface::default());
    node.set_aim_surface(Some(surface.clone()));
    // 一局缩短到 2.5 秒、倒计时 0.3 秒。
    node.set_aim_config(Config {
        countdown: 300_000,
        duration: 2_500_000,
        ..Config::default()
    });
    Dev { node, surface }
}

async fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn api<T: Send + 'static>(n: &Node, f: impl FnOnce(&Node) -> T + Send + 'static) -> T {
    let n = n.clone();
    tokio::task::spawn_blocking(move || f(&n)).await.unwrap()
}

/// 两台设备的叠加层上此刻显示的标靶总数。
fn targets(a: &Dev, b: &Dev) -> (usize, usize) {
    let n = |d: &Dev| d.surface.view().map_or(0, |v| v.targets.len());
    (n(a), n(b))
}

fn running(d: &Dev) -> bool {
    d.surface.view().is_some_and(|v| v.phase == WirePhase::Running)
}

#[test]
fn game_runs_across_two_devices_and_always_closes() {
    common::watchdog::run(
        module_path!(),
        "game_runs_across_two_devices_and_always_closes",
        Some(4),
        game_runs_across_two_devices_and_always_closes_scenario(),
    );
}

#[test]
fn authority_disconnect_restores_trail_after_the_delayed_input_turn() {
    common::watchdog::run(
        module_path!(),
        "authority_disconnect_restores_trail_after_the_delayed_input_turn",
        Some(4),
        authority_disconnect_restores_trail_after_the_delayed_input_turn_scenario(),
    );
}

async fn authority_disconnect_restores_trail_after_the_delayed_input_turn_scenario() {
    let a = start("trail-host", 0).await;
    let gate = Arc::new(InputTurnGate::default());
    let fake = FakeInput::new();
    fake.set_cursor(Point::new(800.0, 500.0));
    let b = start_with_input(
        "trail-guest",
        0,
        Arc::new(PausedInput {
            fake,
            gate: gate.clone(),
        }),
    )
    .await;
    common::pair(&a.node, &b.node).await;
    wait_for("shared layout before the game", 10.0, || {
        a.node.layout().0 == b.node.layout().0 && a.node.layout().0.screens.len() == 2
    })
    .await;
    api(&a.node, |n| n.aim_start()).await.unwrap();
    wait_for("participant joined with pinned trail", 5.0, || {
        a.node.aim_status().unwrap().devices == 2 && b.surface.is_open() && b.node.input_status().unwrap().trail_pinned
    })
    .await;

    // Delay actual command consumption, without changing admission or the router.
    let held_input = gate.hold();
    b.node.input_thread().unwrap().wake()();
    gate.entered.notified().await;
    // Lose the authority transport before graceful Aim Close can be sent.
    a.node.links().unwrap().stop().await;
    wait_for("Aim closes independently of the held input owner", 5.0, || {
        !b.surface.is_open() && b.node.aim_status().unwrap().phase == AimPhase::Idle
    })
    .await;
    assert!(
        b.node.input_status().unwrap().trail_pinned,
        "input owner has not consumed the queued unpin yet"
    );
    drop(held_input);
    // No manual unpin/resend: the command emitted by link_lost must survive.
    wait_for("queued disconnect unpin is consumed", 5.0, || {
        !b.node.input_status().unwrap().trail_pinned
    })
    .await;
    assert!(!b.surface.is_open());
    assert_eq!(b.node.aim_status().unwrap().phase, AimPhase::Idle);
    a.node.shutdown().await;
    b.node.shutdown().await;
}
async fn game_runs_across_two_devices_and_always_closes_scenario() {
    oink_agent::logs::init();
    let base = 48_200 + (std::process::id() % 100) as u16 * 4;
    let a = start("alpha", base).await;
    let b = start("beta", base + 2).await;
    common::pair(&a.node, &b.node).await;
    wait_for("shared layout", 10.0, || {
        a.node.layout().0 == b.node.layout().0 && a.node.layout().0.screens.len() == 2
    })
    .await;
    let (an, bn) = (a.node.clone(), b.node.clone());

    // 空闲：可以开始，没有结果。
    let idle = an.aim_status().unwrap();
    assert!(idle.available && idle.phase == AimPhase::Idle && !idle.native_input && idle.results.is_empty());
    assert_eq!((idle.target_diameter, idle.duration_ms), (14.0 / 686.0, 2_500));

    // ---- 一局打满：A 发起，两台都盖上叠加层；倒计时没有标靶；进行中全场 3 个。
    let st = api(&an, |n| n.aim_start()).await.unwrap();
    assert!(st.host && st.phase == AimPhase::Countdown);
    assert!(a.surface.is_open());
    wait_for("overlay on B", 5.0, || b.surface.is_open()).await;
    wait_for("B joined", 5.0, || an.aim_status().unwrap().devices == 2).await;
    assert!(api(&an, |n| n.aim_start()).await.is_err(), "one game at a time");
    assert!(api(&bn, |n| n.aim_start()).await.is_err(), "B is in A's game");
    assert_eq!(targets(&a, &b), (0, 0));
    // 两台都常驻拖尾（共享开着：由路由驱动）。
    assert!(an.input_status().unwrap().trail_pinned);
    wait_for("trail pinned on B", 2.0, || bn.input_status().unwrap().trail_pinned).await;
    wait_for("running on both", 5.0, || running(&a) && running(&b)).await;
    wait_for("three targets in total", 2.0, || {
        let (x, y) = targets(&a, &b);
        x + y == 3
    })
    .await;
    // 叠加层的看护时间。
    let spec = a.surface.state.lock().unwrap().spec.clone().unwrap();
    assert_eq!(
        (spec.stale_after, spec.lifetime),
        (Duration::from_secs(3), Duration::from_secs(90))
    );
    assert_eq!(spec.displays.len(), 1);

    // 点中标靶：在哪台设备上点都由 A 计分，立刻补上一个新的。
    let mut hits = 0;
    for _ in 0..6 {
        let (dev, other) = if b.surface.view().is_some_and(|v| !v.targets.is_empty()) {
            (&b, &a)
        } else {
            (&a, &b)
        };
        let t = dev.surface.view().unwrap().targets[0];
        // 所有屏幕上是同一个物理尺寸：7 mm × 4 单位/mm。
        assert!((t.radius - 28.0).abs() < 1e-6, "{}", t.radius);
        dev.surface.click(Point::new(t.center.x + t.radius * 0.9, t.center.y));
        hits += 1;
        let (an2, id) = (an.clone(), t.id);
        let (ds, os) = (dev.surface.clone(), other.surface.clone());
        wait_for("hit scored and target replaced", 2.0, || {
            an2.aim_status().unwrap().score == hits
                && ds.view().is_some_and(|v| v.targets.iter().all(|x| x.id != id))
                && ds.view().map_or(0, |v| v.targets.len()) + os.view().map_or(0, |v| v.targets.len()) == 3
        })
        .await;
    }
    // 没点中：只算一次点击。标靶之外的位置（屏幕角上，标靶不会放在离边缘一个半径以内）。
    b.surface.click(Point::new(2.0, 998.0));
    wait_for("miss counted", 2.0, || an.aim_status().unwrap().clicks == hits + 1).await;
    assert_eq!(an.aim_status().unwrap().score, hits);
    // B 的界面也看得到这一局（不是发起方）。
    wait_for("B sees the score", 2.0, || bn.aim_status().unwrap().score == hits).await;
    let on_b = bn.aim_status().unwrap();
    assert!(!on_b.host && on_b.phase == AimPhase::Running && on_b.results.is_empty());

    // 时间到：结果只记在 A；叠加层留着显示结果。
    wait_for("finished", 5.0, || an.aim_status().unwrap().phase == AimPhase::Finished).await;
    let done = an.aim_status().unwrap();
    assert_eq!(done.results.len(), 1);
    let r = &done.results[0];
    assert!(r.completed && !r.native_input && !r.mode_changed);
    assert_eq!((r.score, r.clicks, r.devices, r.played_ms), (hits, hits + 1, 2, 2_500));
    assert!((r.accuracy.unwrap() - f64::from(hits) / f64::from(hits + 1)).abs() < 1e-9);
    assert!(a.surface.is_open() && b.surface.is_open());
    wait_for("B shows the result", 2.0, || {
        b.surface
            .view()
            .is_some_and(|v| v.phase == WirePhase::Finished && v.targets.is_empty())
    })
    .await;
    // 结束后马上的点击不关（保护时间），之后在任一台上点一下就全组收起。
    b.surface.click(Point::new(800.0, 500.0));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(a.surface.is_open() && b.surface.is_open());
    tokio::time::sleep(Duration::from_millis(600)).await;
    b.surface.click(Point::new(800.0, 500.0));
    wait_for("both overlays closed", 3.0, || {
        !a.surface.is_open() && !b.surface.is_open()
    })
    .await;
    wait_for("idle again", 2.0, || {
        an.aim_status().unwrap().phase == AimPhase::Idle && bn.aim_status().unwrap().phase == AimPhase::Idle
    })
    .await;
    assert!(!an.input_status().unwrap().trail_pinned, "trail restored on A");
    wait_for("trail restored on B", 2.0, || !bn.input_status().unwrap().trail_pinned).await;
    assert_eq!(an.aim_status().unwrap().results.len(), 1, "not recorded twice");

    // Each participant pauses locally; the explicitly invited game continues
    // on both devices and records native input. End this longer game with Esc.
    for n in [&an, &bn] {
        n.set_aim_config(Config {
            countdown: 300_000,
            duration: 30_000_000,
            ..Config::default()
        });
    }
    api(&an, |n| {
        n.set_local_paused("00000000-0000-4000-8000-000000000001".into(), true)
    })
    .await
    .unwrap();
    wait_for("A applies its local pause", 5.0, || an.fully_paused()).await;
    assert!(!bn.fully_paused(), "ordinary pause is local to its owner");
    api(&bn, |n| {
        n.set_local_paused("00000000-0000-4000-8000-000000000002".into(), true)
    })
    .await
    .unwrap();
    wait_for(
        "both participants pause while the game route remains ready",
        5.0,
        || {
            bn.fully_paused()
                && an.aim_status().unwrap().native_input
                && bn.aim_status().unwrap().native_input
                && common::linked(&an, bn.id())
                && common::linked(&bn, an.id())
        },
    )
    .await;
    assert!(an.aim_status().unwrap().native_input);
    api(&an, |n| n.aim_start()).await.unwrap();
    wait_for("overlay on B while paused", 5.0, || b.surface.is_open()).await;
    wait_for("running on both while paused", 5.0, || running(&a) && running(&b)).await;
    assert!(b.surface.view().unwrap().native_input);
    // 暂停时拖尾不由路由驱动（每台按本机光标位置画自己的）。
    assert!(!an.input_status().unwrap().trail_pinned);
    assert!(!bn.input_status().unwrap().trail_pinned);
    // 每台设备点自己屏幕上的标靶：B 上的点击照样计分。
    wait_for("a target on B", 3.0, || {
        // 标靶随机：B 上暂时没有的话，点掉 A 上的一个让它换位置。
        if b.surface.view().is_some_and(|v| !v.targets.is_empty()) {
            return true;
        }
        if let Some(t) = a.surface.view().and_then(|v| v.targets.first().copied()) {
            a.surface.click(t.center);
        }
        false
    })
    .await;
    let before = an.aim_status().unwrap().score;
    let t = b.surface.view().unwrap().targets[0];
    b.surface.click(t.center);
    wait_for("B's native click scored on A", 2.0, || {
        an.aim_status().unwrap().score == before + 1
    })
    .await;

    // ---- 进行中恢复共享：新业务会话不改变本局身份，点击照样计分；拖尾换回由路由驱动。
    let opened_before = (
        a.surface.state.lock().unwrap().opened,
        b.surface.state.lock().unwrap().opened,
    );
    api(&an, |n| {
        n.set_local_paused("00000000-0000-4000-8000-000000000003".into(), false)
    })
    .await
    .unwrap();
    api(&bn, |n| {
        n.set_local_paused("00000000-0000-4000-8000-000000000004".into(), false)
    })
    .await
    .unwrap();
    wait_for("both participants resume locally", 5.0, || {
        !an.fully_paused() && !bn.fully_paused()
    })
    .await;
    wait_for("B remains in the same game after business resumes", 8.0, || {
        an.aim_status().unwrap().devices == 2 && an.input_status().unwrap().trail_pinned
    })
    .await;
    assert!(a.surface.is_open() && b.surface.is_open(), "neither overlay closed");
    assert_eq!(
        (
            a.surface.state.lock().unwrap().opened,
            b.surface.state.lock().unwrap().opened
        ),
        opened_before,
        "business resume must not reopen either game surface"
    );
    wait_for("trail pinned on B again", 3.0, || {
        bn.input_status().unwrap().trail_pinned
    })
    .await;
    wait_for("a target on B after the resume", 3.0, || {
        if b.surface.view().is_some_and(|v| !v.targets.is_empty()) {
            return true;
        }
        if let Some(t) = a.surface.view().and_then(|v| v.targets.first().copied()) {
            a.surface.click(t.center);
        }
        false
    })
    .await;
    let before = an.aim_status().unwrap().score;
    let t = b.surface.view().unwrap().targets[0];
    b.surface.click(t.center);
    wait_for("B's click after the resume scored on A", 3.0, || {
        an.aim_status().unwrap().score == before + 1
    })
    .await;

    // ---- B 上按 Esc：B 的叠加层自己收起，A 结束整局，记下提前结束的结果。
    b.surface.escape();
    wait_for("Esc on B ends the whole game", 3.0, || {
        !a.surface.is_open() && an.aim_status().unwrap().phase == AimPhase::Idle
    })
    .await;
    let results = an.aim_status().unwrap().results;
    assert_eq!(results.len(), 2);
    assert!(results[0].native_input && !results[0].completed && results[0].played_ms < 30_000);
    assert!(results[0].mode_changed, "the pause state changed during the game");
    assert!(results[0].score > before);

    // ---- 发起方的界面点「结束」：全组收起。倒计时里就结束的一局不记结果。
    api(&bn, |n| n.aim_start()).await.unwrap();
    wait_for("overlay on A (B hosts)", 5.0, || a.surface.is_open()).await;
    assert!(!an.aim_status().unwrap().host);
    api(&bn, |n| n.aim_stop()).await.unwrap();
    wait_for("stop closes both", 3.0, || !a.surface.is_open() && !b.surface.is_open()).await;
    assert!(bn.aim_status().unwrap().results.is_empty());
    // 参与设备的界面点「结束」：请发起方结束。
    api(&bn, |n| n.aim_start()).await.unwrap();
    wait_for("overlay on A again", 5.0, || a.surface.is_open()).await;
    wait_for("A joined", 5.0, || bn.aim_status().unwrap().devices == 2).await;
    api(&an, |n| n.aim_stop()).await.unwrap();
    wait_for("participant stop closes both", 3.0, || {
        !a.surface.is_open() && !b.surface.is_open() && bn.aim_status().unwrap().phase == AimPhase::Idle
    })
    .await;

    // ---- 发起方退出：先让参与设备收起，再收起自己的。
    api(&an, |n| n.aim_start()).await.unwrap();
    wait_for("overlay on B before A goes away", 5.0, || b.surface.is_open()).await;
    wait_for("B joined before A goes away", 5.0, || {
        an.aim_status().unwrap().devices == 2
    })
    .await;
    a.node.shutdown().await;
    // Aim and the input owner publish independently: closing the game only
    // queues TrailPinned(false). Completion includes the input owner's snapshot.
    wait_for("B closes and restores its trail when the authority exits", 5.0, || {
        !b.surface.is_open()
            && bn.aim_status().unwrap().phase == AimPhase::Idle
            && !bn.input_status().unwrap().trail_pinned
    })
    .await;
    assert!(!a.surface.is_open(), "the authority closed its own overlay on shutdown");
    assert!(!bn.input_status().unwrap().trail_pinned);
    b.node.shutdown().await;
}

#[test]
fn a_single_device_plays_alone_and_a_lost_participant_frees_its_targets() {
    common::watchdog::run(
        module_path!(),
        "a_single_device_plays_alone_and_a_lost_participant_frees_its_targets",
        Some(4),
        a_single_device_plays_alone_and_a_lost_participant_frees_its_targets_scenario(),
    );
}

async fn a_single_device_plays_alone_and_a_lost_participant_frees_its_targets_scenario() {
    let base = 48_600 + (std::process::id() % 100) as u16 * 4;
    let a = start("solo", base).await;
    let an = a.node.clone();
    // 没有别的设备：只在本机进行。
    api(&an, |n| n.aim_start()).await.unwrap();
    wait_for("running alone", 5.0, || running(&a)).await;
    assert_eq!(an.aim_status().unwrap().devices, 1);
    wait_for("three targets on the only screen", 2.0, || {
        a.surface.view().is_some_and(|v| v.targets.len() == 3)
    })
    .await;
    // 标靶离屏幕边缘至少一个半径、不在顶部的计分栏里（19 mm × 4 单位/mm，再加一个半径）。
    for t in a.surface.view().unwrap().targets {
        assert!(t.center.x >= 28.0 - 1e-6 && t.center.x <= 1600.0 - 28.0 + 1e-6, "{t:?}");
        assert!(
            t.center.y >= 76.0 + 28.0 - 1e-6 && t.center.y <= 1000.0 - 28.0 + 1e-6,
            "{t:?}"
        );
    }
    // 没有叠加层的设备不能开始。
    api(&an, |n| n.aim_stop()).await.unwrap();
    wait_for("closed", 3.0, || !a.surface.is_open()).await;
    an.set_aim_surface(None);
    let st = an.aim_status().unwrap();
    assert!(!st.available && st.phase == AimPhase::Idle);
    assert!(api(&an, |n| n.aim_start()).await.is_err());
    a.node.shutdown().await;
}
