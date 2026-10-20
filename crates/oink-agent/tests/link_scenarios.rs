#![cfg(debug_assertions)]
//! Link scenarios: several complete backends in one process over
//! real loopback QUIC/TLS/TCP with fake input, clipboard and display backends and temporary
//! state directories. Nothing here touches an installed Oink or injects native input.
use oink_agent as test_agent;
mod common;

use oink_agent::api::{AgentApi, FileTransferPhase, Placement, ScreenKey};
use oink_agent::node::{Node, NodeOptions};
use oink_clipboard::{ClipData, Sensitivity};
use oink_display::wallpaper::{DisplayWallpaper, Thumbnail};
use oink_display::{ChangeHandler, DisplayBackend, DisplayError};
use oink_foundation::display::DisplayInfo;
use oink_foundation::input::{CaptureMode, CapturedEvent, InjectEvent, InputEvent, KeyCode, SourceKind};
use oink_foundation::{DeviceId, Point, Rect, Size};
use oink_input::InputBackend;
use oink_network::NetworkConfig;
use oink_network::link::{Disconnect, LinkPath};
use oink_test_support::clipboard::FakeClipboard;
use oink_test_support::input::{FakeInput, PendingMode};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Fake display backend: enumeration and native notifications are independently driven.
struct Displays {
    key: String,
    current: Mutex<Vec<DisplayInfo>>,
    changes: Mutex<Vec<ChangeHandler>>,
    fill: Mutex<u8>,
    handlers: Mutex<Vec<ChangeHandler>>,
}
impl Displays {
    fn set_displays(&self, displays: Vec<DisplayInfo>) {
        *self.current.lock().unwrap() = displays;
        for handler in self.changes.lock().unwrap().clone() {
            handler();
        }
    }
    fn set_wallpaper(&self, fill: u8) {
        *self.fill.lock().unwrap() = fill;
        for handler in self.handlers.lock().unwrap().iter() {
            handler();
        }
    }
}
impl DisplayBackend for Displays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        Ok(self.current.lock().unwrap().clone())
    }
    fn watch(&self, handler: ChangeHandler) -> Result<(), DisplayError> {
        self.changes.lock().unwrap().push(handler);
        Ok(())
    }
    fn wallpapers(&self) -> Option<Vec<DisplayWallpaper>> {
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.resize(20_000, *self.fill.lock().unwrap());
        Some(vec![DisplayWallpaper {
            key: self.key.clone(),
            thumbnail: Thumbnail {
                width: 480,
                height: 300,
                jpeg,
            },
        }])
    }
    fn watch_wallpaper(&self, on_change: ChangeHandler) -> Result<(), DisplayError> {
        self.handlers.lock().unwrap().push(on_change);
        Ok(())
    }
}

struct Dev {
    node: Node,
    input: FakeInput,
    clip: Arc<FakeClipboard>,
    displays: Arc<Displays>,
    dir: PathBuf,
    name: String,
    port: u16,
}

/// Starts a backend. A device keeps its port across restarts, as the product does;
/// `port` 0 picks a free one.
async fn start(root: &Path, name: &str) -> Dev {
    start_on(root, name, 0).await
}
async fn restart(root: &Path, old: &Dev) -> Dev {
    start_on(root, &old.name, old.port).await
}
async fn start_on(root: &Path, name: &str, port: u16) -> Dev {
    common::phase("Node::start");
    if std::env::var_os("OINK_LOG").is_some() {
        oink_agent::logs::init();
    }
    let net = oink_test_support::network_config(root.join(name), name, port);
    start_config(root, name, net).await
}
async fn start_config(root: &Path, name: &str, net: NetworkConfig) -> Dev {
    let dir = root.join(name);
    let port = net.port;
    let input = FakeInput::new();
    if std::env::var_os("OINK_MEASURE_POINTER").is_some() {
        input.set_capture_tier(oink_foundation::realtime::Tier::Realtime);
    }
    input.set_cursor(Point::new(800.0, 500.0));
    let clip = Arc::new(FakeClipboard::new());
    let displays = Arc::new(Displays {
        key: name.into(),
        current: Mutex::new(vec![display(name)]),
        changes: Mutex::default(),
        fill: Mutex::new(name.as_bytes()[0]),
        handlers: Mutex::default(),
    });
    let node = Node::start_with_fake_platform(NodeOptions {
        net,
        input: Arc::new(input.clone()),
        displays: vec![display(name)],
        display_backend: Some(displays.clone()),
        clipboard: Some(clip.clone()),
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap_or_else(|e| panic!("start {name}: {e:#}"));
    if let Ok(mut settings) = node.clipboard_settings() {
        settings.files = true;
        node.set_clipboard_settings(settings.into()).unwrap();
    }
    Dev {
        node,
        input,
        clip,
        displays,
        dir,
        name: name.into(),
        port,
    }
}

async fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    common::phase(what);
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
    common::phase("blocking AgentApi request");
    let n = n.clone();
    tokio::task::spawn_blocking(move || f(&n)).await.unwrap()
}

/// Puts the devices' screens in one row, left to right in the given order, and waits
/// until every device has applied that layout.
async fn row(devs: &[&Dev]) {
    let placements = devs
        .iter()
        .enumerate()
        .map(|(i, d)| Placement {
            device: d.node.id().into(),
            origin: Point::new(400.0 / 686.0 * i as f64, 0.0),
            scale: 400.0 / 686.0 / 1600.0,
        })
        .collect::<Vec<_>>();
    let end = Instant::now() + Duration::from_secs(15);
    while !devs.iter().all(|d| d.node.layout().0.screens.len() == devs.len()) {
        assert!(
            Instant::now() < end,
            "timed out waiting for all screens in the shared layout; per device (layout version, screens): {:?}{}",
            devs.iter()
                .map(|d| {
                    let (layout, version, _) = d.node.layout();
                    (
                        d.name.clone(),
                        version.revision,
                        layout.screens.iter().map(|s| s.id.display.clone()).collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
            dump(devs)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    api(&devs[0].node, move |n| n.save_layout(placements)).await.unwrap();
    wait_for("row layout applied everywhere", 15.0, || {
        let first = devs[0].node.layout();
        devs.iter().all(|d| {
            let compiled = d.node.link_config().unwrap().router_layout().load();
            d.node.layout().0 == first.0
                && first.0.screens.iter().all(|screen| {
                    compiled
                        .topology
                        .topology()
                        .screen(&screen.id)
                        .is_some_and(|actual| actual.placement == *screen)
                        && compiled.topology.topology().is_active(&screen.id)
                })
        }) && devs.windows(2).all(|pair| {
            let x = |d: &Dev| {
                first
                    .0
                    .screens
                    .iter()
                    .find(|s| s.id.device == d.node.id())
                    .map(|s| s.rect.x)
            };
            x(pair[0]) < x(pair[1])
        })
    })
    .await;
}

fn os_move(input: &FakeInput, dx: f64) {
    os_move_xy(input, dx, 0.);
}

fn os_move_xy(input: &FakeInput, dx: f64, dy: f64) {
    let bounds = Rect::new(0.0, 0.0, 1600.0, 1000.0);
    let cur = input.cursor_position().unwrap();
    let local = matches!(input.mode(), Some(CaptureMode::Local));
    let next = if local {
        bounds.clamp(Point::new(cur.x + dx, cur.y + dy))
    } else {
        cur
    };
    let d = input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::PointerDelta { dx, dy },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: Some(next),
    });
    if local && d == oink_foundation::input::Disposition::Pass {
        input.set_cursor(next);
    }
}

fn key(input: &FakeInput, code: u32, pressed: bool) {
    input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::Key {
            code: KeyCode::keyboard(code as u16),
            pressed,
        },
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    });
}

fn holder(node: &Node) -> Option<DeviceId> {
    node.input_status().ok()?.focus_info.clone().map(|f| f.holder)
}

// Link loss publishes router focus after enqueueing native restoration. The
// injection worker applies that mode separately, so focus alone is not readiness.
fn local_pointer_ready(dev: &Dev) -> bool {
    holder(&dev.node) == Some(dev.node.id()) && matches!(dev.input.mode(), Some(CaptureMode::Local))
}

/// Injected key events on a device as (usage, pressed).
fn keys(input: &FakeInput) -> Vec<(u32, bool)> {
    input
        .injected()
        .into_iter()
        .filter_map(|e| match e {
            InjectEvent::Key { code, pressed } => Some((code.0 & 0xFFFF, pressed)),
            _ => None,
        })
        .collect()
}

/// Pushes `from`'s pointer over its edge (dir: +1 right, -1 left) until `to` holds focus.
async fn cross(from: &Dev, to: &Dev, dir: f64) {
    from.input
        .set_cursor(Point::new(if dir > 0.0 { 1580.0 } else { 20.0 }, 500.0));
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        os_move(&from.input, 10.0 * dir);
        tokio::time::sleep(Duration::from_millis(10)).await;
        if holder(&from.node) == Some(to.node.id()) && holder(&to.node) == Some(to.node.id()) {
            return;
        }
        assert!(
            Instant::now() < end,
            "{} could not hand focus to {} (holders: {:?} / {:?})",
            from.name,
            to.name,
            holder(&from.node),
            holder(&to.node)
        );
    }
}

/// Types `count` distinct press/release pairs on `from` (which is driving `to`) and
/// checks that `to` injected exactly that sequence after `already` earlier key events.
async fn type_and_check(from: &Dev, to: &Dev, count: u32, what: &str) {
    let already = keys(&to.input).len();
    let mut expected = Vec::new();
    for i in 0..count {
        let usage = 4 + (i % 26);
        key(&from.input, usage, true);
        key(&from.input, usage, false);
        expected.push((usage, true));
        expected.push((usage, false));
    }
    let end = Instant::now() + Duration::from_secs(10);
    while keys(&to.input).len() < already + expected.len() && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let got = keys(&to.input)[already..].to_vec();
    assert_eq!(got, expected, "{what}: keys injected on {}", to.name);
}

async fn clipboard_reaches(from: &Dev, to: &Dev, text: &str) {
    from.clip.copy(vec![ClipData::Text(text.into())], Sensitivity::Normal);
    let clip = to.clip.clone();
    let expected = vec![ClipData::Text(text.into())];
    wait_for(&format!("clipboard text {text:?} on {}", to.name), 10.0, || {
        clip.items() == expected
    })
    .await;
}

fn wallpaper_seen(node: &Node, of: &Node) -> Option<String> {
    node.wallpapers(false)
        .unwrap()
        .into_iter()
        .find(|w| w.key.device.0 == of.id())
        .map(|w| w.rev)
}

fn path_to(node: &Node, peer: &Node) -> Option<LinkPath> {
    node.links().ok()?.links().by_peer(peer.id()).map(|l| l.path)
}

fn group_id(node: &Node) -> Option<String> {
    node.overview().ok()?.group.map(|g| format!("{:?}", g.id))
}

/// Immutable group id and signed roster root as stored.
async fn group_anchor(dev: &Dev) -> (oink_foundation::GroupId, [u8; 32]) {
    let stored = oink_agent::config_module::group_store::load(&dev.dir.join("group-state.json"), dev.node.id())
        .unwrap()
        .unwrap();
    (stored.group(), stored.state.roster().unwrap().root_id().unwrap().0)
}

fn member_ids(node: &Node) -> Vec<DeviceId> {
    let mut ids: Vec<_> = node.group().unwrap().members.iter().map(|m| m.device.id.0).collect();
    ids.sort();
    ids
}
fn ids(devs: &[&Dev]) -> Vec<DeviceId> {
    let mut ids: Vec<_> = devs.iter().map(|d| d.node.id()).collect();
    ids.sort();
    ids
}

/// State of every device as the others see it, for failure messages.
fn dump(devs: &[&Dev]) -> String {
    let mut out = String::new();
    for d in devs {
        let n = &d.node;
        out += &format!(
            "\n{} {:?} group={:?} members={:?}",
            d.name,
            n.id(),
            n.overview().ok().and_then(|o| o.group.map(|g| g.id)),
            n.group().map(|g| g.members.len()).ok()
        );
        if let Ok(group) = n.group() {
            for m in group.members.iter().filter(|m| !m.is_self) {
                let link = n.links().ok().and_then(|l| {
                    l.links()
                        .by_peer(m.device.id.0)
                        .map(|l| format!("{:?}/{:?}", l.state, l.path))
                });
                let input = n.input_status().ok().and_then(|s| {
                    s.peers.iter().find(|p| p.peer == m.device.id.0).map(|p| {
                        format!(
                            "in={} out={} attached={} online={}",
                            p.inbound.is_some(),
                            p.outbound.is_some(),
                            p.attached,
                            p.online
                        )
                    })
                });
                out += &format!(
                    "\n   -> {} {:?}: sharing={:?} link={link:?} input={input:?}",
                    m.device.name, m.device.id.0, m.sharing
                );
            }
        }
    }
    out
}

/// Every device shares input with every other one.
async fn active(devs: &[&Dev], secs: f64, what: &str) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    loop {
        let ok = devs.iter().all(|a| {
            let group = a.node.group().unwrap();
            devs.iter().all(|b| {
                a.node.id() == b.node.id()
                    || group
                        .members
                        .iter()
                        .any(|m| m.device.id.0 == b.node.id() && m.sharing == oink_agent::api::SharingState::Active)
            })
        });
        if ok {
            return;
        }
        assert!(Instant::now() < end, "not all sharing ({what}):{}", dump(devs));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `wait_for` that shows every device's view when it times out.
async fn wait_all(devs: &[&Dev], what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}:{}", dump(devs));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn shutdown(devs: Vec<Dev>) {
    for dev in devs {
        dev.node.shutdown().await;
    }
}

/// Three devices, A and B reach each other only through R.
async fn relayed_trio(root: &Path) -> (Dev, Dev, Dev) {
    let a = start(root, "alpha").await;
    let b = start(root, "beta").await;
    let r = start(root, "relay").await;
    oink_network::link::testing::set_direct_blocked(&a.node.links().unwrap(), b.node.id(), true);
    oink_network::link::testing::set_direct_blocked(&b.node.links().unwrap(), a.node.id(), true);
    common::pair(&r.node, &a.node).await;
    common::pair(&r.node, &b.node).await;
    active(&[&a, &b, &r], 15.0, "relayed trio").await;
    wait_for("A-B link is relayed", 15.0, || {
        matches!(path_to(&a.node, &b.node), Some(LinkPath::Relayed { .. }))
            && matches!(path_to(&b.node, &a.node), Some(LinkPath::Relayed { .. }))
    })
    .await;
    let group = a.node.group().unwrap();
    assert_eq!(
        group
            .members
            .iter()
            .find(|m| m.device.id.0 == b.node.id())
            .unwrap()
            .link
            .as_ref()
            .unwrap()
            .remote_address,
        None
    );
    (a, b, r)
}

// ───────────── 1. three devices, one reachable only via relay ─────────────

#[test]
fn relay_only_peer_gets_input_clipboard_wallpaper_and_diagnostics() {
    common::watchdog::run(
        module_path!(),
        "relay_only_peer_gets_input_clipboard_wallpaper_and_diagnostics",
        Some(4),
        relay_only_peer_gets_input_clipboard_wallpaper_and_diagnostics_scenario(),
    );
}

async fn relay_only_peer_gets_input_clipboard_wallpaper_and_diagnostics_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, r) = relayed_trio(root.path()).await;
    row(&[&r, &a, &b]).await;

    // Input: A drives B over the relayed link, keys arrive complete and in order.
    cross(&a, &b, 1.0).await;
    type_and_check(&a, &b, 40, "relayed typing").await;
    assert!(keys(&r.input).is_empty(), "the relay must not inject forwarded keys");

    // Clipboard in both directions, small and large (Low operation through the relay).
    clipboard_reaches(&a, &b, "relayed a to b").await;
    clipboard_reaches(&b, &a, "relayed b to a").await;
    let png: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    a.clip.copy(vec![ClipData::Png(png.clone())], Sensitivity::Normal);
    let clip = b.clip.clone();
    wait_for("relayed image on B", 15.0, || {
        clip.items() == vec![ClipData::Png(png.clone())]
    })
    .await;

    // Wallpaper: initial copy and a later change.
    wait_for("B has A's wallpaper", 15.0, || {
        wallpaper_seen(&b.node, &a.node).is_some()
    })
    .await;
    let before = wallpaper_seen(&b.node, &a.node).unwrap();
    a.displays.set_wallpaper(0x33);
    wait_for("B sees A's new wallpaper", 15.0, || {
        wallpaper_seen(&b.node, &a.node).is_some_and(|rev| rev != before)
    })
    .await;

    // Save-time diagnostics collection crosses the relay.
    let bundle = api(&b.node, |n| n.export_diagnostics()).await.unwrap();
    assert_eq!(bundle.included_device_count, Some(3));
    assert_eq!(bundle.cutoff_complete, Some(true));
    assert!(matches!(path_to(&a.node, &b.node), Some(LinkPath::Relayed { .. })));
    shutdown(vec![a, b, r]).await;
}

