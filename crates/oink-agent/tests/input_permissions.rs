#![cfg(debug_assertions)]
//! Effective input permission and native outcome plumbing, with two full nodes
//! over real networking and fake OS input. Raw macOS AX/IOHID normalization is
//! covered by oink-input; this source can deliberately supply either missing right.
//!
//! - 缺「辅助功能」：注入被系统静默丢弃，本机按暂停处理——其他设备不把指针移进来（不再出现
//!   「指针移过去就消失」）；提醒写明「本机无法共享键鼠」；
//! - 两项权限均已授予后：其他设备可以控制本机；撤销任一权限收回旧会话；
//! - 授予「输入监控」后不必重启：截获自动启动，本机可以控制其他设备；
//! - 权限齐全且原生挂接实际失败：报告恢复/重新打开提示；仅未挂接不构成失败。

use oink_agent as test_agent;
mod common;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oink_agent::api::{AgentApi, AlertKind, AlertLevel, ApiEvent, PermissionKind, ServiceState, SubscriptionId};
use oink_agent::node::{Node, NodeOptions};
use oink_foundation::display::DisplayInfo;
use oink_foundation::input::{
    CaptureMode, CapturedEvent, Disposition, FeelProfile, InjectEvent, InputEvent, InputPermissions, PermissionState,
    SourceKind,
};
use oink_foundation::{Point, Rect, Size};
use oink_input::{EventHandler, InputBackend, InputError};
use oink_test_support::input::FakeInput;

fn display(key: &str) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds: Rect::new(0.0, 0.0, 1600.0, 1000.0),
        pixel_width: 1600,
        pixel_height: 1000,
        reference_extent: Some(Size::new(400.0, 250.0)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}

/// Fake effective permission source and native capture outcomes. These values are
/// already InputBackend observations, not raw TCC entries: macOS's AX-or-IOHID
/// normalization is tested in oink-input. Missing effective rights withdraw capture;
/// only a completed attachment failure reports capture_available(false).
#[derive(Clone)]
struct GatedInput {
    fake: FakeInput,
    perms: Arc<Mutex<InputPermissions>>,
    /// 权限齐全也启动失败（模拟「授权后仍需重新打开」）。
    fail_start: Arc<AtomicBool>,
    /// 本进程查到的「输入监控」是授权前的旧值：系统其实已经放行，截获能启动。
    stale_input_monitoring: Arc<AtomicBool>,
    injected: Arc<tokio::sync::Notify>,
}

