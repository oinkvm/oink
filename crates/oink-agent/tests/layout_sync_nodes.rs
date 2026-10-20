#![cfg(debug_assertions)]
//! Three real loopback nodes preserve exact shared geometry through offline edits
//! and restart. Owner preferences remain separate from retained layout registers.

use oink_agent as test_agent;
mod common;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oink_agent::api::{AgentApi, Placement, ScreenKey};
use oink_agent::node::{Node, NodeOptions};
use oink_foundation::display::DisplayInfo;
use oink_foundation::{Point, Rect, Size};
use oink_protocol::v1::schema::WireDto;
use oink_test_support::input::FakeInput;

fn display(key: &str, reference: Size) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds: Rect::new(0.0, 0.0, 1600.0, 1000.0),
        pixel_width: 1600,
        pixel_height: 1000,
        reference_extent: Some(reference),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    }
}

struct MemoryDisplay(Vec<DisplayInfo>);
impl oink_display::DisplayBackend for MemoryDisplay {
    fn displays(&self) -> Result<Vec<DisplayInfo>, oink_display::DisplayError> {
        Ok(self.0.clone())
    }
    fn watch(&self, _: oink_display::ChangeHandler) -> Result<(), oink_display::DisplayError> {
        Ok(())
    }
}

fn home(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("oink-layout-sync-{}-{name}", std::process::id()))
}

/// 启动一个服务进程；`fresh` 时先清空数据目录（否则沿用，模拟重启）。
async fn start(name: &str, port: u16, fresh: bool) -> Node {
    let dir = home(name);
    if fresh {
        let _ = std::fs::remove_dir_all(&dir);
    }
    // 重启时上一个实例刚释放的端口可能还没完全关闭：短暂重试。
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let input = FakeInput::new();
        input.set_cursor(Point::new(800.0, 500.0));
        let r = Node::start_with_fake_platform(NodeOptions {
            net: oink_test_support::network_config(dir.clone(), name, port),
            input: Arc::new(input),
            displays: vec![display(name, Size::new(400.0 / 686.0, 250.0 / 686.0))],
            display_backend: Some(Arc::new(MemoryDisplay(vec![display(
                name,
                Size::new(400.0 / 686.0, 250.0 / 686.0),
            )]))),
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        })
        .await;
        match r {
            Ok(n) => return n,
            Err(e) if Instant::now() < end => {
                eprintln!("start {name}: {e:#}, retrying");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => panic!("start {name}: {e:#}"),
        }
    }
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

async fn api<T: Send + 'static>(n: &Node, f: impl FnOnce(&Node) -> T + Send + 'static) -> T {
    let n = n.clone();
    tokio::task::spawn_blocking(move || f(&n)).await.unwrap()
}

/// Compare the retained layout register bytes as well as its applied projection.
fn identical(a: &Node, b: &Node) -> bool {
    let (la, lb) = (a.layout(), b.layout());
    let register = |n: &Node| {
        oink_agent::config_module::group_store::load(
            &n.data_dir().join(oink_agent::config_module::group_store::STATE_FILE),
            n.id(),
        )
        .ok()??
        .layout_register()
        .ok()?
        .encode(oink_protocol::v1::limits::CONFIG_STATE)
        .ok()
    };
    la.1.revision > 0 && la.1 == lb.1 && la.0 == lb.0 && register(a).is_some() && register(a) == register(b)
}

fn gain_of(n: &Node, device: oink_foundation::DeviceId) -> Option<f64> {
    AgentApi::layout(n)
        .ok()?
        .geometry
        .screens
        .into_iter()
        .find(|s| s.key.device.0 == device)
        .map(|s| s.gain)
}

fn describe(nodes: &[&Node]) -> String {
    nodes
        .iter()
        .map(|n| format!("{:?}\n{:?}", n.layout().1, n.layout().0))
        .collect::<Vec<_>>()
        .join("\n---\n")
}

fn rect_of(n: &Node, device: oink_foundation::DeviceId) -> Rect {
    n.layout()
        .0
        .screens
        .iter()
        .find(|s| s.id.device == device)
        .unwrap()
        .rect
}

#[test]
fn three_nodes_manual_coordinates_offline_and_restart() {
    common::watchdog::run(
        module_path!(),
        "three_nodes_manual_coordinates_offline_and_restart",
        Some(4),
        three_nodes_manual_coordinates_offline_and_restart_scenario(),
    );
}