// ───────────── 2. a single-device group pairs into a group ─────────────

#[test]
fn single_device_pairs_into_existing_group_in_both_directions() {
    common::watchdog::run(
        module_path!(),
        "single_device_pairs_into_existing_group_in_both_directions",
        Some(4),
        single_device_pairs_into_existing_group_in_both_directions_scenario(),
    );
}

async fn single_device_pairs_into_existing_group_in_both_directions_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    common::pair(&a.node, &b.node).await;
    let original = group_anchor(&a).await;
    assert_eq!(group_anchor(&b).await, original);

    // The single device dials a member of the group.
    let c = start(root.path(), "gamma").await;
    let c_alone = group_anchor(&c).await;
    assert_ne!(c_alone, original);
    common::pair(&c.node, &a.node).await;
    // A group member initiates a directional merge into the singleton target.
    let d = start(root.path(), "delta").await;
    let destination = group_anchor(&d).await;
    common::pair(&b.node, &d.node).await;

    let all = [&a, &b, &c, &d];
    wait_for("four members everywhere", 20.0, || {
        all.iter().all(|dev| member_ids(&dev.node) == ids(&all))
    })
    .await;
    // Loopback fixture: device facts advertise the machine's LAN addresses, which the
    // 127.0.0.1-bound test endpoints do not answer, and discovery is off. Give
    // each remaining pair its actual loopback address before checking the mesh.
    for (source, target) in [(&b, &c), (&a, &d), (&c, &d)] {
        assert_eq!(
            common::query(&source.node, &target.node).await.peer.device.0,
            target.node.id()
        );
    }
    active(&all, 20.0, "four members").await;
    for dev in all {
        assert_eq!(group_anchor(dev).await, destination, "{} target group anchor", dev.name);
    }
    wait_for("one layout with four screens", 15.0, || {
        let first = a.node.layout().0;
        first.screens.len() == 4 && all.iter().all(|dev| dev.node.layout().0 == first)
    })
    .await;
    // The new members talk to the member they did not pair with.
    row(&[&c, &b, &a, &d]).await;
    cross(&c, &b, 1.0).await;
    type_and_check(&c, &b, 10, "new member drives old member").await;
    clipboard_reaches(&d, &c, "between the two new members").await;
    shutdown(vec![a, b, c, d]).await;
}

// ───────────── 3. leave and remove ─────────────

#[test]
fn leave_and_remove_give_only_the_departed_a_new_group() {
    common::watchdog::run(
        module_path!(),
        "leave_and_remove_give_only_the_departed_a_new_group",
        Some(4),
        leave_and_remove_give_only_the_departed_a_new_group_scenario(),
    );
}

async fn leave_and_remove_give_only_the_departed_a_new_group_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    let c = start(root.path(), "gamma").await;
    common::pair(&a.node, &b.node).await;
    let first_pairing = Instant::now();
    common::pair(&a.node, &c.node).await;
    common::active(&[&a.node, &b.node, &c.node]).await;
    wait_for("three members everywhere", 15.0, || {
        [&a, &b, &c].iter().all(|d| member_ids(&d.node) == ids(&[&a, &b, &c]))
    })
    .await;
    let original = group_anchor(&a).await;
    let old_id = group_id(&a.node);

    // B leaves by itself.
    api(&b.node, |n| n.leave_group()).await.unwrap();
    wait_all(&[&a, &b, &c], "B alone, A and C without B", 15.0, || {
        member_ids(&b.node) == vec![b.node.id()]
            && member_ids(&a.node) == ids(&[&a, &c])
            && member_ids(&c.node) == ids(&[&a, &c])
    })
    .await;
    let b_new = group_anchor(&b).await;
    assert_ne!(b_new.0, original.0, "the leaver has a new group id");
    assert_ne!(b_new.1, original.1, "the leaver has a new signed group root");
    assert_eq!(group_anchor(&a).await, original);
    assert_eq!(group_anchor(&c).await, original);
    assert!(b.node.layout().0.screens.iter().all(|s| s.id.device == b.node.id()));
    common::active(&[&a.node, &c.node]).await;

    // A removes C: C forms a new group, A is the last device and keeps id and key.
    let target = c.node.id();
    api(&a.node, move |n| n.remove_member(target.into())).await.unwrap();
    wait_all(&[&a, &b, &c], "C removed and alone", 15.0, || {
        member_ids(&a.node) == vec![a.node.id()] && member_ids(&c.node) == vec![c.node.id()]
    })
    .await;
    wait_for("C is in a new group", 15.0, || group_id(&c.node) != old_id).await;
    let c_new = group_anchor(&c).await;
    assert_ne!(c_new.1, original.1);
    assert_ne!(c_new, b_new);
    assert_eq!(
        group_anchor(&a).await,
        original,
        "the last device keeps group id and key"
    );
    assert!(
        api(&a.node, |n| n.leave_group()).await.is_err(),
        "a group of one cannot be left"
    );
    // Nobody reconnects on its own afterwards.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    for dev in [&a, &b, &c] {
        assert_eq!(member_ids(&dev.node), vec![dev.node.id()], "{}", dev.name);
    }
    assert_eq!(group_anchor(&a).await, original);
    // The departed can pair again (a pair request to the same peer is accepted once
    // per 10 s, a kept value; the first pairing of this test is long enough ago by now).
    tokio::time::sleep(Duration::from_secs(10).saturating_sub(first_pairing.elapsed()) + Duration::from_millis(300))
        .await;
    common::pair(&b.node, &a.node).await;
    assert_eq!(group_anchor(&a).await, group_anchor(&b).await);
    shutdown(vec![a, b, c]).await;
}