impl GatedInput {
    fn new() -> Self {
        let fake = FakeInput::new();
        Self {
            fake,
            perms: Arc::new(Mutex::new(InputPermissions {
                accessibility: PermissionState::Denied,
                input_monitoring: PermissionState::Denied,
                secure_input_active: false,
            })),
            fail_start: Arc::new(AtomicBool::new(false)),
            stale_input_monitoring: Arc::new(AtomicBool::new(false)),
            injected: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn update(&self, f: impl FnOnce(&mut InputPermissions)) {
        let mut perms = self.perms.lock().unwrap();
        f(&mut perms);
    }

    fn get(&self) -> InputPermissions {
        self.perms.lock().unwrap().clone()
    }
}

struct GatedLoop {
    owner: Box<dyn oink_input::capture_loop::CaptureLoop>,
    input: GatedInput,
    allowed: bool,
    outcome: Option<bool>,
}
impl oink_input::capture_loop::CaptureLoop for GatedLoop {
    fn set_wake(&mut self, wake: oink_input::capture_loop::Wake) {
        self.owner.set_wake(wake);
    }
    fn event(&mut self, event: &CapturedEvent) -> Disposition {
        self.owner.event(event)
    }
    fn poll(&mut self, now: Instant) -> Option<Instant> {
        // Consume permission/pause commands before attempting native attachment,
        // matching the macOS capture loop's poll -> capture_requested -> install.
        let due = self.owner.poll(now);
        let permissions = self.input.get();
        let requested = self.owner.capture_requested()
            && permissions.accessibility == PermissionState::Granted
            && (permissions.input_monitoring == PermissionState::Granted
                || self.input.stale_input_monitoring.load(Ordering::Acquire));
        let outcome = requested.then(|| !self.input.fail_start.load(Ordering::Acquire));
        if outcome != self.outcome {
            match outcome {
                Some(false) => self.owner.capture_available(false),
                None if self.outcome.is_some() => self.owner.capture_detached(),
                // FakeInput reports successful attachment when allowed becomes true.
                _ => {}
            }
            self.outcome = outcome;
        }
        self.allowed = outcome == Some(true);
        let refresh = now + Duration::from_millis(50);
        Some(due.map_or(refresh, |due| due.min(refresh)))
    }
    fn stopped(&self) -> bool {
        self.owner.stopped()
    }
    fn capture_requested(&self) -> bool {
        self.allowed && self.owner.capture_requested()
    }
    fn capture_available(&mut self, available: bool) {
        self.owner.capture_available(available);
    }
    fn capture_detached(&mut self) {
        self.allowed = false;
        self.outcome = None;
        self.owner.capture_detached();
    }
}

impl InputBackend for GatedInput {
    fn set_input_paused(&self, paused: bool) {
        self.fake.set_input_paused(paused);
    }
    fn start_injection_session(&self) -> Result<(), InputError> {
        self.fake.start_injection_session()
    }
    fn stop_injection_session(&self) {
        self.fake.stop_injection_session();
    }
    fn input_safety(&self) -> oink_input::session_gate::InputSafety {
        self.fake.input_safety()
    }
    fn watch_input_safety(&self, wake: &oink_input::capture_loop::Wake) {
        self.fake.watch_input_safety(wake);
    }
    fn start_loop(&self, owner: Box<dyn oink_input::capture_loop::CaptureLoop>) -> Result<(), InputError> {
        self.fake.start_loop(Box::new(GatedLoop {
            owner,
            input: self.clone(),
            allowed: false,
            outcome: None,
        }))
    }
    fn capture_active(&self) -> Option<bool> {
        self.fake.capture_active()
    }

    fn display_power_observation(&self) -> Result<Box<dyn oink_input::display_power::Observation>, InputError> {
        self.fake.display_power_observation()
    }
    fn start(&self, handler: EventHandler) -> Result<(), InputError> {
        let p = self.get();
        let im = p.input_monitoring == PermissionState::Granted || self.stale_input_monitoring.load(Ordering::Acquire);
        if p.accessibility != PermissionState::Granted || !im {
            return Err(InputError::PermissionDenied("无法创建事件 tap".into()));
        }
        if self.fail_start.load(Ordering::Acquire) {
            return Err(InputError::Platform("tap 创建失败".into()));
        }
        self.fake.start(handler)
    }

    fn stop(&self) {
        self.fake.stop();
    }

    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        self.fake.set_mode(mode)
    }

    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        if self.get().accessibility != PermissionState::Granted {
            // macOS：没有「辅助功能」时 CGEventPost 不报错，事件被丢弃。
            return Ok(());
        }
        self.fake.inject(event)?;
        self.injected.notify_one();
        Ok(())
    }

    fn cursor_position(&self) -> Result<Point, InputError> {
        self.fake.cursor_position()
    }

    fn feel_profile(&self) -> FeelProfile {
        self.fake.feel_profile()
    }

    fn permissions(&self) -> InputPermissions {
        self.get()
    }
}

fn network_config(dir: std::path::PathBuf, name: &str, port: u16) -> oink_network::NetworkConfig {
    // Permission scenarios use interactive QUIC, without bulk transfers. Keep
    // port 0 until the live socket binds: probing and releasing an available
    // port leaves a race with other processes on hosted runners.
    oink_network::NetworkConfig {
        data_dir: dir,
        device_name: name.into(),
        os: std::env::consts::OS.into(),
        bind_ip: std::net::Ipv4Addr::LOCALHOST.into(),
        port,
        bulk_port: 0,
        bulk_pacing: true,
        mdns: oink_network::MdnsMode::Off,
        beacon: false,
        beacon_ports: Vec::new(),
    }
}

