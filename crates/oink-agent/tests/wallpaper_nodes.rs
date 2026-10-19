#![cfg(debug_assertions)]
//! Two loopback nodes exchange wallpaper thumbnails on connection and changes.
//! Injected display and input backends keep native services disabled. Unchanged
//! thumbnails do not publish again; disconnected peers retain their last image.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use oink_agent as test_agent;
mod common;

use base64::Engine;
use oink_agent::api::{AgentApi, ApiEvent, ScreenWallpaper};
use oink_agent::node::{Node, NodeOptions};
use oink_display::wallpaper::{DisplayWallpaper, Thumbnail};
use oink_display::{ChangeHandler, DisplayBackend, DisplayError};
use oink_foundation::display::DisplayInfo;
use oink_foundation::{Point, Rect, Size};
use oink_test_support::input::FakeInput;

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

/// 假的显示器 backend：壁纸由测试设置，`poke` 模拟「切换了桌面空间」的通知。
#[derive(Default)]
struct FakeDisplays {
    key: String,
    /// `None`：平台没有实现（Windows 目前如此）。
    wallpapers: Mutex<Option<Vec<DisplayWallpaper>>>,
    handlers: Mutex<Vec<ChangeHandler>>,
    reads: AtomicUsize,
    /// Hold the first blocking read until the test has subscribed.
    initial_read: Mutex<Option<mpsc::Receiver<()>>>,
}

impl FakeDisplays {
    fn set(&self, jpeg: Option<Vec<u8>>) {
        let list = jpeg
            .into_iter()
            .map(|jpeg| DisplayWallpaper {
                key: self.key.clone(),
                thumbnail: Thumbnail {
                    width: 480,
                    height: 300,
                    jpeg,
                },
            })
            .collect();
        *self.wallpapers.lock().unwrap() = Some(list);
    }

    fn poke(&self) {
        for h in self.handlers.lock().unwrap().iter() {
            h();
        }
    }
}

impl DisplayBackend for FakeDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        Ok(vec![display(&self.key)])
    }
    fn watch(&self, _: ChangeHandler) -> Result<(), DisplayError> {
        Ok(())
    }
    fn wallpapers(&self) -> Option<Vec<DisplayWallpaper>> {
        let gate = self.initial_read.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.recv().expect("release initial wallpaper read");
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.wallpapers.lock().unwrap().clone()
    }
    fn watch_wallpaper(&self, on_change: ChangeHandler) -> Result<(), DisplayError> {
        self.handlers.lock().unwrap().push(on_change);
        Ok(())
    }
}

fn jpeg(fill: u8) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    v.resize(30_000, fill);
    v
}

fn data_url(jpeg: &[u8]) -> String {
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    )
}