// ───────────── 4. link drop and reconnect ─────────────

#[test]
fn link_loss_waits_for_native_local_mode_after_focus_returns() {
    common::watchdog::run(
        module_path!(),
        "link_loss_waits_for_native_local_mode_after_focus_returns",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let a = start(root.path(), "alpha").await;
            let b = start(root.path(), "beta").await;
            common::pair(&a.node, &b.node).await;
            row(&[&a, &b]).await;
            cross(&a, &b, 1.0).await;
            wait_for("A captures remotely", 5.0, || {
                matches!(a.input.mode(), Some(CaptureMode::Remote { .. }))
            })
            .await;
            a.input.set_deferred_modes(true);
            let link = a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link;
            a.node
                .links()
                .unwrap()
                .disconnect(link, Disconnect::ControlStreamFailed);
            wait_for("focus returned with native restore still queued", 5.0, || {
                holder(&a.node) == Some(a.node.id())
                    && a.input.pending_modes().iter().any(|mode| {
                        matches!(
                            mode,
                            PendingMode::RestoreLocal(_) | PendingMode::Set(CaptureMode::Local)
                        )
                    })
            })
            .await;
            let mode_at_focus_return = a.input.mode();
            let early = {
                let mut ready = std::pin::pin!(wait_for("A's native pointer is local", 5.0, || {
                    local_pointer_ready(&a)
                }));
                let early = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(std::future::Future::poll(ready.as_mut(), cx).is_ready())
                })
                .await;
                // Release the native gate before asserting, including on regression.
                a.input.set_deferred_modes(false);
                if !early {
                    ready.await;
                }
                early
            };
            assert!(matches!(mode_at_focus_return, Some(CaptureMode::Remote { .. })));
            assert!(!early, "local readiness must wait for native restoration");
            shutdown(vec![a, b]).await;
        },
    );
}

#[test]
fn link_drop_releases_keys_and_every_module_stream_comes_back() {
    common::watchdog::run(
        module_path!(),
        "link_drop_releases_keys_and_every_module_stream_comes_back",
        Some(4),
        link_drop_releases_keys_and_every_module_stream_comes_back_scenario(),
    );
}

async fn link_drop_releases_keys_and_every_module_stream_comes_back_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    common::pair(&a.node, &b.node).await;
    row(&[&a, &b]).await;
    wait_for("audio capabilities exchanged", 10.0, || {
        a.node
            .audio_status()
            .unwrap()
            .devices
            .iter()
            .any(|d| d.device == b.node.id())
            && b.node
                .audio_status()
                .unwrap()
                .devices
                .iter()
                .any(|d| d.device == a.node.id())
    })
    .await;
    wait_for("initial wallpaper", 10.0, || wallpaper_seen(&b.node, &a.node).is_some()).await;
    clipboard_reaches(&a, &b, "before the drop").await;

    for round in 0..3 {
        cross(&a, &b, 1.0).await;
        type_and_check(&a, &b, 5, "before drop").await;
        // A holds a key and the left button on B when the link drops.
        key(&a.input, 0x50, true);
        wait_for("held key on B", 5.0, || keys(&b.input).last() == Some(&(0x50, true))).await;
        let old_link = a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link;
        let old_input = common::input_id(&a.node, b.node.id()).unwrap();
        // Either side's link layer loses the link in turn.
        let dropper = if round % 2 == 0 { &a } else { &b };
        let peer = if round % 2 == 0 { &b } else { &a };
        let link = dropper
            .node
            .links()
            .unwrap()
            .links()
            .by_peer(peer.node.id())
            .unwrap()
            .link;
        dropper
            .node
            .links()
            .unwrap()
            .disconnect(link, Disconnect::ControlStreamFailed);
        wait_for("B releases the held key", 5.0, || {
            keys(&b.input).last() == Some(&(0x50, false))
        })
        .await;
        wait_for("focus and native pointer return to each device", 5.0, || {
            local_pointer_ready(&a) && local_pointer_ready(&b)
        })
        .await;
        key(&a.input, 0x50, false);

        // Reconnect without any user action.
        wait_for("new link and attached input pair", 15.0, || {
            a.node
                .links()
                .unwrap()
                .links()
                .by_peer(b.node.id())
                .is_some_and(|l| l.link != old_link)
                && common::input_id(&a.node, b.node.id()).is_some_and(|id| id != old_input)
                && common::input_id(&b.node, a.node.id()).is_some()
        })
        .await;
        common::active(&[&a.node, &b.node]).await;
        let released = keys(&b.input).len();
        cross(&a, &b, 1.0).await;
        type_and_check(&a, &b, 20, "after reconnect").await;
        assert_eq!(
            keys(&b.input)[..released]
                .iter()
                .filter(|k| **k == (0x50, false))
                .count(),
            round + 1,
            "one release per drop (round {round}); injected on B: {:?}",
            b.input
                .injected()
                .iter()
                .filter(|e| !matches!(e, InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. }))
                .collect::<Vec<_>>()
        );
        // Bring the pointer home for the next round.
        for _ in 0..400 {
            if local_pointer_ready(&a) {
                break;
            }
            if holder(&a.node) != Some(a.node.id()) {
                os_move(&a.input, -20.0);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(local_pointer_ready(&a), "pointer came back to A in native Local mode");

        // Clipboard both ways, audio control, wallpaper, diagnostics.
        clipboard_reaches(&a, &b, &format!("a to b after drop {round}")).await;
        clipboard_reaches(&b, &a, &format!("b to a after drop {round}")).await;
        wait_for("audio devices after reconnect", 10.0, || {
            a.node
                .audio_status()
                .unwrap()
                .devices
                .iter()
                .any(|d| d.device == b.node.id())
                && b.node
                    .audio_status()
                    .unwrap()
                    .devices
                    .iter()
                    .any(|d| d.device == a.node.id())
        })
        .await;
        let before = wallpaper_seen(&b.node, &a.node);
        a.displays.set_wallpaper(0x40 + round as u8);
        wait_for("wallpaper change after reconnect", 10.0, || {
            wallpaper_seen(&b.node, &a.node) != before
        })
        .await;
        for dev in [&a, &b] {
            key(&dev.input, 0x2C, true);
            key(&dev.input, 0x2C, false);
            let bundle = api(&dev.node, |n| n.export_diagnostics()).await.unwrap();
            assert_eq!(bundle.included_device_count, Some(2));
            assert_eq!(bundle.cutoff_complete, Some(true));
        }
    }
    shutdown(vec![a, b]).await;
}

#[test]
fn file_transfer_interrupted_by_link_drop_completes_after_reconnect() {
    common::watchdog::run(
        module_path!(),
        "file_transfer_interrupted_by_link_drop_completes_after_reconnect",
        Some(4),
        file_transfer_interrupted_by_link_drop_completes_after_reconnect_scenario(),
    );
}