#[test]
fn permission_fixture_binds_ephemeral_ports_atomically() {
    let config = network_config("unused-permission-fixture".into(), "fixture", 0);
    assert_eq!(
        config.port, 0,
        "the live socket must allocate its port, without a probe/drop/rebind gap"
    );
    assert_eq!(config.discovery_port(), 0);
}

async fn start(name: &str, port: u16, input: Arc<dyn InputBackend>) -> Node {
    let dir = std::env::temp_dir().join(format!("oink-input-perms-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Node::start_with_fake_platform(NodeOptions {
        net: network_config(dir, name, port),
        input,
        displays: vec![display(name)],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap()
}

async fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    loop {
        if f() {
            return;
        }
        if Instant::now() >= end {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// 模拟本机系统：本机模式下按位移移动光标（出了屏幕停在边上），交给截获回调。
fn os_move(input: &FakeInput, dx: f64) {
    let b = Rect::new(0.0, 0.0, 1600.0, 1000.0);
    let cur = input.cursor_position().unwrap();
    let local = matches!(input.mode(), Some(CaptureMode::Local));
    let next = if local {
        b.clamp(Point::new(cur.x + dx, cur.y))
    } else {
        cur
    };
    let d = input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::PointerDelta { dx, dy: 0.0 },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: Some(next),
    });
    if local && d == Disposition::Pass {
        input.set_cursor(next);
    }
}

/// Capture callbacks update the local router synchronously. Feed a fixed gesture and
/// stop as soon as it hands off; peer confirmation is awaited separately via events.
fn push(input: &FakeInput, dir: f64, mut done: impl FnMut() -> bool) {
    input.set_cursor(Point::new(if dir > 0.0 { 1580.0 } else { 20.0 }, 500.0));
    for _ in 0..30 {
        os_move(input, 10.0 * dir);
        if done() {
            return;
        }
    }
}

/// Subscribe before acting, then recheck conditions on relevant state invalidations.
/// Marking the watch value before checking prevents losing a concurrent transition.
/// No elapsed-time assertion includes scheduler delay or permission-check tick latency.
struct StateChanges {
    nodes: Vec<(Node, SubscriptionId)>,
    changed: tokio::sync::watch::Receiver<u64>,
}

impl StateChanges {
    fn new(nodes: &[&Node]) -> Self {
        let (tx, changed) = tokio::sync::watch::channel(0);
        let nodes = nodes
            .iter()
            .map(|node| {
                let tx = tx.clone();
                let id = node.subscribe(Arc::new(move |event| {
                    if matches!(
                        event,
                        ApiEvent::PairingChanged { .. }
                            | ApiEvent::PairingRequestsChanged { .. }
                            | ApiEvent::Group(_)
                            | ApiEvent::Layout(_)
                            | ApiEvent::Focus(_)
                            | ApiEvent::Permissions(_)
                            | ApiEvent::Overview(_)
                            | ApiEvent::DeviceCards(_)
                    ) {
                        tx.send_modify(|revision| *revision += 1);
                    }
                }));
                ((*node).clone(), id)
            })
            .collect();
        Self { nodes, changed }
    }

    async fn wait_for(&mut self, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            self.changed.borrow_and_update();
            if ready() {
                return;
            }
            tokio::time::timeout_at(deadline, self.changed.changed())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for {what}: {:?}",
                        self.nodes.iter().map(|(n, _)| n.input_status()).collect::<Vec<_>>()
                    )
                })
                .expect("state event stream closed");
        }
    }
}

impl Drop for StateChanges {
    fn drop(&mut self) {
        for (node, id) in &self.nodes {
            node.unsubscribe(*id);
        }
    }
}

fn permission_alert(n: &Node, kind: PermissionKind) -> Option<oink_agent::api::Alert> {
    n.overview()
        .unwrap()
        .alerts
        .into_iter()
        .find(|a| matches!(&a.kind, AlertKind::PermissionMissing { permissions } if permissions.iter().any(|p| p.kind == kind)))
}

fn has_alert(n: &Node, id: &str) -> bool {
    n.overview().unwrap().alerts.iter().any(|a| a.id == id)
}

#[test]
fn missing_permissions_block_then_recover_without_restart() {
    common::watchdog::run(
        module_path!(),
        "missing_permissions_block_then_recover_without_restart",
        Some(4),
        missing_permissions_block_then_recover_without_restart_scenario(),
    );
}