async fn three_nodes_manual_coordinates_offline_and_restart_scenario() {
    oink_agent::logs::init();
    let base = 48_000 + (std::process::id() % 100) as u16 * 6;
    let (pa, pb, pc) = (base, base + 2, base + 4);
    let a = start("alpha", pa, true).await;
    let b = start("beta", pb, true).await;
    common::pair(&b, &a).await;
    let b_frequency = b.input_settings().unwrap().event_frequency;
    api(&a, |n| {
        let mut settings = n.input_settings().unwrap();
        settings.event_frequency = oink_agent::api::EventFrequency::Unlimited;
        n.set_input_settings(settings.into())
    })
    .await
    .unwrap();
    assert_eq!(
        a.input_settings().unwrap().event_frequency,
        oink_agent::api::EventFrequency::Unlimited
    );
    assert_eq!(b.input_settings().unwrap().event_frequency, b_frequency);
    let resident_tokens = [common::input_id(&a, b.id()), common::input_id(&b, a.id())];
    let c = start("gamma", pc, true).await;
    let c_frequency = c.input_settings().unwrap().event_frequency;
    common::pair(&c, &a).await;
    let all = [&a, &b, &c];
    wait_for("all three online and in every layout", 15.0, || {
        all.iter()
            .all(|n| n.online_set().len() == 3 && n.layout().0.screens.len() == 3)
    })
    .await;
    assert_eq!(
        [common::input_id(&a, b.id()), common::input_id(&b, a.id())],
        resident_tokens,
        "inviting a third member must preserve the resident pair"
    );

    assert_eq!(c.input_settings().unwrap().event_frequency, c_frequency);

    // An API client can submit an un-compacted layout without using the canvas.
    // The upper computer must meet the nearer of two staggered lower computers;
    // the complete normalized register then converges without receiver rewrites.
    let compact_placements = [
        (a.id(), Point::new(0.9, 0.0), 1.6 / 1600.0),
        (b.id(), Point::new(0.0, 1.08), 1.0 / 1600.0),
        (c.id(), Point::new(1.3, 1.12), 1.04 / 1600.0),
    ]
    .map(|(device, origin, scale)| Placement {
        device: device.into(),
        origin,
        scale,
    })
    .to_vec();
    api(&b, move |n| n.save_layout(compact_placements)).await.unwrap();
    wait_for("backend-compacted layout reaches all three nodes", 10.0, || {
        identical(&a, &b) && identical(&a, &c)
    })
    .await;
    assert!((rect_of(&a, a.id()).y - 0.08).abs() < 1e-10);
    assert_eq!(rect_of(&a, b.id()).y, 1.08);
    assert_eq!(rect_of(&a, c.id()).y, 1.12);

    // ---- A 摆放：坐标与尺寸带很长的小数（与标定写入的一样），同步后三台完全相同。
    let (ida, idb, idc) = (a.id(), b.id(), c.id());
    let key = |d, name: &str| ScreenKey {
        device: oink_agent::api::DeviceKey(d),
        display: name.into(),
    };
    let placements = vec![
        Placement {
            device: ida.into(),
            origin: Point::new(-399.99989766224076 / 686.0, -3.4340814103472894e-5 / 686.0),
            scale: 399.99989766224076 / 686.0 / 1600.0,
        },
        Placement {
            device: idb.into(),
            origin: Point::new(0.0, (0.1 + 0.2) / 686.0),
            // One scale preserves the native aspect exactly.
            scale: 400.0 / 686.0 / 1600.0,
        },
        Placement {
            device: idc.into(),
            origin: Point::new(400.0000000000001 / 686.0, 7.940933880509066e-21 / 686.0),
            scale: 400.0 / 686.0 / 1600.0,
        },
    ];
    let saved = api(&a, move |n| n.save_layout(placements)).await.unwrap();
    let beta = saved
        .geometry
        .screens
        .iter()
        .find(|s| s.key == key(idb, "beta"))
        .unwrap();
    assert_eq!(beta.size, Size::new(400.0 / 686.0, 250.0 / 686.0));
    wait_for("placed layout identical on all three", 10.0, || {
        identical(&a, &b) && identical(&a, &c)
    })
    .await;
    // Gap travel belongs to the shared layout register; input frequency stays local.
    api(&a, |n| {
        let mut settings = n.input_settings().unwrap();
        settings.gap_travel = true;
        settings.event_frequency = oink_agent::api::EventFrequency::Unlimited;
        n.set_input_settings(settings.into())
    })
    .await
    .unwrap();
    wait_for("gap setting reaches all nodes", 10.0, || {
        identical(&a, &b) && identical(&a, &c) && c.layout().0.settings.gap_travel
    })
    .await;
    assert_eq!(c.input_settings().unwrap().event_frequency, c_frequency);
    let placed = a.layout();
    assert!(
        placed.0.screens.iter().any(|s| s.rect.x.fract() != 0.0),
        "the test layout must carry fractional coordinates: {:?}",
        placed.0
    );

    // ---- C 离线：它的屏幕仍留在布局数据里（原位置），只从运行时拓扑中排除。
    let gamma_rect = rect_of(&a, idc);
    c.shutdown().await;
    drop(c);
    // Router cleanup precedes path teardown; the UI intentionally describes
    // reachability even when business sharing is paused. Wait for the stopped
    // instance's path to disappear before checking its offline screen marker.
    wait_for("C offline on A and B", 15.0, || {
        !a.online_set().contains(&idc)
            && !b.online_set().contains(&idc)
            && !common::linked(&a, idc)
            && !common::linked(&b, idc)
    })
    .await;
    assert_eq!(a.layout(), placed, "going offline must not change the shared layout");
    assert_eq!(b.layout(), placed);
    // 界面上画成熄屏（online = false），位置不变。
    let view = AgentApi::layout(&a).unwrap();
    let g = view.geometry.screens.iter().find(|s| s.key.device.0 == idc).unwrap();
    assert!(!g.online);
    assert_eq!((g.origin.x, g.origin.y), (gamma_rect.x, gamma_rect.y));

    // ---- C 离线时 B 修改：新版本在 A、B 上完全相同，C 的屏幕原样保留。
    let screen = key(idb, "beta");
    api(&b, move |n| n.set_screen_gain(screen, 1.0 / 0.73)).await.unwrap();
    wait_for("B owner gain reaches A without editing shared geometry", 10.0, || {
        gain_of(&a, idb) == Some(1.0 / 0.73) && gain_of(&b, idb) == Some(1.0 / 0.73)
    })
    .await;
    assert_eq!(a.layout(), placed);
    assert_eq!(b.layout(), placed);
    let alpha = rect_of(&b, ida);
    let alpha_key = key(ida, "alpha");
    api(&b, move |n| {
        n.save_layout(vec![Placement {
            device: alpha_key.device,
            origin: Point::new(alpha.x - 0.125 / 686.0, alpha.y),
            scale: 400.0 / 686.0 / 1600.0,
        }])
    })
    .await
    .unwrap();
    wait_for("shared geometry edit reaches A while C is offline", 10.0, || {
        identical(&a, &b) && a.layout().1.revision > placed.1.revision
    })
    .await;
    assert_eq!(rect_of(&a, idc), gamma_rect, "offline device keeps its position");
    assert_eq!(rect_of(&b, idc), gamma_rect);

    api(&b, |n| {
        let mut settings = n.input_settings().unwrap();
        settings.event_frequency = oink_agent::api::EventFrequency::Hz125;
        n.set_input_settings(settings.into())
    })
    .await
    .unwrap();
    assert_eq!(
        b.input_settings().unwrap().event_frequency,
        oink_agent::api::EventFrequency::Hz125
    );
    assert_eq!(
        a.input_settings().unwrap().event_frequency,
        oink_agent::api::EventFrequency::Unlimited
    );

    // ---- C 用原来的数据目录重启：追上最新版本，与 A、B 逐字节相同。
    let c = start("gamma", pc, false).await;
    wait_for("restarted C catches up", 20.0, || {
        c.online_set().len() == 3 && identical(&a, &c) && identical(&a, &b) && gain_of(&c, idb) == Some(1.0 / 0.73)
    })
    .await;
    assert_eq!(rect_of(&c, idc), gamma_rect);
    assert!(c.input_settings().unwrap().gap_travel);
    assert_eq!(c.input_settings().unwrap().event_frequency, c_frequency);
    // 之后没有人再发布新版本（接收方不做变换，不会来回重发）。
    let settled = a.layout().1;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for n in [&a, &b, &c] {
        assert_eq!(n.layout().1, settled, "{}", describe(&[&a, &b, &c]));
    }
    assert!(identical(&a, &b) && identical(&a, &c), "{}", describe(&[&a, &b, &c]));

    // Reset uses original geometry while retaining each owner's gain and local options.
    api(&a, |n| n.reset_initial_layout()).await.unwrap();
    wait_for("reset initial layout reaches all nodes", 10.0, || {
        identical(&a, &b) && identical(&a, &c)
    })
    .await;
    for n in [&a, &b, &c] {
        assert_eq!(gain_of(n, idb), Some(1.0 / 0.73));
        for screen in n.layout().0.screens {
            assert_eq!(screen.rect.w, 400.0 / 686.0);
            assert_eq!(screen.rect.h, 250.0 / 686.0);
        }
    }
    for n in [&a, &b, &c] {
        n.shutdown().await;
    }
    for name in ["alpha", "beta", "gamma"] {
        let _ = std::fs::remove_dir_all(home(name));
    }
}