async fn file_transfer_interrupted_by_link_drop_completes_after_reconnect_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    common::pair(&a.node, &b.node).await;
    let source = a.dir.join("source.bin");
    let bytes: Vec<u8> = (0..24_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(&source, &bytes).unwrap();
    let files = vec![oink_clipboard::FileEntry {
        path: "big.bin".into(),
        size: bytes.len() as u64,
        is_dir: false,
        modified_ms: None,
        local_path: Some(source.clone()),
    }];
    const VERIFIED: u64 = 1024 * 1024;
    let (gate, entered) = a.clip.gate_file_reads_after(VERIFIED);
    a.clip.copy(vec![ClipData::Files(files)], Sensitivity::Normal);
    wait_for("file demand available", 10.0, || {
        b.node
            .clipboard_transfer()
            .unwrap()
            .is_some_and(|p| p.phase == FileTransferPhase::Pending)
    })
    .await;
    b.node.fetch_clipboard_files().unwrap();
    let stopped_at = tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    assert_eq!(stopped_at, VERIFIED, "source pauses before its second payload block");
    wait_for("receiver verified the first payload block", 10.0, || {
        b.node
            .clipboard_transfer()
            .unwrap()
            .is_some_and(|progress| progress.done >= VERIFIED && progress.done < bytes.len() as u64)
    })
    .await;
    let links = b.node.links().unwrap();
    let source_device = a.node.id();
    let interrupted = links.links().by_peer(source_device).unwrap().link;
    links.disconnect(interrupted, Disconnect::ControlStreamFailed);
    wait_for("old link retires while the payload reader is blocked", 10.0, || {
        links
            .links()
            .by_peer(source_device)
            .is_none_or(|link| link.link != interrupted)
    })
    .await;
    gate.release();
    let clip = b.clip.clone();
    let end = Instant::now() + Duration::from_secs(40);
    let received = loop {
        let path = clip.items().into_iter().find_map(|d| match d {
            ClipData::Files(f) => f[0].local_path.clone().filter(|p| p != &source && p.is_file()),
            _ => None,
        });
        if let Some(path) = path {
            break path;
        }
        assert!(
            Instant::now() < end,
            "file never arrived after the reconnect: transfer={:?}, link={:?}",
            b.node.clipboard_transfer().unwrap(),
            path_to(&b.node, &a.node)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        gate.opened_offsets().iter().any(|offset| *offset >= VERIFIED),
        "reconnect resumes a verified nonzero payload offset"
    );
    assert_ne!(links.links().by_peer(source_device).unwrap().link, interrupted);
    assert!(
        std::fs::read(received).unwrap() == bytes,
        "received file differs from the source"
    );
    shutdown(vec![a, b]).await;
}

// ───────────── 5. pause and resume on either side ─────────────

#[test]
fn pause_and_resume_from_either_side_restore_every_stream() {
    common::watchdog::run(
        module_path!(),
        "pause_and_resume_from_either_side_restore_every_stream",
        Some(4),
        pause_and_resume_from_either_side_restore_every_stream_scenario(),
    );
}

async fn pause_and_resume_from_either_side_restore_every_stream_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    common::pair(&a.node, &b.node).await;
    row(&[&a, &b]).await;
    wait_for("initial wallpaper", 10.0, || wallpaper_seen(&b.node, &a.node).is_some()).await;
    // (who pauses, who resumes)
    for (round, (pauser, resumer)) in [(&a, &a), (&a, &b), (&b, &a), (&b, &b)].into_iter().enumerate() {
        cross(&a, &b, 1.0).await;
        key(&a.input, 0x51, true);
        wait_for("held key on B", 5.0, || keys(&b.input).last() == Some(&(0x51, true))).await;
        let link = a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link;
        let old = [
            common::input_id(&a.node, b.node.id()).unwrap(),
            common::input_id(&b.node, a.node.id()).unwrap(),
        ];
        api(&pauser.node, move |n| {
            n.set_local_paused(format!("00000000-0000-4000-8000-{round:012x}"), true)
        })
        .await
        .unwrap();
        wait_for("only the pause owner is paused", 10.0, || {
            let status = pauser.node.pause_status();
            status.paused
                && !status.settling
                && [&a, &b]
                    .iter()
                    .filter(|device| device.node.id() != pauser.node.id())
                    .all(|device| !device.node.pause_status().paused)
        })
        .await;
        wait_for("held key released on pause", 5.0, || {
            keys(&b.input).last() == Some(&(0x51, false))
        })
        .await;
        key(&a.input, 0x51, false);
        wait_for("input attachment retired on both peers", 5.0, || {
            common::input_id(&a.node, b.node.id()).is_none() && common::input_id(&b.node, a.node.id()).is_none()
        })
        .await;
        // While paused nothing is forwarded and the clipboard does not sync.
        let before = keys(&b.input).len();
        key(&a.input, 0x53, true);
        key(&a.input, 0x53, false);
        b.clip
            .copy(vec![ClipData::Text(format!("b keeps {round}"))], Sensitivity::Normal);
        a.clip.copy(
            vec![ClipData::Text(format!("paused copy {round}"))],
            Sensitivity::Normal,
        );
        let stale_wallpaper = wallpaper_seen(&b.node, &a.node);
        a.displays.set_wallpaper(0x60 + round as u8);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(keys(&b.input).len(), before, "no keys while paused");
        assert_eq!(b.clip.items(), vec![ClipData::Text(format!("b keeps {round}"))]);
        if pauser.node.id() == a.node.id() {
            assert_eq!(wallpaper_seen(&b.node, &a.node), stale_wallpaper);
        } else {
            wait_for("paused receiver accepts wallpaper state", 10.0, || {
                wallpaper_seen(&b.node, &a.node) != stale_wallpaper
            })
            .await;
        }

        if resumer.node.id() == pauser.node.id() {
            api(&resumer.node, move |node| {
                node.set_local_paused(format!("00000000-0000-4000-9000-{round:012x}"), false)
            })
            .await
            .unwrap();
        } else {
            wait_for("remote ordinary pause can resume", 5., || {
                resumer.node.device_cards().unwrap().cards.iter().any(|card| {
                    card.device.0 == pauser.node.id()
                        && card
                            .pause
                            .as_ref()
                            .is_some_and(|pause| pause.paused && pause.can_resume)
                })
            })
            .await;
            let view = resumer.node.device_cards().unwrap();
            let pause = view
                .cards
                .iter()
                .find(|card| card.device.0 == pauser.node.id())
                .unwrap()
                .pause
                .as_ref()
                .unwrap();
            let oink_agent::device_cards::CardAction::Invoke { token, .. } = &pause.action else {
                panic!("missing resume action")
            };
            let token = token.clone();
            api(&resumer.node, move |node| {
                node.set_device_paused(format!("00000000-0000-4000-9000-{round:012x}"), token, false)
            })
            .await
            .unwrap();
        }
        common::active(&[&a.node, &b.node]).await;
        assert_eq!(
            a.node.links().unwrap().links().by_peer(b.node.id()).map(|l| l.link),
            Some(link),
            "pause keeps the link"
        );
        // Full pause closes both input directions. The authenticated link stays,
        // but neither endpoint may reuse a writer from the retired session.
        for (index, (writer, peer)) in [(&a, &b), (&b, &a)].into_iter().enumerate() {
            let resumed = common::input_id(&writer.node, peer.node.id()).unwrap();
            assert_eq!(resumed.link, old[index].link, "keep the authenticated link");
            assert!(
                resumed.tag.ordinal > old[index].tag.ordinal,
                "resume opens a fresh writer: {resumed:?} after {:?}",
                old[index]
            );
        }
        wait_for("wallpaper re-sent on resume", 10.0, || {
            wallpaper_seen(&b.node, &a.node) != stale_wallpaper
        })
        .await;
        cross(&a, &b, 1.0).await;
        type_and_check(&a, &b, 20, "after resume").await;
        for _ in 0..400 {
            if local_pointer_ready(&a) {
                break;
            }
            if holder(&a.node) != Some(a.node.id()) {
                os_move(&a.input, -20.0);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(local_pointer_ready(&a), "pointer came back to A in native Local mode");
        clipboard_reaches(&a, &b, &format!("a to b after resume {round}")).await;
        clipboard_reaches(&b, &a, &format!("b to a after resume {round}")).await;
    }
    shutdown(vec![a, b]).await;
}

// ───────────── 6. relay to direct switch during activity ─────────────

/// The ordinary relay-to-direct switch preserves the link, input handles and delivery.
#[test]
fn relay_to_direct_switch_during_typing_loses_nothing() {
    common::watchdog::run(
        module_path!(),
        "relay_to_direct_switch_during_typing_loses_nothing",
        Some(4),
        relay_to_direct_switch_during_typing_loses_nothing_scenario(),
    );
}

async fn relay_to_direct_switch_during_typing_loses_nothing_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, r) = relayed_trio(root.path()).await;
    row(&[&r, &a, &b]).await;
    cross(&a, &b, 1.0).await;
    let link = a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link;
    let input = common::input_id(&a.node, b.node.id()).unwrap();
    let reverse = common::input_id(&b.node, a.node.id()).unwrap();
    let already = keys(&b.input).len();
    let typing = {
        let input = a.input.clone();
        tokio::task::spawn_blocking(move || {
            let mut expected = Vec::new();
            // 200 key events a second for four seconds: brisk but ordinary input.
            // LINK_TEST_IDLE_SWITCH=1 measures the same switch with no input at all.
            let pairs = if std::env::var_os("LINK_TEST_IDLE_SWITCH").is_some() {
                0
            } else {
                400u32
            };
            for i in 0..pairs {
                let usage = 4 + (i % 26);
                key(&input, usage, true);
                key(&input, usage, false);
                expected.push((usage, true));
                expected.push((usage, false));
                std::thread::sleep(Duration::from_millis(10));
            }
            expected
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Persist manual loopback hints through the owner for either connection
    // coordinator, exercising their merge with ordinary periodic Facts. Raw
    // LinkLayer updates bypass the owner's retained-candidate policy.
    for (source, target) in [(&a, &b), (&b, &a)] {
        let peer = target.node.id().into();
        let address = format!("127.0.0.1:{}", target.node.network_info().unwrap().port);
        api(&source.node, move |node| node.set_member_address(peer, address))
            .await
            .unwrap();
    }
    oink_network::link::testing::set_direct_blocked(&b.node.links().unwrap(), a.node.id(), false);
    oink_network::link::testing::set_direct_blocked(&a.node.links().unwrap(), b.node.id(), false);
    let end = Instant::now() + Duration::from_secs(20);
    while !(path_to(&a.node, &b.node) == Some(LinkPath::Direct) && path_to(&b.node, &a.node) == Some(LinkPath::Direct))
    {
        let now = a.node.links().unwrap().links().by_peer(b.node.id()).map(|l| l.link);
        assert!(
            now == Some(link),
            "the switch to the direct path dropped the link ({link:?} became {now:?}); keys injected on B so far: {}",
            keys(&b.input).len() - already
        );
        assert!(
            Instant::now() < end,
            "fixture: the direct attempt never started: {:?} / {:?}",
            path_to(&a.node, &b.node),
            path_to(&b.node, &a.node)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let expected = typing.await.unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while keys(&b.input).len() < already + expected.len() && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let got = keys(&b.input)[already..].to_vec();
    let same_link = a.node.links().unwrap().links().by_peer(b.node.id()).map(|l| l.link) == Some(link);
    let same_stream = common::input_id(&a.node, b.node.id()) == Some(input);
    let same_reverse = common::input_id(&b.node, a.node.id()) == Some(reverse);
    assert!(
        got == expected && same_link && same_stream && same_reverse,
        "switch lost or reordered input: sent {} key events, injected {}, same link {same_link}, same input stream {same_stream} (B's stream to A the same: {}), first difference at {:?}, focus holder seen by A/B: {:?}/{:?} (B is {:?}), A capture mode {:?}{}",
        expected.len(),
        got.len(),
        same_reverse,
        got.iter().zip(&expected).position(|(g, e)| g != e),
        holder(&a.node),
        holder(&b.node),
        b.node.id(),
        a.input.mode(),
        dump(&[&a, &b])
    );
    clipboard_reaches(&a, &b, "after the switch").await;
    shutdown(vec![a, b, r]).await;
}

// ───────────── 7. restart of one backend ─────────────

#[test]
fn restarted_backend_rejoins_and_every_module_works() {
    common::watchdog::run(
        module_path!(),
        "restarted_backend_rejoins_and_every_module_works",
        Some(4),
        restarted_backend_rejoins_and_every_module_works_scenario(),
    );
}

async fn restarted_backend_rejoins_and_every_module_works_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    common::pair(&a.node, &b.node).await;
    row(&[&a, &b]).await;
    let original = group_anchor(&a).await;
    let mut b = b;
    for round in 0..2 {
        cross(&a, &b, 1.0).await;
        key(&a.input, 0x52, true);
        wait_for("held key on B", 5.0, || keys(&b.input).last() == Some(&(0x52, true))).await;
        // B's backend goes away while A drives it.
        b.node.shutdown().await;
        wait_for("A takes focus and native pointer back", 8.0, || local_pointer_ready(&a)).await;
        key(&a.input, 0x52, false);
        // A changes shared state while B is away.
        api(&a.node, move |n| {
            n.rename_device(n.id().into(), format!("renamed {round}"))
        })
        .await
        .unwrap();
        a.clip.copy(
            vec![ClipData::Text(format!("copied while away {round}"))],
            Sensitivity::Normal,
        );
        b = restart(root.path(), &b).await;
        common::active(&[&a.node, &b.node]).await;
        assert!(
            b.node.pairings().unwrap().requests.is_empty(),
            "no pairing after a restart"
        );
        assert_eq!(group_anchor(&b).await, original);
        wait_for("B has the state changed while it was away", 10.0, || {
            b.node.device_name(a.node.id()) == format!("renamed {round}") && a.node.layout().0 == b.node.layout().0
        })
        .await;
        cross(&a, &b, 1.0).await;
        type_and_check(&a, &b, 20, "after restart").await;
        for _ in 0..400 {
            if local_pointer_ready(&a) {
                break;
            }
            if holder(&a.node) != Some(a.node.id()) {
                os_move(&a.input, -20.0);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(local_pointer_ready(&a), "pointer came back to A in native Local mode");
        // And the restarted side can drive the resident one.
        cross(&b, &a, -1.0).await;
        type_and_check(&b, &a, 10, "restarted device drives the resident one").await;
        for _ in 0..400 {
            if local_pointer_ready(&b) {
                break;
            }
            if holder(&b.node) != Some(b.node.id()) {
                os_move(&b.input, 20.0);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(local_pointer_ready(&b), "pointer came back to B in native Local mode");
        clipboard_reaches(&a, &b, &format!("a to b after restart {round}")).await;
        clipboard_reaches(&b, &a, &format!("b to a after restart {round}")).await;
        wait_for("wallpapers after restart", 10.0, || {
            wallpaper_seen(&b.node, &a.node).is_some() && wallpaper_seen(&a.node, &b.node).is_some()
        })
        .await;
        wait_for("audio devices after restart", 10.0, || {
            a.node
                .audio_status()
                .unwrap()
                .devices
                .iter()
                .any(|d| d.device == b.node.id())
                && b.node
                    .audio_status()
                    .unwrap()
                    .devices
                    .iter()
                    .any(|d| d.device == a.node.id())
        })
        .await;
    }
    shutdown(vec![a, b]).await;
}

// ───────────── 8. group state file unreadable ─────────────

#[test]
fn unreadable_group_state_of_a_member_then_reset_and_pair_again() {
    common::watchdog::run(
        module_path!(),
        "unreadable_group_state_of_a_member_then_reset_and_pair_again",
        Some(4),
        unreadable_group_state_of_a_member_then_reset_and_pair_again_scenario(),
    );
}

async fn unreadable_group_state_of_a_member_then_reset_and_pair_again_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    common::pair(&a.node, &b.node).await;
    let first_pairing = Instant::now();
    let original = group_anchor(&a).await;
    b.node.shutdown().await;
    let bad = b"{ \"format\": 1, \"device\": ";
    std::fs::write(b.dir.join("group-state.json"), bad).unwrap();
    // Unreadable state leaves networking off, so the old port is not owned
    // across this interval. Another test process can legitimately acquire it.
    let mut net = oink_test_support::network_config(b.dir.clone(), &b.name, b.port);
    // Ask the kernel at the actual reset bind, rather than probing a free port
    // here and assuming it stays free while the user inspects unreadable state.
    net.port = 0;
    #[cfg(windows)]
    {
        // Windows allocates ephemeral UDP ports consecutively. Isolate this
        // delayed bind from other processes' QUIC/discovery neighbours too.
        // This scenario is the only user of this loopback address in its process.
        let pid = std::process::id().to_be_bytes();
        net.bind_ip = std::net::Ipv4Addr::new(127, pid[1], pid[2], pid[3]).into();
    }
    let b = start_config(root.path(), &b.name, net).await;
    // Sharing does not start and the file is left alone.
    assert!(b.node.links().is_err());
    assert!(b.node.overview().unwrap().group.is_none());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(std::fs::read(b.dir.join("group-state.json")).unwrap(), bad);
    assert_eq!(
        member_ids(&a.node),
        ids(&[&a, &b]),
        "A still lists the unreachable member"
    );
    assert_eq!(group_anchor(&a).await, original);

    api(&b.node, |n| n.reset_group_state()).await.unwrap();
    wait_for("B is a new group of one", 10.0, || {
        b.node.group().unwrap().id.is_some() && member_ids(&b.node) == vec![b.node.id()]
    })
    .await;
    assert_ne!(group_anchor(&b).await, original);
    // A learns that B is no longer in its group the next time they talk directly;
    // A stays in its group with the same id and key. (One pair request per peer per
    // 10 s is a kept value.)
    tokio::time::sleep(Duration::from_secs(10).saturating_sub(first_pairing.elapsed()) + Duration::from_millis(300))
        .await;
    // The reset member initiates: A may still be dialling its retired port.
    common::pair(&b.node, &a.node).await;
    assert_eq!(group_anchor(&a).await, original);
    assert_eq!(group_anchor(&b).await, original);
    assert_eq!(member_ids(&a.node), ids(&[&a, &b]));
    row(&[&a, &b]).await;
    cross(&a, &b, 1.0).await;
    type_and_check(&a, &b, 10, "after reset and pairing").await;
    shutdown(vec![a, b]).await;
}

// ───────────── 10. a shared-state edit made while the peer is away ─────────────

/// The Windows gate failure `snapshot_tests::restart_adopts_resident_layout_and_current_displays`
/// ("timed out: resident layout adopted"), with the state printed.
#[test]
fn layout_edit_made_while_peer_is_away_reaches_it_after_restart() {
    common::watchdog::run(
        module_path!(),
        "layout_edit_made_while_peer_is_away_reaches_it_after_restart",
        Some(4),
        layout_edit_made_while_peer_is_away_reaches_it_after_restart_scenario(),
    );
}

async fn layout_edit_made_while_peer_is_away_reaches_it_after_restart_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    common::pair(&a.node, &b.node).await;
    wait_for("both screens in one layout", 15.0, || {
        a.node.layout().0.screens.len() == 2 && a.node.layout().0 == b.node.layout().0
    })
    .await;
    let mut b = b;
    for round in 0..3 {
        let before = (a.node.layout(), b.node.layout());
        b.node.shutdown().await;
        a.node
            .link_config()
            .unwrap()
            .edit_layout(
                Box::new(|l| l.settings.gap_travel = !l.settings.gap_travel),
                oink_protocol::v1::dto::LayoutSource::Manual,
            )
            .unwrap();
        let expected = a.node.layout();
        b = restart(root.path(), &b).await;
        let end = Instant::now() + Duration::from_secs(15);
        while !(a.node.layout().0 == expected.0 && b.node.layout().0 == expected.0) {
            assert!(
                Instant::now() < end,
                "round {round}: the edit made while B was away did not survive B's return.\n before: A {:?} gap={} / B {:?} gap={}\n edited: A {:?} gap={}\n now:    A {:?} gap={} screens={} / B {:?} gap={} screens={}\n A==expected {} B==expected {}{}",
                before.0.1,
                before.0.0.settings.gap_travel,
                before.1.1,
                before.1.0.settings.gap_travel,
                expected.1,
                expected.0.settings.gap_travel,
                a.node.layout().1,
                a.node.layout().0.settings.gap_travel,
                a.node.layout().0.screens.len(),
                b.node.layout().1,
                b.node.layout().0.settings.gap_travel,
                b.node.layout().0.screens.len(),
                a.node.layout().0 == expected.0,
                b.node.layout().0 == expected.0,
                dump(&[&a, &b])
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        active(&[&a, &b], 15.0, "after restart").await;
    }
    shutdown(vec![a, b]).await;
}

// ───────────── 11. Pairing and membership items without a real-stack test ─────────────

/// A source group adopts the explicitly selected target group.
#[test]
fn two_single_devices_keep_the_target_group() {
    common::watchdog::run(
        module_path!(),
        "two_single_devices_keep_the_target_group",
        Some(4),
        two_single_devices_keep_the_target_group_scenario(),
    );
}

async fn two_single_devices_keep_the_target_group_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    let (a_before, b_before) = (group_anchor(&a).await, group_anchor(&b).await);
    assert_ne!(a_before, b_before);
    assert_eq!(a.node.group().unwrap().members.len(), 1);
    common::pair(&a.node, &b.node).await;
    assert_eq!(group_anchor(&a).await, b_before, "the source adopts the target anchor");
    assert_eq!(group_anchor(&b).await, b_before);
    // Both screens are in the shared layout and can be crossed without any further step.
    row(&[&a, &b]).await;
    cross(&a, &b, 1.0).await;
    type_and_check(&a, &b, 5, "after pairing two single devices").await;
    shutdown(vec![a, b]).await;
}

/// Both users start pairing towards each other at the same moment: one link remains,
/// one side shows the code, the other enters it, and pairing completes.
#[test]
fn both_users_starting_pairing_at_once_still_pair() {
    common::watchdog::run(
        module_path!(),
        "both_users_starting_pairing_at_once_still_pair",
        Some(4),
        both_users_starting_pairing_at_once_still_pair_scenario(),
    );
}

async fn both_users_starting_pairing_at_once_still_pair_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    let (first, second) = tokio::join!(common::request(&a.node, &b.node), common::request(&b.node, &a.node));
    assert_ne!(first.0, second.0, "opposite requests remain independent sessions");
    let (first_result, second_result) = tokio::join!(
        common::submit(&b.node, first.0.clone(), 1, first.1),
        common::submit(&a.node, second.0.clone(), 1, second.1)
    );
    assert!(matches!(first_result, oink_agent::api::pairing::PairReply::Value(_)));
    assert!(matches!(second_result, oink_agent::api::pairing::PairReply::Value(_)));
    wait_for("both in one group", 15.0, || {
        a.node.group_id() == b.node.group_id()
            && member_ids(&a.node) == ids(&[&a, &b])
            && member_ids(&b.node) == ids(&[&a, &b])
    })
    .await;
    active(&[&a, &b], 15.0, "after crossed pairing").await;
    assert_eq!(group_anchor(&a).await, group_anchor(&b).await);
    assert_eq!(a.node.links().unwrap().links().links.len(), 1);
    assert_eq!(b.node.links().unwrap().links().links.len(), 1);
    shutdown(vec![a, b]).await;
}

/// A member is removed while it is switched off. When it comes back it learns of the
/// removal in the handshake and forms a new group; the remaining device keeps its group.
#[test]
fn member_removed_while_away_forms_its_own_group_when_it_returns() {
    common::watchdog::run(
        module_path!(),
        "member_removed_while_away_forms_its_own_group_when_it_returns",
        Some(4),
        member_removed_while_away_forms_its_own_group_when_it_returns_scenario(),
    );
}

async fn member_removed_while_away_forms_its_own_group_when_it_returns_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    let c = start(root.path(), "gamma").await;
    common::pair(&a.node, &b.node).await;
    common::pair(&a.node, &c.node).await;
    wait_for("three members everywhere", 15.0, || {
        [&a, &b, &c].iter().all(|d| member_ids(&d.node) == ids(&[&a, &b, &c]))
    })
    .await;
    common::active(&[&a.node, &b.node, &c.node]).await;
    let original = group_anchor(&a).await;
    let old_id = group_id(&a.node);
    c.node.shutdown().await;
    let target = c.node.id();
    api(&a.node, move |n| n.remove_member(target.into())).await.unwrap();
    wait_all(&[&a, &b, &c], "A and B without C", 10.0, || {
        member_ids(&a.node) == ids(&[&a, &b]) && member_ids(&b.node) == ids(&[&a, &b])
    })
    .await;
    let c = restart(root.path(), &c).await;
    wait_for("C alone in a new group", 15.0, || {
        member_ids(&c.node) == vec![c.node.id()] && group_id(&c.node) != old_id
    })
    .await;
    assert_ne!(group_anchor(&c).await.1, original.1);
    assert!(c.node.layout().0.screens.iter().all(|s| s.id.device == c.node.id()));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(member_ids(&a.node), ids(&[&a, &b]));
    assert_eq!(member_ids(&b.node), ids(&[&a, &b]));
    assert_eq!(group_anchor(&a).await, original);
    assert_eq!(group_anchor(&b).await, original);
    active(&[&a, &b], 10.0, "the remaining pair").await;
    shutdown(vec![a, b, c]).await;
}

/// The device that removes a member also takes that member's screens
/// out of the shared layout; a member that leaves is taken out the same way by the others.
#[test]
fn screens_of_a_removed_or_departed_member_leave_the_shared_layout() {
    common::watchdog::run(
        module_path!(),
        "screens_of_a_removed_or_departed_member_leave_the_shared_layout",
        Some(4),
        screens_of_a_removed_or_departed_member_leave_the_shared_layout_scenario(),
    );
}

async fn screens_of_a_removed_or_departed_member_leave_the_shared_layout_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    let c = start(root.path(), "gamma").await;
    common::pair(&a.node, &b.node).await;
    common::pair(&a.node, &c.node).await;
    wait_for("three screens in every layout", 15.0, || {
        [&a, &b, &c]
            .iter()
            .all(|d| AgentApi::layout(&d.node).unwrap().geometry.screens.len() == 3)
    })
    .await;
    let shown = |d: &Dev, of: DeviceId| {
        AgentApi::layout(&d.node)
            .unwrap()
            .geometry
            .screens
            .iter()
            .filter(|s| s.key.device.0 == of)
            .count()
    };
    common::active(&[&a.node, &b.node, &c.node]).await;
    let (b_id, c_id) = (b.node.id(), c.node.id());
    // Audio claim membership retirement runs in audio::link's fake-platform
    // owner test. This real-Node fixture never selects a native audio output.
    api(&a.node, move |n| n.remove_member(c_id.into())).await.unwrap();
    wait_for("C removed", 10.0, || {
        member_ids(&a.node) == ids(&[&a, &b]) && member_ids(&b.node) == ids(&[&a, &b])
    })
    .await;
    wait_for("C screens removed", 10.0, || {
        [&a, &b].iter().all(|d| shown(d, c_id) == 0)
    })
    .await;
    let after_remove = (shown(&a, c_id), shown(&b, c_id));
    let screen = AgentApi::layout(&b.node)
        .unwrap()
        .geometry
        .screens
        .into_iter()
        .find(|screen| screen.key.device.0 == b_id)
        .unwrap()
        .key;
    api(&b.node, move |node| node.set_screen_gain(screen, 1.75))
        .await
        .unwrap();
    let mut own_screens = b
        .node
        .layout()
        .0
        .screens
        .into_iter()
        .filter(|s| s.id.device == b_id)
        .collect::<Vec<_>>();
    common::active(&[&a.node, &b.node]).await;
    let prior_group = b.node.group_id();
    api(&b.node, |n| n.leave_group()).await.unwrap();
    wait_for("B independent projection", 10.0, || {
        b.node.group_id() != prior_group
            && member_ids(&b.node) == vec![b_id]
            && b.node.layout().0.screens.iter().any(|screen| screen.id.device == b_id)
    })
    .await;
    wait_all(&[&a, &b, &c], "B left", 10.0, || {
        member_ids(&a.node) == vec![a.node.id()]
    })
    .await;
    wait_for("B screens removed", 10.0, || shown(&a, b_id) == 0).await;
    // A fresh singleton group has its own origin; dimensions and local display
    // metadata survive while the old group's translation is no longer retained.
    let origin = (own_screens[0].rect.x, own_screens[0].rect.y);
    for screen in &mut own_screens {
        screen.rect.x -= origin.0;
        screen.rect.y -= origin.1;
    }
    assert_eq!(b.node.layout().0.screens, own_screens);
    let after_leave = (shown(&a, b_id), shown(&a, c_id));
    let members = a.node.group().unwrap().members.len();
    shutdown(vec![a, b, c]).await;
    assert_eq!(
        (after_remove, after_leave),
        ((0, 0), (0, 0)),
        "screens still shown in the layout: of removed C on (A, B) = {after_remove:?}; later on A of (departed B, removed C) = {after_leave:?}; A's member count is {members}"
    );
}

/// The diagnostics bundle does not contain the group state file, and the
/// excerpt in it does not contain the identity seed.
#[test]
fn diagnostics_bundle_excludes_authority_files_and_raw_identities() {
    common::watchdog::run(
        module_path!(),
        "diagnostics_bundle_excludes_authority_files_and_raw_identities",
        Some(4),
        diagnostics_bundle_excludes_authority_files_and_raw_identities_scenario(),
    );
}

async fn diagnostics_bundle_excludes_authority_files_and_raw_identities_scenario() {
    use std::io::Read;
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    wait_for("diagnostic recovery", 20.0, || {
        [&a, &b]
            .iter()
            .all(|d| !d.node.diagnostics_status().unwrap().recovering)
    })
    .await;
    common::pair(&a.node, &b.node).await;
    let key = std::fs::read(a.dir.join("device.key")).unwrap();
    assert_eq!(key.len(), 32);
    let bundle = api(&a.node, |n| n.export_diagnostics()).await.unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    let mut names = Vec::new();
    let mut all = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).unwrap();
        names.push(entry.name().unwrap().to_string());
        entry.read_to_end(&mut all).unwrap();
        all.push(b'\n');
    }
    assert!(
        !names.iter().any(|n| ["group-state", "settings", "device.key", "owner"]
            .iter()
            .any(|part| n.contains(part))),
        "group state file in the bundle: {names:?}"
    );
    let text = String::from_utf8_lossy(&all).replace([' ', '\n', '\r', '\t'], "");
    let decimal = key.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",");
    let hex = key.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let base64 = {
        use base64::Engine;
        [
            base64::engine::general_purpose::STANDARD.encode(&key),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&key),
        ]
    };
    assert!(
        text.contains("group_state") || text.contains("members"),
        "the excerpt is in the bundle"
    );
    assert!(!text.contains(&decimal), "identity seed bytes in the bundle");
    assert!(!text.to_lowercase().contains(&hex), "identity seed (hex) in the bundle");
    assert!(
        !base64.iter().any(|k| text.contains(k.trim_end_matches('='))),
        "identity seed (base64) in the bundle"
    );
    assert!(
        !all.windows(32).any(|w| w == key.as_slice()),
        "raw identity seed in the bundle"
    );
    for device in [&a, &b] {
        assert!(
            !text.contains(&device.node.id().to_hex()),
            "public identity is not an alias"
        );
        assert!(
            !all.windows(32).any(|w| w == device.node.id().0),
            "raw public identity leaked"
        );
        assert!(
            !text.contains(device.dir.to_str().unwrap()),
            "private state path leaked"
        );
    }
    shutdown(vec![a, b]).await;
}