async fn missing_permissions_block_then_recover_without_restart_scenario() {
    oink_agent::logs::init();
    let base = 48_100 + (std::process::id() % 100) as u16 * 4;
    let gated = GatedInput::new();
    let b_input = FakeInput::new();
    b_input.set_cursor(Point::new(800.0, 500.0));
    let an = start("mac", base, Arc::new(gated.clone())).await;
    let bn = start("win", base + 2, Arc::new(b_input.clone())).await;

    let mut changes = StateChanges::new(&[&an, &bn]);

    // ---- 配对。
    common::pair(&an, &bn).await;
    let (a2, b2) = (an.clone(), bn.clone());
    changes
        .wait_for("connected, layouts include both", || {
            common::input_id(&b2, a2.id()).is_some()
                && a2.online_set().contains(&b2.id())
                && a2.layout().0.screens.len() == 2
                && b2.layout().0.screens.len() == 2
        })
        .await;

    // Pairing can resolve the two saved origins in either direction. Give this
    // permission/handoff test an explicit horizontally adjacent-screen layout.
    let current = bn.layout().0;
    let a_rect = current.screens.iter().find(|s| s.id.device == an.id()).unwrap().rect;
    let b_rect = current.screens.iter().find(|s| s.id.device == bn.id()).unwrap().rect;
    let delta = Point::new(a_rect.right() - b_rect.x, a_rect.y - b_rect.y);
    let node = bn.clone();
    let bid = bn.id();
    tokio::task::spawn_blocking(move || node.move_device(bid.into(), delta))
        .await
        .unwrap()
        .unwrap();
    changes
        .wait_for("adjacent layout applied", || {
            [&an, &bn].iter().all(|node| {
                let layout = node.layout().0;
                let a = layout.screens.iter().find(|s| s.id.device == an.id()).unwrap();
                let b = layout.screens.iter().find(|s| s.id.device == bn.id()).unwrap();
                (b.rect.x - a.rect.right()).abs() < 1e-6 && (b.rect.y - a.rect.y).abs() < 1e-6
            })
        })
        .await;

    // ---- 缺两项权限：截获没启动，提醒写明本机无法共享键鼠；B 把本机当作不可达。
    assert!(an.capture_active() != Some(true));
    let o = an.overview().unwrap();
    assert_eq!(o.service, ServiceState::Degraded);
    let ax = permission_alert(&an, PermissionKind::Accessibility).unwrap();
    assert_eq!(
        ax.problem.unwrap().code,
        oink_agent::device_cards::UserFacingCode::PermissionDenied
    );
    assert!(permission_alert(&an, PermissionKind::InputMonitoring).is_some());
    assert!(!an.input_status().unwrap().capture_failed);
    assert!(!has_alert(&an, "capture-unavailable"));
    let b2 = bn.clone();
    let aid = an.id();
    let remote_ax = || {
        bn.device_cards().ok().and_then(|view| {
            view.cards
                .iter()
                .find(|c| c.device == aid.into())
                .and_then(|c| c.status.as_ref())
                .and_then(|s| s.accessibility_granted)
        })
    };
    if cfg!(target_os = "macos") {
        changes
            .wait_for("B receives explicit missing AX owner fact", || {
                remote_ax() == Some(false)
            })
            .await;
    }
    changes
        .wait_for("B treats A as unreachable", || !b2.online_set().contains(&aid))
        .await;

    let la = an.layout().0;
    let ax_ = la.screens.iter().find(|s| s.id.device == an.id()).unwrap().rect.x;
    let bx = la.screens.iter().find(|s| s.id.device == bn.id()).unwrap().rect.x;
    let to_a = if ax_ > bx { 1.0 } else { -1.0 };
    let b2 = bn.clone();
    push(&b_input, to_a, || {
        b2.input_status().unwrap().focus_info.clone().unwrap().holder == aid
    });
    assert_eq!(
        bn.input_status().unwrap().focus_info.clone().unwrap().holder,
        bn.id(),
        "pointer must not vanish into a device that cannot inject"
    );

    // Both rights are required: a target must retain its local escape/capture path.
    gated.update(|p| {
        p.accessibility = PermissionState::Granted;
        p.input_monitoring = PermissionState::Granted;
    });
    let b2 = bn.clone();
    changes
        .wait_for("B sees A reachable again", || b2.online_set().contains(&aid))
        .await;
    if cfg!(target_os = "macos") {
        changes
            .wait_for("B receives granted AX without reconnecting", || {
                remote_ax() == Some(true)
            })
            .await;
    }
    assert!(permission_alert(&an, PermissionKind::Accessibility).is_none());
    assert!(permission_alert(&an, PermissionKind::InputMonitoring).is_none());
    wait_for("target capture is active", 3., || an.capture_active() == Some(true)).await;
    assert!(!an.input_status().unwrap().capture_failed);
    assert!(!has_alert(&an, "capture-unavailable"));
    let b2 = bn.clone();
    push(&b_input, to_a, || {
        b2.input_status().unwrap().focus_info.clone().unwrap().holder == aid
    });
    let a2 = an.clone();
    changes
        .wait_for("A confirms this handoff", || {
            let a = a2.input_status().unwrap().focus_info.clone().unwrap();
            let b = bn.input_status().unwrap().focus_info.clone().unwrap();
            a.holder == aid && b.holder == aid && a.epoch == b.epoch
        })
        .await;
    for _ in 0..10 {
        os_move(&b_input, 10.0 * to_a);
    }
    // Injection has its own backend notification: a focus event only proves routing,
    // not that the separate local-action worker has delivered motion to the backend.
    loop {
        let notified = gated.injected.notified();
        if gated
            .fake
            .take_injected()
            .iter()
            .any(|e| matches!(e, InjectEvent::MoveTo { .. }))
        {
            break;
        }
        notified.await;
    }

    // Permission observation changes status, never the pause/resume epoch.
    let original_safety = gated.input_safety();
    gated.update(|p| {
        p.accessibility = PermissionState::Denied;
        p.input_monitoring = PermissionState::Denied;
    });
    wait_for("missing permission remains visible", 3., || {
        permission_alert(&an, PermissionKind::Accessibility).is_some()
    })
    .await;
    if cfg!(target_os = "macos") {
        changes
            .wait_for("B receives revoked AX without changing layout", || {
                remote_ax() == Some(false)
            })
            .await;
    }
    assert_eq!(gated.input_safety(), original_safety);
    assert!(!an.pause_status().paused);

    // Regrant with a completed native attach failure, not a withheld capture
    // request: only the former warrants the existing recovery/reopen guidance.
    gated.fail_start.store(true, Ordering::Release);
    gated.update(|p| {
        p.accessibility = PermissionState::Granted;
        p.input_monitoring = PermissionState::Granted;
    });
    let a2 = an.clone();
    changes
        .wait_for("completed native failure produces recovery alert", || {
            has_alert(&a2, "capture-unavailable")
        })
        .await;
    assert!(an.input_status().unwrap().capture_failed);
    assert!(an.capture_active() != Some(true));
    assert_eq!(an.overview().unwrap().service, ServiceState::Degraded);

    // ---- 截获可以启动了：自动启动（权限状态一有变化就立即重试，不必等重试间隔；这里借安全输入标志触发）。
    gated.fail_start.store(false, Ordering::Release);
    gated.update(|p| p.secure_input_active = true);
    let a2 = an.clone();
    changes
        .wait_for("capture starts without restart", || a2.capture_active() == Some(true))
        .await;
    assert!(gated.fake.mode().is_some());
    gated.update(|p| p.secure_input_active = false);
    let a2 = an.clone();
    changes
        .wait_for("all alerts cleared", || a2.overview().unwrap().alerts.is_empty())
        .await;
    assert_eq!(an.overview().unwrap().service, ServiceState::Running);
    assert!(!an.input_status().unwrap().capture_failed);

    // Permission/capture recovery precedes the asynchronous new I0 opening.
    // Wait for both fresh stream pairs before feeding the finite edge gesture.
    wait_for("fresh input pair ready after regrant", 5., || {
        an.online_set().contains(&bn.id()) && bn.online_set().contains(&an.id())
    })
    .await;

    // ---- 本机的键鼠可以控制 B 了。
    let b2 = bn.clone();
    let bid = bn.id();
    let a2 = an.clone();
    push(&gated.fake, -to_a, || {
        a2.input_status().unwrap().focus_info.clone().unwrap().holder == bid
    });
    changes
        .wait_for("B confirms this handoff", || {
            let a = an.input_status().unwrap().focus_info.clone().unwrap();
            let b = b2.input_status().unwrap().focus_info.clone().unwrap();
            a.holder == bid && b.holder == bid && a.epoch == b.epoch
        })
        .await;
    assert!(matches!(gated.fake.mode(), Some(CaptureMode::Remote { .. })));

    an.shutdown().await;
    bn.shutdown().await;
}