async fn start(dir: &Path, name: &str, displays: Arc<FakeDisplays>) -> Node {
    let input = FakeInput::new();
    input.set_cursor(Point::new(800.0, 500.0));
    Node::start_with_fake_platform(NodeOptions {
        net: oink_test_support::network_config(dir.into(), name, 0),
        input: Arc::new(input),
        displays: vec![display(name)],
        display_backend: Some(displays),
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap_or_else(|e| panic!("start {name}: {e:#}"))
}

async fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `node` 看到的、设备 `of` 的壁纸。
fn seen(node: &Node, of: &Node) -> Vec<ScreenWallpaper> {
    node.wallpapers(false)
        .unwrap()
        .into_iter()
        .filter(|w| w.key.device.0 == of.id())
        .collect()
}

fn count_events(node: &Node) -> Arc<AtomicUsize> {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    node.subscribe(Arc::new(move |ev| {
        if matches!(ev, ApiEvent::Wallpapers) {
            n2.fetch_add(1, Ordering::SeqCst);
        }
    }));
    n
}

#[test]
fn thumbnails_follow_connect_change_and_disconnect() {
    common::watchdog::run(
        module_path!(),
        "thumbnails_follow_connect_change_and_disconnect",
        Some(4),
        thumbnails_follow_connect_change_and_disconnect_scenario(),
    );
}

async fn thumbnails_follow_connect_change_and_disconnect_scenario() {
    oink_agent::logs::init();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (release_initial, initial_read) = mpsc::channel();
    let da = Arc::new(FakeDisplays {
        key: "alpha".into(),
        initial_read: Mutex::new(Some(initial_read)),
        ..FakeDisplays::default()
    });
    // beta：平台没有实现（`wallpapers` 返回 None）。
    let db = Arc::new(FakeDisplays {
        key: "beta".into(),
        ..FakeDisplays::default()
    });
    da.set(Some(jpeg(1)));
    let a = start(dir_a.path(), "alpha", da.clone()).await;
    let b = start(dir_b.path(), "beta", db.clone()).await;
    let (events_a, events_b) = (count_events(&a), count_events(&b));
    release_initial.send(()).unwrap();

    // 启动时读一次：本机界面能拿到。
    wait_for("alpha reads its own wallpaper", 5.0, || seen(&a, &a).len() == 1).await;
    assert_eq!(seen(&a, &a)[0].image, data_url(&jpeg(1)));
    assert_eq!(seen(&a, &a)[0].key.display, "alpha");

    // Node::start does not await the blocking initial read. Force that read to
    // complete after subscribing, then finish its event before counting connect
    // events. Reading the stored image alone can precede event delivery.
    wait_for("alpha publishes its initial wallpaper event", 5.0, || {
        events_a.load(Ordering::SeqCst) > 0
    })
    .await;
    assert_eq!(events_a.swap(0, Ordering::SeqCst), 1);
    assert_eq!(events_b.load(Ordering::SeqCst), 0);

    // 初始化时发给对方；没有实现的一方不发送快照。
    common::pair(&b, &a).await;
    wait_for("beta receives alpha's wallpaper and event", 10.0, || {
        seen(&b, &a).len() == 1 && events_b.load(Ordering::SeqCst) > 0
    })
    .await;
    assert_eq!(seen(&b, &a)[0].image, data_url(&jpeg(1)));
    assert_eq!(seen(&b, &a)[0].rev, seen(&a, &a)[0].rev);
    assert!(seen(&a, &b).is_empty() && seen(&b, &b).is_empty());
    assert_eq!(
        (events_a.load(Ordering::SeqCst), events_b.load(Ordering::SeqCst)),
        (0, 1)
    );

    // 内容变了（切到另一个桌面空间）：对方换成新图。
    da.set(Some(jpeg(2)));
    da.poke();
    wait_for("beta receives the new wallpaper", 10.0, || {
        seen(&b, &a).first().is_some_and(|w| w.image == data_url(&jpeg(2)))
            && events_a.load(Ordering::SeqCst) > 0
            && events_b.load(Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(
        (events_a.load(Ordering::SeqCst), events_b.load(Ordering::SeqCst)),
        (1, 2)
    );

    // 没变：通知、界面的显式刷新都会再读一次，但不重发、不重推界面。
    let reads = da.reads.load(Ordering::SeqCst);
    da.poke();
    wait_for("re-read after notification", 5.0, || {
        da.reads.load(Ordering::SeqCst) > reads
    })
    .await;
    let reads = da.reads.load(Ordering::SeqCst);
    assert_eq!(a.wallpapers(true).unwrap().len(), 1);
    wait_for("re-read after explicit refresh", 5.0, || {
        da.reads.load(Ordering::SeqCst) > reads
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        (events_a.load(Ordering::SeqCst), events_b.load(Ordering::SeqCst)),
        (1, 2)
    );

    // 拿不到静态图片了（换成了视频壁纸）：两边都退回没有壁纸。
    da.set(None);
    da.poke();
    wait_for("beta drops alpha's wallpaper", 10.0, || seen(&b, &a).is_empty()).await;
    assert!(seen(&a, &a).is_empty());

    // 又有了；随后 alpha 离线：beta 保留最后一张。
    da.set(Some(jpeg(3)));
    da.poke();
    wait_for("beta receives the third wallpaper", 10.0, || {
        seen(&b, &a).first().is_some_and(|w| w.image == data_url(&jpeg(3)))
    })
    .await;
    let alpha = a.id();
    a.shutdown().await;
    let b2 = b.clone();
    wait_for("beta sees alpha offline", 15.0, || {
        b2.group()
            .unwrap()
            .members
            .iter()
            .any(|m| m.device.id.0 == alpha && !matches!(m.state, oink_agent::api::PeerState::Online))
    })
    .await;
    let kept: Vec<_> = b
        .wallpapers(false)
        .unwrap()
        .into_iter()
        .filter(|w| w.key.device.0 == alpha)
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].image, data_url(&jpeg(3)));
    b.shutdown().await;
}