// ───────────── bugs found by the scenarios above, reduced ─────────────

/// link-test BUG-1. A device that is still in this device's member list is unreachable at
/// its remembered address (here: it came back on another port as a fresh group of one, as
/// after a reinstall with a changed address). The user enters the new address. While the
/// background attempt to the old address is still in flight (up to 6 s of every 7 s), the
/// manual connect is answered with that attempt's "device unreachable", although the
/// address just answered the discovery query.
// Extreme/boundary case; reproduction kept, deliberately NOT handled (user decision 2026-10-11).
// When a listed member has an obsolete address and has formed a new group, a manual connect can report the old background dial failure.

// Extreme/boundary case; reproduction kept, deliberately NOT handled (user decision 2026-10-11).
// When nine offline members compete for eight dial slots, a user connect waits behind background redials.

#[test]
fn two_offline_members_leave_dial_capacity_for_a_user_started_connect() {
    common::watchdog::run(
        module_path!(),
        "two_offline_members_leave_dial_capacity_for_a_user_started_connect",
        Some(4),
        two_offline_members_leave_dial_capacity_for_a_user_started_connect_scenario(),
    );
}

async fn two_offline_members_leave_dial_capacity_for_a_user_started_connect_scenario() {
    connect_amid_offline_member_redials(2).await;
}