#[test]
fn input_monitoring_not_determined_or_stale() {
    common::watchdog::run(
        module_path!(),
        "input_monitoring_not_determined_or_stale",
        Some(2),
        input_monitoring_not_determined_or_stale_scenario(),
    );
}

async fn input_monitoring_not_determined_or_stale_scenario() {
    oink_agent::logs::init();
    let base = 48_500 + (std::process::id() % 100) as u16 * 2;
    let gated = GatedInput::new();
    gated.update(|p| {
        p.accessibility = PermissionState::Granted;
        p.input_monitoring = PermissionState::NotDetermined;
    });
    let n = start("solo", base, Arc::new(gated.clone())).await;

    // 「未询问」也是缺：部分不可用，提醒本机无法共享键鼠。
    assert!(n.capture_active() != Some(true));
    assert_eq!(n.overview().unwrap().service, ServiceState::Degraded);
    let im = permission_alert(&n, PermissionKind::InputMonitoring).unwrap();
    assert_eq!(im.level, AlertLevel::Error);
    assert_eq!(
        im.problem.unwrap().code,
        oink_agent::device_cards::UserFacingCode::PermissionRequired
    );

    // A live/stale tap must not override a current missing effective right. This
    // deliberately inconsistent fake source tests owner admission, not raw TCC:
    // the real macOS backend includes AX authorization in its listening result.
    gated.update(|p| p.input_monitoring = PermissionState::Denied);
    gated.stale_input_monitoring.store(true, Ordering::Release);
    let n2 = n.clone();
    wait_for("missing current grant keeps capture withdrawn", 3.0, || {
        n2.capture_active() == Some(false)
    })
    .await;
    let n2 = n.clone();
    wait_for("missing grant remains an error", 5.0, || {
        permission_alert(&n2, PermissionKind::InputMonitoring).is_some_and(|a| a.level == AlertLevel::Error)
    })
    .await;
    assert!(!has_alert(&n, "capture-unavailable"));
    wait_for("missing grant remains a permission card fault", 5.0, || {
        n.device_cards().unwrap().cards.iter().any(|card| {
            card.is_self
                && card.status.as_ref().is_some_and(|status| {
                    status.keyboard.resource.fault.as_ref().is_some_and(|fault| {
                        fault.code == oink_agent::device_cards::ResourceFaultCode::PermissionDenied
                    })
                })
        })
    })
    .await;
    n.shutdown().await;
}