async fn connect_amid_offline_member_redials(offline_count: usize) {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let mut offline = Vec::new();
    for index in 0..offline_count {
        let dev = start(root.path(), &format!("offline-{index}")).await;
        common::pair(&a.node, &dev.node).await;
        offline.push(dev);
    }
    wait_for("all members joined", 15.0, || {
        member_ids(&a.node).len() == offline_count + 1
    })
    .await;
    for dev in &offline {
        dev.node.shutdown().await;
    }
    wait_for("source alone online", 10.0, || {
        a.node.links().unwrap().links().links.is_empty()
    })
    .await;
    let d = start(root.path(), "delta").await;
    let started = Instant::now();
    let (pair, _) = common::request(&a.node, &d.node).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "manual prepared query must retain dial capacity"
    );
    let revision = a
        .node
        .pairings()
        .unwrap()
        .sessions
        .iter()
        .find(|s| s.id == pair)
        .unwrap()
        .revision;
    let cancelled = a.node.cancel_pairing(pair, revision).unwrap();
    assert!(
        matches!(cancelled, oink_agent::api::pairing::PairReply::Value(_)),
        "{cancelled:?}"
    );
    shutdown(vec![a, d]).await;
}

/// link-test BUG-5. A pair is linked through a relay; the addresses in the peer's device
/// facts do not answer (here: the machine's LAN addresses, while the test endpoints listen
/// on 127.0.0.1 only; in the field: a peer behind address translation). The user enters an
/// address that does answer. The link layer's address list for the direct attempt is
/// replaced by each source in turn, the next device facts put the unreachable addresses
/// back, and the link stays on the relay.
// Merged candidates and concurrent dialing preserve a manual reachable hint even
// when newer peer facts advertise only unreachable addresses (2026-10-12).
#[test]
fn manually_entered_address_moves_a_relayed_link_to_the_direct_path() {
    common::watchdog::run(
        module_path!(),
        "manually_entered_address_moves_a_relayed_link_to_the_direct_path",
        Some(4),
        manually_entered_address_moves_a_relayed_link_to_the_direct_path_scenario(),
    );
}