#[test]
fn fresh_permissions_are_one_neutral_alert_and_required_card_faults() {
    common::watchdog::run(
        module_path!(),
        "fresh_permissions_are_one_neutral_alert_and_required_card_faults",
        Some(2),
        async {
            use oink_agent::device_cards::{CardAction, ResourceFaultCode, UserFacingCode};
            let input = GatedInput::new();
            input.update(|p| {
                p.accessibility = PermissionState::NotGranted;
                p.input_monitoring = PermissionState::NotDetermined;
            });
            let node = start("fresh", 0, Arc::new(input.clone())).await;
            let mut changes = StateChanges::new(&[&node]);
            let alert = permission_alert(&node, PermissionKind::Accessibility).unwrap();
            assert_eq!(alert.problem.as_ref().unwrap().code, UserFacingCode::PermissionRequired);
            assert_eq!(alert.id, "permissions-missing");
            let AlertKind::PermissionMissing { permissions } = &alert.kind else {
                panic!("permission alert")
            };
            assert_eq!(permissions.len(), 2);
            assert_eq!(permissions[0].state, PermissionState::NotGranted);
            assert_eq!(permissions[1].state, PermissionState::NotDetermined);
            let json = serde_json::to_value(&alert).unwrap();
            assert_eq!(json["kind"]["permissions"][0]["state"], "NotGranted");
            assert_eq!(
                node.overview()
                    .unwrap()
                    .alerts
                    .iter()
                    .filter(|a| matches!(a.kind, AlertKind::PermissionMissing { .. }))
                    .count(),
                1
            );
            let card_faults = || {
                let cards = node.device_cards().unwrap();
                let status = cards.cards.iter().find(|c| c.is_self)?.status.as_ref()?;
                Some((
                    status.mouse.fault.as_ref()?.code,
                    status.keyboard.resource.fault.as_ref()?.code,
                    status.mouse.action.clone(),
                ))
            };
            changes
                .wait_for("required permission faults reach device cards", || {
                    card_faults()
                        == Some((
                            ResourceFaultCode::PermissionRequired,
                            ResourceFaultCode::PermissionRequired,
                            CardAction::OpenPermissions,
                        ))
                })
                .await;
            input.update(|p| {
                p.accessibility = PermissionState::Denied;
                p.input_monitoring = PermissionState::Denied;
            });
            changes
                .wait_for("explicit denial stays distinct", || {
                    permission_alert(&node, PermissionKind::Accessibility)
                        .is_some_and(|a| a.problem.unwrap().code == UserFacingCode::PermissionDenied)
                        && card_faults()
                            == Some((
                                ResourceFaultCode::PermissionDenied,
                                ResourceFaultCode::PermissionDenied,
                                CardAction::OpenPermissions,
                            ))
                })
                .await;
            input.update(|p| {
                p.accessibility = PermissionState::Granted;
                p.input_monitoring = PermissionState::NotGranted;
            });
            changes
                .wait_for("one missing permission replaces the same alert", || {
                    permission_alert(&node, PermissionKind::InputMonitoring).is_some_and(|a| {
                        a.id == alert.id
                            && a.problem.unwrap().code == UserFacingCode::PermissionRequired
                            && matches!(a.kind, AlertKind::PermissionMissing { permissions } if permissions.len() == 1)
                    })
                })
                .await;
            assert!(permission_alert(&node, PermissionKind::Accessibility).is_none());
            input.update(|p| p.input_monitoring = PermissionState::Granted);
            changes
                .wait_for("grants clear the permission issue", || {
                    permission_alert(&node, PermissionKind::InputMonitoring).is_none()
                })
                .await;
            node.shutdown().await;
        },
    );
}