async fn manually_entered_address_moves_a_relayed_link_to_the_direct_path_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, r) = relayed_trio(root.path()).await;
    oink_network::link::testing::set_direct_blocked(&b.node.links().unwrap(), a.node.id(), false);
    oink_network::link::testing::set_direct_blocked(&a.node.links().unwrap(), b.node.id(), false);
    // Let the attempt that lifting the block starts (to the facts' addresses) run out.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let result = common::query(&a.node, &b.node).await;
    let end = Instant::now() + Duration::from_secs(20);
    let mut direct = false;
    while Instant::now() < end && !direct {
        direct = path_to(&a.node, &b.node) == Some(LinkPath::Direct);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let path = path_to(&a.node, &b.node);
    shutdown(vec![a, b, r]).await;
    assert!(
        direct,
        "20 s after the manual connect ({result:?}) the path is still {path:?}"
    );
}

/// Group geometry remains at the durable version while its atomic save fails.
#[test]
fn update_received_while_the_state_file_cannot_be_written_is_applied_once_it_can() {
    common::watchdog::run(
        module_path!(),
        "update_received_while_the_state_file_cannot_be_written_is_applied_once_it_can",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let a = start(root.path(), "alpha").await;
            let b = start(root.path(), "beta").await;
            common::pair(&a.node, &b.node).await;
            wait_for("settled layout", 15.0, || {
                a.node.layout().0.screens.len() == 2 && a.node.layout() == b.node.layout()
            })
            .await;
            let state = b.dir.join("group-state.json");
            let saved = b.dir.join("group-state.saved-for-test.json");
            let before_link = path_to(&b.node, &a.node);
            let old = b.node.layout();
            std::fs::rename(&state, &saved).unwrap();
            std::fs::create_dir(&state).unwrap();
            api(&a.node, |node| {
                node.link_config().unwrap().edit_layout(
                    Box::new(|layout| layout.settings.gap_travel = !layout.settings.gap_travel),
                    oink_protocol::v1::dto::LayoutSource::Manual,
                )
            })
            .await
            .unwrap();
            assert_ne!(a.node.layout(), old, "the source must author a new shared register");
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(b.node.layout(), old, "unpersisted geometry cannot become applied");
            assert!(state.is_dir());
            std::fs::remove_dir(&state).unwrap();
            wait_for("accepted layout persisted and applied after recovery", 10., || {
                b.node.layout() == a.node.layout()
            })
            .await;
            let persisted = oink_agent::config_module::group_store::load(&state, b.node.id())
                .unwrap()
                .unwrap();
            let expected = oink_agent::config_module::group_store::load(&a.dir.join("group-state.json"), a.node.id())
                .unwrap()
                .unwrap();
            assert_eq!(
                persisted.layout_register().unwrap(),
                expected.layout_register().unwrap()
            );
            assert_eq!(path_to(&b.node, &a.node), before_link);
            shutdown(vec![a, b]).await;
        },
    );
}

/// Three idle members still publish live link statistics every five seconds.
/// Those facts must not rewrite settings, signed group authority or address hints.
#[cfg(unix)]
#[test]
fn an_idle_group_does_not_rewrite_authority_files() {
    common::watchdog::run(
        module_path!(),
        "an_idle_group_does_not_rewrite_authority_files",
        Some(4),
        an_idle_group_does_not_rewrite_authority_files_scenario(),
    );
}