#[test]
#[cfg(target_os = "macos")]
fn input_monitoring_changes_publish_before_accessibility_without_capture() {
    common::watchdog::run(
        module_path!(),
        "input_monitoring_changes_publish_before_accessibility_without_capture",
        Some(2),
        async {
            let input = GatedInput::new();
            let node = start("im-first", 0, Arc::new(input.clone())).await;
            let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
            let sub = node.subscribe(Arc::new(move |event| {
                if let ApiEvent::Permissions(items) = event {
                    let _ = tx.send(items.clone());
                }
            }));
            let original_safety = input.input_safety();
            for state in [
                PermissionState::Granted,
                PermissionState::NotGranted,
                PermissionState::Granted,
            ] {
                input.update(|p| p.input_monitoring = state);
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let items = events.recv().await.unwrap();
                        let ax = items.iter().find(|p| p.kind == PermissionKind::Accessibility).unwrap();
                        let im = items
                            .iter()
                            .find(|p| p.kind == PermissionKind::InputMonitoring)
                            .unwrap();
                        assert_eq!(ax.state, PermissionState::Denied);
                        if im.state == state {
                            break;
                        }
                    }
                })
                .await
                .expect("IM-only changes must push without AX or a permission query");
                assert_ne!(node.capture_active(), Some(true));
                assert_eq!(input.input_safety(), original_safety);
            }
            node.unsubscribe(sub);
            node.shutdown().await;
        },
    );
}