#[cfg(unix)]
async fn an_idle_group_does_not_rewrite_authority_files_scenario() {
    use std::os::unix::fs::MetadataExt;
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    let c = start(root.path(), "gamma").await;
    common::pair(&a.node, &b.node).await;
    common::pair(&a.node, &c.node).await;
    common::active(&[&a.node, &b.node, &c.node]).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    // Live link statistics refresh the facts cache. Authority and address
    // records must remain stable across that periodic publication.
    let files = ["settings.json", "group-state.json", "addresses.json"];
    // Require every current record to exist: missing legacy paths must not
    // silently turn this into a test of zero-valued inode sentinels.
    let inode = |f: &str| std::fs::metadata(a.dir.join(f)).unwrap().ino();
    let mut last: Vec<u64> = files.iter().map(|f| inode(f)).collect();
    let mut replaced = vec![0u32; files.len()];
    let window = oink_agent::config_module::FACTS_INTERVAL + Duration::from_secs(1);
    let begun = Instant::now();
    while begun.elapsed() < window {
        for (i, f) in files.iter().enumerate() {
            let now = inode(f);
            if now != last[i] {
                last[i] = now;
                replaced[i] += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let report: Vec<_> = files.iter().zip(&replaced).collect();
    println!("replacements on one device in {window:?} of idling: {report:?}");
    assert_eq!(
        replaced.iter().sum::<u32>(),
        0,
        "files replaced (each with two flushes) on one idle device in {window:?}: {report:?}"
    );
    shutdown(vec![a, b, c]).await;
}

/// Every change of this device's displays is synchronised into the shared layout;
/// Publication is deferred until the handshake that adopted a layout ends. Here an
/// unrelated link is waiting for the user to type a pairing code (up to 120 s), and the
/// display change of a device in a working group of two is held back for all that time.
#[test]
fn display_change_reaches_the_layout_while_a_pairing_code_is_awaited() {
    common::watchdog::run(
        module_path!(),
        "display_change_reaches_the_layout_while_a_pairing_code_is_awaited",
        Some(4),
        display_change_reaches_the_layout_while_a_pairing_code_is_awaited_scenario(),
    );
}

async fn display_change_reaches_the_layout_while_a_pairing_code_is_awaited_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "alpha").await;
    let b = start(root.path(), "beta").await;
    let c = start(root.path(), "gamma").await;
    common::pair(&a.node, &b.node).await;
    row(&[&a, &b]).await;
    let screens_of_a = |n: &Node| {
        n.layout()
            .0
            .screens
            .iter()
            .filter(|s| s.id.device == a.node.id())
            .count()
    };
    // Control: with no handshake in progress a second display appears on both devices.
    let two = vec![display("alpha"), {
        let mut second = display("alpha-2");
        second.bounds = Rect::new(1600.0, 0.0, 1600.0, 1000.0);
        second.is_primary = false;
        second
    }];
    a.displays.set_displays(two.clone());
    wait_all(&[&a, &b], "second display in the layout", 10.0, || {
        screens_of_a(&a.node) == 2 && screens_of_a(&b.node) == 2
    })
    .await;
    a.displays.set_displays(vec![display("alpha")]);
    wait_all(&[&a, &b], "second display removed", 10.0, || {
        screens_of_a(&a.node) == 1 && screens_of_a(&b.node) == 1
    })
    .await;
    // A starts pairing with C; nobody types the code on C.
    let (pair, _) = common::request(&a.node, &c.node).await;
    a.displays.set_displays(two);
    let end = Instant::now() + Duration::from_secs(8);
    while !(screens_of_a(&a.node) == 2 && screens_of_a(&b.node) == 2) {
        assert!(
            Instant::now() < end,
            "8 s after the second display was connected the shared layout still has {} screen(s) of A on A and {} on B; the pairing code is still awaited: {:?}",
            screens_of_a(&a.node),
            screens_of_a(&b.node),
            a.node
                .pairings()
                .unwrap()
                .sessions
                .iter()
                .find(|s| s.id == pair)
                .map(|p| p.phase)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown(vec![a, b, c]).await;
}

/// Exercise the native notification seam, not ConfigModule's direct test input.
/// Both owners must expose the same placement, compiled coordinates and API metadata.
#[test]
fn display_notifications_refresh_layout_topology_api_and_restore_detached_placement() {
    common::watchdog::run(
        module_path!(),
        "display_notifications_refresh_layout_topology_api_and_restore_detached_placement",
        Some(4),
        display_notifications_refresh_layout_topology_api_and_restore_detached_placement_scenario(),
    );
}

async fn display_notifications_refresh_layout_topology_api_and_restore_detached_placement_scenario() {
    use oink_display::layout::{ScreenId, SizeSource};
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "display-owner").await;
    let b = start(root.path(), "display-peer").await;
    common::pair(&a.node, &b.node).await;
    row(&[&a, &b]).await;
    let id = ScreenId::new(a.node.id(), &a.name);
    let check = |displays: &[DisplayInfo]| {
        [&a, &b].iter().all(|dev| {
            let config = dev.node.link_config().unwrap();
            let shared = config.applied();
            let compiled = config.router_layout().load();
            let view = AgentApi::layout(&dev.node).unwrap();
            let own = shared
                .layout
                .screens
                .iter()
                .filter(|s| s.id.device == a.node.id())
                .count();
            own == displays.len()
                && displays.iter().all(|display| {
                    let key = ScreenId::new(a.node.id(), &display.key);
                    let Some(screen) = shared.layout.screen(&key) else {
                        return false;
                    };
                    let Some(topology) = compiled.topology.topology().screen(&key) else {
                        return false;
                    };
                    let Some(api) = view
                        .geometry
                        .screens
                        .iter()
                        .find(|s| s.key.device.0 == key.device && s.key.display == key.display)
                    else {
                        return false;
                    };
                    screen.local == display.bounds
                        && &topology.placement == screen
                        && &api.display == display
                        && api.origin == Point::new(screen.rect.x, screen.rect.y)
                        && api.size == Size::new(screen.rect.w, screen.rect.h)
                        && compiled.topology.topology().is_active(&key)
                })
                && shared.layout.screens.iter().all(|screen| {
                    screen.id.device != a.node.id() || displays.iter().any(|d| d.key == screen.id.display)
                })
        })
    };
    let mut primary = display(&a.name);
    // Existing key, different desktop bounds and backing resolution/scale.
    primary.bounds = Rect::new(0., 0., 1280., 800.);
    primary.pixel_width = 2560;
    primary.pixel_height = 1600;
    a.displays.set_displays(vec![primary.clone()]);
    wait_all(&[&a, &b], "resolution in shared layout/topology/API", 10., || {
        check(&[primary.clone()])
    })
    .await;
    assert_eq!(
        a.node.layout().0.screen(&id).unwrap().rect,
        Rect::new(0., 0., 320. / 686., 200. / 686.)
    );
    assert_eq!(
        a.node.link_config().unwrap().local_displays().displays,
        vec![primary.clone()]
    );

    // Display mode changes preserve group scale and owner gain.
    let edit_id = id.clone();
    let config = a.node.link_config().unwrap();
    tokio::task::spawn_blocking(move || {
        config.edit_layout(
            Box::new(move |layout| {
                let screen = layout.screen_mut(&edit_id).unwrap();
                screen.size_source = SizeSource::Manual;
            }),
            oink_protocol::v1::dto::LayoutSource::Manual,
        )
    })
    .await
    .unwrap()
    .unwrap();
    let gain_key = ScreenKey {
        device: a.node.id().into(),
        display: id.display.clone(),
    };
    api(&a.node, move |node| node.set_screen_gain(gain_key, 1.7))
        .await
        .unwrap();
    primary.pixel_width = 3840;
    primary.pixel_height = 2400;
    a.displays.set_displays(vec![primary.clone()]);
    wait_all(&[&a, &b], "pixel-only scale change", 10., || check(&[primary.clone()])).await;
    let placement = a.node.layout().0.screen(&id).unwrap().clone();
    primary.bounds = Rect::new(0., 0., 1920., 1200.);
    a.displays.set_displays(vec![primary.clone()]);
    wait_all(&[&a, &b], "manual-size resolution change", 10., || {
        check(&[primary.clone()])
    })
    .await;
    for dev in [&a, &b] {
        let layout = dev.node.layout().0;
        let changed = layout.screen(&id).unwrap();
        assert_eq!(changed.rect.x, placement.rect.x);
        assert!((changed.rect.w / changed.local.w - placement.rect.w / placement.local.w).abs() < 1e-12);
        assert_eq!(changed.size_source, SizeSource::Manual);
        assert_eq!(
            dev.node.link_config().unwrap().router_layout().load().owners[&a.node.id()].screens[&id.display].gain,
            1.7
        );
    }
    // Mac peer's reported sequence: a scaled mode and its revert, with the same
    // peer attached throughout. Check facts, layout and compiled geometry after
    // every notification, then drive a real fake-backend handoff across its gap.
    let link = a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link;
    for (width, height) in [(1728., 1117.), (1496., 967.), (1728., 1117.)] {
        primary.bounds = Rect::new(0., 0., width, height);
        primary.pixel_width = (width * 2.) as u32;
        primary.pixel_height = (height * 2.) as u32;
        a.displays.set_displays(vec![primary.clone()]);
        wait_all(&[&a, &b], "mode change and revert while connected", 10., || {
            check(&[primary.clone()])
        })
        .await;
        assert_eq!(a.node.links().unwrap().links().by_peer(b.node.id()).unwrap().link, link);
    }
    let gain_key = ScreenKey {
        device: a.node.id().into(),
        display: id.display.clone(),
    };
    api(&a.node, move |node| node.set_screen_gain(gain_key, 1.))
        .await
        .unwrap();
    let a_id = id.clone();
    let b_id = ScreenId::new(b.node.id(), &b.name);
    let config = a.node.link_config().unwrap();
    tokio::task::spawn_blocking(move || {
        config.edit_layout(
            Box::new(move |layout| {
                let left = layout.screen_mut(&a_id).unwrap();
                left.rect = Rect::new(
                    -526.2891611498471 / 686.,
                    222.16467832122427 / 686.,
                    337.83240576203025 / 686.,
                    218.3789335857568 / 686.,
                );
                let right = layout.screen_mut(&b_id).unwrap();
                right.rect = Rect::new(
                    -123.71222202910451 / 686.,
                    235.48722050101873 / 686.,
                    447.39681080170357 / 686.,
                    447.39681080170357 / 686. * 1000. / 1600.,
                );
                right.size_source = SizeSource::Manual;
            }),
            oink_protocol::v1::dto::LayoutSource::Manual,
        )
    })
    .await
    .unwrap()
    .unwrap();
    wait_all(&[&a, &b], "gap layout compiled on peer", 10., || {
        check(&[primary.clone()]) && b.node.layout().0 == a.node.layout().0
    })
    .await;
    a.input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::PointerDelta { dx: 2., dy: 0. },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: Some(Point::new(1727.999, 500.)),
    });
    wait_all(&[&a, &b], "source owns pointer before re-entry", 10., || {
        holder(&a.node) == Some(b.node.id())
            && holder(&b.node) == Some(b.node.id())
            && matches!(b.input.mode(), Some(CaptureMode::Local))
            && b.input.cursor_position().unwrap().x < 5.
    })
    .await;
    // The native callback supplies the already-clamped source position.
    b.input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::PointerDelta { dx: -2., dy: 0. },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: Some(Point::new(0., 420.)),
    });
    wait_all(&[&a, &b], "entry after restored mode", 10., || {
        holder(&a.node) == Some(a.node.id())
            && holder(&b.node) == Some(a.node.id())
            && matches!(a.input.mode(), Some(CaptureMode::Local))
    })
    .await;
    let entry = a.input.cursor_position().unwrap();
    assert!(
        (entry.x - 1725.139).abs() < 0.01,
        "restored mode must consume the remainder using fresh bounds: {entry:?}"
    );
    let mut second = display("display-added");
    second.bounds.x = primary.bounds.right();
    second.is_primary = false;
    let two = vec![primary.clone(), second.clone()];
    a.displays.set_displays(two.clone());
    wait_all(&[&a, &b], "display addition", 10., || check(&two)).await;
    let second_id = ScreenId::new(a.node.id(), &second.key);
    let transform = a.node.layout().0.device_transform(a.node.id()).unwrap();
    let mut rearranged = second.clone();
    rearranged.bounds.x = -1600.0;
    rearranged.bounds.y = -100.0;
    let native = vec![primary.clone(), rearranged.clone()];
    a.displays.set_displays(native.clone());
    wait_all(&[&a, &b], "native multi-screen rearrangement", 10.0, || check(&native)).await;
    for dev in [&a, &b] {
        let layout = dev.node.layout().0;
        let current = layout.device_transform(a.node.id()).unwrap();
        assert!((current.0.x - transform.0.x).abs() < 1e-12 && (current.0.y - transform.0.y).abs() < 1e-12);
        assert!((current.1 - transform.1).abs() < 1e-12);
        let screen = layout.screen(&second_id).unwrap();
        assert!((screen.rect.x - transform.0.x - rearranged.bounds.x * transform.1).abs() < 1e-12);
    }
    a.displays.set_displays(two.clone());
    wait_all(&[&a, &b], "native group revert", 10.0, || check(&two)).await;
    let hidden = ScreenKey::from(&second_id);
    api(&a.node, move |node| node.set_screen_hidden(hidden, true))
        .await
        .unwrap();
    wait_all(&[&a, &b], "hidden group member", 10.0, || {
        [&a, &b].iter().all(|dev| {
            AgentApi::layout(&dev.node)
                .unwrap()
                .geometry
                .screens
                .iter()
                .any(|s| s.key.display == second.key && s.hidden)
        })
    })
    .await;
    let device = a.node.id();
    api(&b.node, move |node| {
        node.save_layout(vec![Placement {
            device: device.into(),
            origin: Point::new(transform.0.x, transform.0.y - 1.0),
            scale: transform.1 * 1.1,
        }])
    })
    .await
    .unwrap();
    wait_all(&[&a, &b], "group scale includes hidden member", 10.0, || {
        a.node.layout().0 == b.node.layout().0
    })
    .await;
    for screen in a.node.layout().0.screens.iter().filter(|s| s.id.device == device) {
        assert!((screen.rect.w / screen.local.w - transform.1 * 1.1).abs() < 1e-12);
    }
    let hidden = ScreenKey::from(&second_id);
    api(&a.node, move |node| node.set_screen_hidden(hidden, false))
        .await
        .unwrap();
    wait_all(&[&a, &b], "revealed native group", 10.0, || check(&two)).await;

    let edit_id = second_id.clone();
    let config = a.node.link_config().unwrap();
    tokio::task::spawn_blocking(move || {
        config.edit_layout(
            Box::new(move |layout| {
                layout.move_screen(&edit_id, Point::new(900.0 / 686.0, -300.0 / 686.0));
            }),
            oink_protocol::v1::dto::LayoutSource::Manual,
        )
    })
    .await
    .unwrap()
    .unwrap();
    wait_all(&[&a, &b], "saved added display placement", 10., || {
        b.node.layout().0.screen(&second_id) == a.node.layout().0.screen(&second_id)
    })
    .await;
    let gain_key = ScreenKey {
        device: a.node.id().into(),
        display: second_id.display.clone(),
    };
    api(&a.node, move |node| node.set_screen_gain(gain_key, 1.9))
        .await
        .unwrap();
    let remembered = a.node.layout().0.screen(&second_id).unwrap().clone();
    let remembered_wire = oink_agent::config_module::layout_wire::to_wire(
        &a.node.layout().0,
        oink_protocol::v1::dto::LayoutSource::Manual,
    )
    .unwrap()
    .screens()
    .unwrap()
    .0
    .into_iter()
    .find(|s| s.id().unwrap().display().unwrap().0 == second_id.display)
    .unwrap();
    a.displays.set_displays(vec![primary.clone()]);
    wait_all(&[&a, &b], "display removal", 10., || {
        check(&[primary.clone()])
            && [&a, &b].iter().all(|dev| {
                dev.node
                    .link_config()
                    .unwrap()
                    .router_layout()
                    .load()
                    .topology
                    .topology()
                    .screen(&second_id)
                    .is_none()
                    && !AgentApi::layout(&dev.node)
                        .unwrap()
                        .geometry
                        .screens
                        .iter()
                        .any(|s| s.key.display == second.key)
            })
    })
    .await;
    let detached = oink_agent::config_module::detached::load(&a.dir.join("detached-screens.json")).unwrap();
    assert!(detached.screens.iter().any(|s| s.placement == remembered_wire));
    a.displays.set_displays(two.clone());
    wait_all(&[&a, &b], "detached display restored", 10., || {
        check(&two)
            && [&a, &b].iter().all(|dev| {
                dev.node.layout().0.screen(&second_id) == Some(&remembered)
                    && dev.node.link_config().unwrap().router_layout().load().owners[&a.node.id()].screens
                        [&second_id.display]
                        .gain
                        == 1.9
            })
    })
    .await;
    shutdown(vec![a, b]).await;
}

#[test]
fn api_link_address_is_the_selected_connection_not_an_advertised_endpoint() {
    common::watchdog::run(
        module_path!(),
        "api_link_address_is_the_selected_connection_not_an_advertised_endpoint",
        Some(2),
        api_link_address_is_the_selected_connection_not_an_advertised_endpoint_scenario(),
    );
}

async fn api_link_address_is_the_selected_connection_not_an_advertised_endpoint_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "address-a").await;
    let b = start(root.path(), "address-b").await;
    common::pair(&a.node, &b.node).await;
    common::active(&[&a.node, &b.node]).await;
    let actual = format!("127.0.0.1:{}", b.node.network_info().unwrap().port);
    // The scenario publishes LAN interface facts while connecting on loopback.
    // This reproduces an advertised VPN/LAN address differing from the actual hop.
    let view = a.node.group().unwrap();
    let member = view.members.iter().find(|m| m.device.id.0 == b.node.id()).unwrap();
    assert_eq!(
        member.link.as_ref().unwrap().remote_address.as_deref(),
        Some(actual.as_str())
    );
    let snapshot = a.node.links().unwrap().links();
    assert_eq!(
        snapshot
            .by_peer(b.node.id())
            .unwrap()
            .remote_address
            .unwrap()
            .to_string(),
        actual
    );
    shutdown(vec![a, b]).await;
}

#[path = "link_scenarios/dnd.rs"]
mod dnd;
