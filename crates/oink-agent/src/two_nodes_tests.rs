//! 两个完整的服务进程（`Node`）在本机经真实组网（QUIC、回环地址）相连：配对、焦点移交与键鼠、
//! 剪贴板同步（内联文本与经大块数据流拉取的图片）、布局同步、断线释放按键。输入与剪贴板用假的 backend。

use crate::snapshot_tests::common;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::{AgentApi, ScreenKey};
use crate::node::{Node, NodeOptions};
use oink_clipboard::{ClipData, Sensitivity};
use oink_foundation::display::DisplayInfo;
use oink_foundation::input::{CaptureMode, CapturedEvent, InjectEvent, InputEvent, KeyCode, SourceKind};
use oink_foundation::{Point, Rect, Size};
use oink_input::InputBackend;
use oink_test_support::clipboard::FakeClipboard;
use oink_test_support::input::FakeInput;

fn display(key: &str) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.to_owned(),
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
    input: FakeInput,
    clip: Arc<FakeClipboard>,
    _dir: tempfile::TempDir,
}

async fn start(name: &str, port: u16) -> Dev {
    let dir = tempfile::tempdir().unwrap();
    let input = FakeInput::new();
    input.set_cursor(Point::new(800.0, 500.0));
    let clip = Arc::new(FakeClipboard::new());
    let node = Node::start(NodeOptions {
        net: oink_test_support::network_config(dir.path().into(), name, port),
        input: Arc::new(input.clone()),
        displays: vec![display(name)],
        display_backend: None,
        clipboard: Some(clip.clone()),
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap();
    let mut settings = node.clipboard_settings().unwrap();
    settings.files = true;
    node.set_clipboard_settings(settings.into()).unwrap();
    Dev {
        node,
        input,
        clip,
        _dir: dir,
    }
}

async fn wait_for(what: &str, secs: f64, f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    if observe_until(end, Instant::now, f).await {
        return;
    }
    panic!("timed out waiting for {what}");
}

async fn observe_until(end: Instant, mut now: impl FnMut() -> Instant, mut f: impl FnMut() -> bool) -> bool {
    loop {
        // A delayed wake may cross the deadline after the producer became
        // ready. Observe that result before declaring a timeout; never wait
        // again past the same deadline when the condition is still false.
        if f() {
            return true;
        }
        if now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 在阻塞线程里调用 API（API 会等网络，不能在运行时的工作线程里直接调用）。
async fn api<T: Send + 'static>(n: &Node, f: impl FnOnce(&Node) -> T + Send + 'static) -> T {
    let n = n.clone();
    tokio::task::spawn_blocking(move || f(&n)).await.unwrap()
}

#[test]
fn node_scenarios_do_not_install_a_global_log_subscriber() {
    const CHILD: &str = "OINK_LOG_ISOLATION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // Exercise a real node scenario in a clean process: a global logger
        // would synchronously format every other fixture's network traffic and
        // fan it out to every node's log listener for the rest of the suite.
        focus_lock_api_and_overview_events_name_the_remote_target_on_both_members();
        tracing::dispatcher::get_default(|dispatch| {
            assert!(
                dispatch.is::<tracing::subscriber::NoSubscriber>(),
                "a node fixture must not enable process-wide logging for unrelated tests"
            );
        });
        return;
    }
    let _watchdog = crate::test_watchdog::Scenario::new(module_path!(), "log isolation subprocess");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "two_nodes_tests::node_scenarios_do_not_install_a_global_log_subscriber",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn input_transform_is_owned_locally_and_persists_without_a_layout_edit() {
    crate::test_watchdog::run(
        module_path!(),
        "input_transform_is_owned_locally_and_persists_without_a_layout_edit",
        Some(4),
        async {
            let a = start("transform-a", 0).await;
            let b = start("transform-b", 0).await;
            common::pair(&a.node, &b.node).await;
            let version = a.node.layout().1;
            let group = a.node.group().unwrap();
            let own = group.members.iter().find(|member| member.is_self).unwrap();
            assert!(own.input_transform.is_some());
            assert!(
                group
                    .members
                    .iter()
                    .find(|member| !member.is_self)
                    .unwrap()
                    .input_transform
                    .is_none()
            );
            let mut transform = a.node.settings().input_transform;
            transform.windows.reverse_wheel = !transform.windows.reverse_wheel;
            let expected = transform.clone();
            let owner = a.node.id();
            api(&a.node, move |node| {
                node.set_input_transform(owner.into(), transform.into())
            })
            .await
            .unwrap();
            assert_eq!(a.node.settings().input_transform, expected);
            assert_eq!(
                crate::config::AgentConfig::load_from(a._dir.path())
                    .unwrap()
                    .settings
                    .input_transform,
                expected
            );
            assert_eq!(a.node.layout().1, version);
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

mod audio;

/// Compare the exact retained layout register, including its owner version.
fn assert_layout_files_equal(a: &Node, b: &Node) {
    let read = |node: &Node| {
        crate::config_module::group_store::load(
            &node.data_dir().join(crate::config_module::group_store::STATE_FILE),
            node.id(),
        )
        .unwrap()
        .unwrap()
        .layout_register()
        .unwrap()
    };
    assert_eq!(read(a), read(b));
}

/// 模拟本机系统：焦点在本机时按位移移动光标（出了屏幕停在边上），交给截获回调。
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
    if local && d == oink_foundation::input::Disposition::Pass {
        input.set_cursor(next);
    }
}

fn key(input: &FakeInput, code: KeyCode, pressed: bool) {
    input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::Key { code, pressed },
        source: SourceKind::Keyboard,
        time_us: 0,
        cursor: None,
    });
}

#[test]
fn pair_route_clipboard_layout_disconnect() {
    crate::test_watchdog::run(
        module_path!(),
        "pair_route_clipboard_layout_disconnect",
        Some(4),
        pair_route_clipboard_layout_disconnect_scenario(),
    );
}

async fn pair_route_clipboard_layout_disconnect_scenario() {
    let a = start("alpha", 0).await;
    let b = start("beta", 0).await;

    // ---- 配对：A 手动输入 B 的地址 → 附近列表里出现 B 的组 → 加入 → B 输入 A 显示的配对码。
    common::pair(&a.node, &b.node).await;
    let (an, bn) = (a.node.clone(), b.node.clone());
    let original_tokens = [
        common::input_id(&an, bn.id()).unwrap(),
        common::input_id(&bn, an.id()).unwrap(),
    ];
    let sessions_unchanged = |stage| {
        assert_eq!(
            [common::input_id(&an, bn.id()), common::input_id(&bn, an.id())],
            original_tokens.map(Some),
            "{stage}"
        );
    };
    wait_for("both online", 10.0, || {
        an.online_set().contains(&bn.id()) && bn.online_set().contains(&an.id())
    })
    .await;

    // Audio capability/device lists use the real QUIC control channel, with
    // capture disabled on both nodes. This is not a hardware audio test.
    wait_for("audio capabilities exchanged", 5.0, || {
        an.audio_status().unwrap().devices.iter().any(|d| d.device == bn.id())
            && bn.audio_status().unwrap().devices.iter().any(|d| d.device == an.id())
    })
    .await;
    // Output discovery and owner claims arrive independently. Resolution stays
    // unknown until the current claim record from every member is observed.
    wait_for("audio owner claims exchanged", 5.0, || {
        [&an, &bn].iter().all(|node| {
            matches!(
                node.audio_status().unwrap().resolution,
                oink_audio::claims::Resolution::None { .. }
            )
        })
    })
    .await;
    assert!(matches!(
        an.audio_status().unwrap().resolution,
        oink_audio::claims::Resolution::None { .. }
    ));
    assert!(matches!(
        bn.audio_status().unwrap().resolution,
        oink_audio::claims::Resolution::None { .. }
    ));

    // ---- 布局：两边独立自动排列，结果一致；B 在 A 的哪一边由设备 ID 决定。
    wait_for("layouts include both devices", 5.0, || {
        an.layout().0.screens.len() == 2 && bn.layout().0.screens.len() == 2
    })
    .await;
    assert_eq!(an.layout().0, bn.layout().0);
    // ---- 真实 API：布局预览与提交同源；非法编辑不得发布新版本。
    let preview = an
        .preview_move_device(bn.id().into(), Point::new(0.0, 10.0 / 686.0))
        .unwrap()
        .unwrap();
    let committed = an.move_device(bn.id().into(), Point::new(0.0, 10.0 / 686.0)).unwrap();
    assert_eq!(preview, committed.geometry);
    let revision = an.layout().1.revision;
    assert!(an.move_device(bn.id().into(), Point::new(f64::NAN, 0.0)).is_err());
    assert_eq!(revision, an.layout().1.revision);
    wait_for("edited layout on B", 5.0, || an.layout().0 == bn.layout().0).await;
    sessions_unchanged("checkpoint 1");

    // The pointer scenario below needs a horizontal portal regardless of which
    // device sorted first or how the preceding move preview separated overlap.
    let placements = an
        .layout()
        .0
        .screens
        .iter()
        .map(|screen| crate::api::Placement {
            device: screen.id.device.into(),
            origin: Point::new(if screen.id.device == an.id() { 0. } else { 400. / 686.0 }, 0.),
            scale: 400.0 / 686.0 / 1600.0,
        })
        .collect();
    api(&an, move |node| node.save_layout(placements)).await.unwrap();
    wait_for("manual layout on B", 5., || an.layout() == bn.layout()).await;
    assert_layout_files_equal(&an, &bn);
    sessions_unchanged("checkpoint 2");

    an.identify_screens().unwrap();
    wait_for("identify reaches peer", 5.0, || {
        bn.overlay()
            .unwrap()
            .is_some_and(|o| o.kind == crate::api::OverlayKind::Identify)
    })
    .await;
    wait_for("identify expires on both peers", 5.0, || {
        an.overlay().unwrap().is_none() && bn.overlay().unwrap().is_none()
    })
    .await;

    let la = an.layout().0;
    let ax = la.screens.iter().find(|s| s.id.device == an.id()).unwrap().rect.x;
    let bx = la.screens.iter().find(|s| s.id.device == bn.id()).unwrap().rect.x;
    let dir = if bx > ax { 1.0 } else { -1.0 };

    // ---- 焦点：A 的鼠标推到通往 B 的边缘 → B 成为焦点机；A 的位移在 B 上按毫米注入；按键只到 B。
    a.input
        .set_cursor(Point::new(if dir > 0.0 { 1580.0 } else { 20.0 }, 500.0));
    for _ in 0..20 {
        os_move(&a.input, 10.0 * dir);
        tokio::time::sleep(Duration::from_millis(10)).await;
        if an.input_status().unwrap().focus_info.clone().unwrap().holder == bn.id() {
            break;
        }
    }
    wait_for("B holds focus", 3.0, || {
        bn.input_status().unwrap().focus_info.clone().unwrap().holder == bn.id()
    })
    .await;
    wait_for("A applies remote capture", 3.0, || {
        matches!(a.input.mode(), Some(CaptureMode::Remote { .. }))
    })
    .await;
    // The focus claim can precede FocusEnter and its native entry warp. An
    // ordered key after the crossing flushes that motion before we take the
    // displacement baseline; observing the holder alone can still see x=800.
    let barrier = KeyCode::keyboard(0x68);
    key(&a.input, barrier, true);
    key(&a.input, barrier, false);
    wait_for("B applies the entry and preceding motion", 3.0, || {
        b.input
            .injected()
            .iter()
            .any(|event| matches!(event, InjectEvent::Key { code, pressed: false } if *code == barrier))
    })
    .await;
    b.input.take_injected();
    let before = b.input.cursor_position().unwrap();
    for _ in 0..10 {
        os_move(&a.input, 10.0 * dir);
    }
    key(&a.input, KeyCode::keyboard(0x04), true);
    let bi = b.input.clone();
    wait_for("B receives motion and key", 3.0, || {
        let s = injected(&b.input);
        s.moves >= 1 && s.key_events >= 1
    })
    .await;
    // Motion may coalesce within 8 ms; validate the whole displacement, not
    // one native injection per captured sample. The key follows the final move.
    // 同样的物理尺寸（0.25 mm/单位）：100 单位 → 100 单位。
    let after = bi.cursor_position().unwrap();
    assert!(
        ((after.x - before.x) - 100.0 * dir).abs() < 1.0,
        "{before:?} → {after:?}"
    );
    assert!(
        a.input
            .take_injected()
            .iter()
            .all(|e| !matches!(e, InjectEvent::Key { .. }))
    );

    // Applying the real audio domain must keep the held key and exact session.
    // Capture remains disabled: this verifies control application, not hardware.
    let before_audio = injected(&b.input).key_events;
    let claim = audio::withdraw_exit(&an).await;
    wait_for("audio claim withdrawal on B", 5.0, || {
        bn.device_facts(an.id())
            .and_then(|facts| facts.audio_exit_claim().ok())
            .flatten()
            == Some(claim.clone())
    })
    .await;
    sessions_unchanged("checkpoint 3");
    key(&a.input, KeyCode::keyboard(0x05), true);
    key(&a.input, KeyCode::keyboard(0x05), false);
    wait_for("input after audio configuration", 3.0, || {
        injected(&b.input).key_events >= before_audio + 2
    })
    .await;

    // ---- 剪贴板：文本（内联）与图片（> 64 KiB，经大块数据流拉取）双向同步。
    a.clip
        .copy(vec![ClipData::Text("你好，Oink".into())], Sensitivity::Normal);
    let bc = b.clip.clone();
    wait_for("text on B", 5.0, || {
        bc.items() == vec![ClipData::Text("你好，Oink".into())]
    })
    .await;
    let png: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    b.clip.copy(vec![ClipData::Png(png.clone())], Sensitivity::Normal);
    let ac = a.clip.clone();
    wait_for("image on A", 5.0, || ac.items() == vec![ClipData::Png(png.clone())]).await;
    assert!(!a.node.clipboard_activity().unwrap().is_empty());

    // A text payload exceeds the reliable frame cap; then native descriptors
    // alone require a Low manifest attachment.
    let text = "x".repeat(32 * 1024);
    a.clip.copy(vec![ClipData::Text(text.clone())], Sensitivity::Normal);
    wait_for("32 KiB text on B", 10.0, || {
        bc.items() == vec![ClipData::Text(text.clone())]
    })
    .await;
    let platform = if cfg!(target_os = "macos") {
        oink_foundation::clipboard::ClipPlatform::Mac
    } else {
        oink_foundation::clipboard::ClipPlatform::Windows
    };
    // A few large format identities cross the same manifest boundary without
    // turning this routing scenario into hundreds of sequential payload reads.
    let many: Vec<_> = (0..8)
        .map(|i| ClipData::Custom {
            platform,
            name: format!("org.oink.test.manifest.{}.{i}", "x".repeat(3000)),
            bytes: format!("item-{i}").into_bytes(),
        })
        .collect();
    a.clip.copy(many.clone(), Sensitivity::Normal);
    {
        use oink_clipboard::ClipboardBackend;
        use oink_protocol::v1::{limits, schema::WireDto};
        let inventory = a.clip.capture(an.data_dir()).unwrap().content.unwrap().inventory();
        let wire = oink_clipboard::wire::inventory(&inventory).unwrap();
        assert!(wire.encode(limits::INVENTORY).unwrap().len() > limits::RELIABLE_FRAME);
    }
    wait_for("large offer manifest on B", 10.0, || bc.items() == many).await;

    // ---- File sharing: real QUIC file streams, actual source/received files, fake platform clipboard.
    let source = an.data_dir().join("source.bin");
    let bytes: Vec<u8> = (0..2_000_000u32).map(|i| (i * 11 % 251) as u8).collect();
    std::fs::write(&source, &bytes).unwrap();
    let files = vec![oink_clipboard::FileEntry {
        path: "传输.bin".into(),
        size: bytes.len() as u64,
        is_dir: false,
        modified_ms: None,
        local_path: Some(source.clone()),
    }];
    let previous = bn.clipboard_transfer().unwrap().map(|transfer| transfer.id);
    a.clip.copy(vec![ClipData::Files(files.clone())], Sensitivity::Normal);
    wait_for("file demand available", 5.0, || {
        bn.clipboard_transfer()
            .unwrap()
            .is_some_and(|p| Some(p.id) != previous && p.phase == crate::api::FileTransferPhase::Pending)
    })
    .await;
    // This fake clipboard has no native lazy provider. The receiver must make
    // an explicit request; focus changes are no longer a download trigger.
    bn.fetch_clipboard_files().unwrap();
    wait_for("files on B", 10.0, || {
    bc.items().iter().any(|d| matches!(d, ClipData::Files(f) if f.iter().any(|e| e.local_path.as_ref().is_some_and(|p| p != &source && p.is_file()))))
}).await;
    let received = bc
        .items()
        .into_iter()
        .find_map(|d| match d {
            ClipData::Files(f) => f[0].local_path.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(std::fs::read(received).unwrap(), bytes);
    assert!(bn.clipboard_transfer().unwrap().is_some());

    // A disabled receiver must not download, and a later sensitive local copy must defeat old pending work.
    let mut settings = bn.clipboard_settings().unwrap();
    settings.files = false;
    bn.set_clipboard_settings(settings.into()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.clip
        .copy(vec![ClipData::Text("reset file hash".into())], Sensitivity::Normal);
    wait_for("intervening text", 5.0, || {
        bc.items() == vec![ClipData::Text("reset file hash".into())]
    })
    .await;
    a.clip.copy(vec![ClipData::Files(files.clone())], Sensitivity::Normal);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(bc.items(), vec![ClipData::Text("reset file hash".into())]);
    settings.files = true;
    bn.set_clipboard_settings(settings.into()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.clip
        .copy(vec![ClipData::Text("another reset".into())], Sensitivity::Normal);
    wait_for("second intervening text", 5.0, || {
        bc.items() == vec![ClipData::Text("another reset".into())]
    })
    .await;
    let previous = bn.clipboard_transfer().unwrap().map(|transfer| transfer.id);
    a.clip.copy(vec![ClipData::Files(files.clone())], Sensitivity::Normal);
    wait_for("pending file offer", 5.0, || {
        bn.clipboard_transfer()
            .unwrap()
            .is_some_and(|p| Some(p.id) != previous && p.phase == crate::api::FileTransferPhase::Pending)
    })
    .await;
    let pending = bn.clipboard_transfer().unwrap().unwrap();
    bn.set_clipboard_settings(bn.clipboard_settings().unwrap().into())
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(bn.clipboard_transfer().unwrap().unwrap().id, pending.id);
    bn.clipboard().unwrap().set_paused(true);
    wait_for("pause clears active transfer", 2.0, || {
        bn.clipboard_transfer().unwrap().is_none()
    })
    .await;
    bn.clipboard().unwrap().set_paused(false);
    bn.clipboard().unwrap().settle().await;
    assert!(
        bn.clipboard_transfer().unwrap().is_none(),
        "resume does not continue a half-completed file transfer"
    );
    a.clip
        .copy(vec![ClipData::Text("fresh after resume".into())], Sensitivity::Normal);
    wait_for("fresh text after resume", 5.0, || {
        bc.items() == vec![ClipData::Text("fresh after resume".into())]
    })
    .await;
    let previous = bn.clipboard_transfer().unwrap().map(|transfer| transfer.id);
    a.clip.copy(vec![ClipData::Files(files)], Sensitivity::Normal);
    wait_for("fresh file offer after resume", 5.0, || {
        bn.clipboard_transfer()
            .unwrap()
            .is_some_and(|p| p.id != pending.id && Some(p.id) != previous)
    })
    .await;
    bn.cancel_clipboard_files().unwrap();
    wait_for("cancel transfer state", 2.0, || {
        bn.clipboard_transfer()
            .unwrap()
            .is_some_and(|p| p.phase == crate::api::FileTransferPhase::Cancelled)
    })
    .await;
    b.clip
        .copy(vec![ClipData::Text("local secret".into())], Sensitivity::Concealed);
    bn.fetch_clipboard_files().unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(bc.items(), vec![ClipData::Text("local secret".into())]);

    sessions_unchanged("checkpoint 4");

    // Gain is an owner preference. It synchronizes without changing geometry.
    let version = an.layout().1;
    let screen = ScreenKey {
        device: an.id().into(),
        display: "alpha".into(),
    };
    let target = screen.clone();
    api(&a.node, move |node| node.set_screen_gain(target, 1.5))
        .await
        .unwrap();
    wait_for("owner gain reaches peer", 5.0, || {
        AgentApi::layout(&bn)
            .unwrap()
            .geometry
            .screens
            .iter()
            .any(|item| item.key == screen && item.gain == 1.5)
    })
    .await;
    assert_eq!(an.layout().1, version);
    assert_eq!(bn.layout().1, version);
    assert_layout_files_equal(&an, &bn);

    let target = screen.clone();
    api(&a.node, move |node| node.set_screen_remote_gain(target, 2.0))
        .await
        .unwrap();
    wait_for("remote screen gain reaches peer", 5.0, || {
        AgentApi::layout(&bn)
            .unwrap()
            .geometry
            .screens
            .iter()
            .any(|item| item.key == screen && item.remote_gain == 2.0 && item.gain == 1.5)
    })
    .await;
    assert_eq!(an.layout().1, version);
    assert_eq!(bn.layout().1, version);

    // Group gap mode travels through the settings API and the existing layout
    // publication path, in both directions, and is persisted identically.
    assert!(!an.input_settings().unwrap().gap_travel);
    let mut input = an.input_settings().unwrap();
    input.gap_travel = true;
    api(&an, move |n| n.set_input_settings(input.into())).await.unwrap();
    wait_for("gap setting on B", 5.0, || bn.input_settings().unwrap().gap_travel).await;
    wait_for("gap layout persisted", 5.0, || bn.layout().1 == an.layout().1).await;
    assert!(bn.layout().0.settings.gap_travel);
    assert_layout_files_equal(&an, &bn);
    let mut input = bn.input_settings().unwrap();
    input.gap_travel = false;
    api(&bn, move |n| n.set_input_settings(input.into())).await.unwrap();
    wait_for("gap setting off on A", 5.0, || !an.input_settings().unwrap().gap_travel).await;
    wait_for("gap off layout persisted", 5.0, || bn.layout().1 == an.layout().1).await;
    assert_layout_files_equal(&an, &bn);
    sessions_unchanged("checkpoint 5");

    // Pacing is local source state; edits do not mutate the peer or geometry.
    use crate::api::EventFrequency;
    let version = an.layout().1;
    let peer_frequency = bn.input_settings().unwrap().event_frequency;
    for frequency in [EventFrequency::Unlimited, EventFrequency::Hz125] {
        api(&an, move |node| {
            let mut input = node.input_settings().unwrap();
            input.event_frequency = frequency;
            node.set_input_settings(input.into())
        })
        .await
        .unwrap();
        assert_eq!(an.input_settings().unwrap().event_frequency, frequency);
        assert_eq!(bn.input_settings().unwrap().event_frequency, peer_frequency);
        assert_eq!(
            crate::config::AgentConfig::load_from(an.data_dir())
                .unwrap()
                .settings
                .event_frequency,
            frequency
        );
        assert_eq!(an.layout().1, version);
        assert_layout_files_equal(&an, &bn);
        sessions_unchanged("source pacing preference");
    }

    // ---- 断线：A 按着 A 键时退出 → B 释放它。
    let server = crate::ipc::Server::start_on(an.clone(), 0).await.unwrap();
    let ep: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(an.data_dir().join("ipc.json")).unwrap()).unwrap();
    tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, Write};
        let mut socket = std::net::TcpStream::connect(("127.0.0.1", ep["port"].as_u64().unwrap() as u16)).unwrap();
        socket.write_all(b"{\"token\":\"wrong\",\"stop\":true}\n").unwrap();
        let mut reply = String::new();
        std::io::BufReader::new(socket).read_line(&mut reply).unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&reply)
                .unwrap()
                .get("err")
                .is_some()
        );
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), an.wait_for_stop())
            .await
            .is_err()
    );
    let dir = an.data_dir().to_owned();
    tokio::task::spawn_blocking(move || crate::ipc::Client::connect(&dir).unwrap().stop().unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), an.wait_for_stop())
        .await
        .unwrap();
    server.stop();
    assert!(!an.data_dir().join("ipc.json").exists());
    a.node.shutdown().await;
    wait_for("B releases the key", 5.0, || {
        injected(&b.input).keys.back().map(String::as_str) == Some("0x70004↑")
    })
    .await;
    wait_for("B back to its own focus", 5.0, || {
        bn.input_status().unwrap().focus_info.clone().unwrap().holder == bn.id()
    })
    .await;
    api(&bn, |n| n.leave_group()).await.unwrap();
    wait_for("old group screens removed", 5.0, || {
        bn.layout().0.screens.iter().all(|s| s.id.device == bn.id())
    })
    .await;
    b.node.shutdown().await;
}

// In-process renderer probe; the renderer is not exposed through AgentApi or IPC.
trait RendererProbe {
    fn overlay(&self) -> crate::api::ApiResult<Option<crate::api::Overlay>>;
}
impl RendererProbe for Node {
    fn overlay(&self) -> crate::api::ApiResult<Option<crate::api::Overlay>> {
        Ok(crate::display_overlay::overlay(self))
    }
}
struct Injected {
    moves: usize,
    key_events: usize,
    keys: std::collections::VecDeque<String>,
}
fn injected(input: &oink_test_support::input::FakeInput) -> Injected {
    let mut stats = Injected {
        moves: 0,
        key_events: 0,
        keys: Default::default(),
    };
    for event in input.injected() {
        match event {
            InjectEvent::MoveTo { .. } | InjectEvent::MoveToWithDelta { .. } => stats.moves += 1,
            InjectEvent::Key { code, pressed } => {
                stats.key_events += 1;
                stats
                    .keys
                    .push_back(format!("{:#x}{}", code.0, if pressed { "↓" } else { "↑" }));
            }
            _ => {}
        }
    }
    stats
}

#[test]
fn game_badges_follow_only_the_shortcut_target_and_each_owner_preference() {
    crate::test_watchdog::run(
        module_path!(),
        "game_badges_follow_only_the_shortcut_target_and_each_owner_preference",
        Some(4),
        async {
            use crate::config::GameModePreference;
            let a = start("game-a", 0).await;
            let b = start("game-b", 0).await;
            common::pair(&a.node, &b.node).await;
            common::active(&[&a.node, &b.node]).await;
            // Establish one group focus explicitly; this regression tests the
            // shortcut handler and owner facts, independent of native key capture.
            api(&a.node, |node| node.set_focus_locked(true)).await.unwrap();
            wait_for("shared focus on source", 5., || {
                b.node.focus().is_some_and(|focus| focus.device.0 == a.node.id())
            })
            .await;
            let badges = |expected: [bool; 2]| {
                [&a.node, &b.node].into_iter().all(|observer| {
                    let view = observer.device_cards().unwrap();
                    [a.node.id(), b.node.id()]
                        .into_iter()
                        .zip(expected)
                        .all(|(id, active)| {
                            view.cards
                                .iter()
                                .find(|c| c.device.0 == id)
                                .and_then(|c| c.status.as_ref())
                                .is_some_and(|s| s.game.active == Some(active))
                        })
                })
            };
            // Invoke the actual shortcut handler from a source whose focus is remote.
            api(&b.node, |node| node.on_hotkey("toggle_game_mode")).await;
            wait_for("shortcut badges only the current focus owner", 2., || {
                badges([true, false])
                    && [&a.node, &b.node].into_iter().all(|node| {
                        let runtime = node.game_mode().unwrap();
                        runtime.holder.0 == a.node.id() && runtime.active
                    })
            })
            .await;
            for node in [&a.node, &b.node] {
                assert_eq!(node.settings().game_mode, GameModePreference::Auto);
                let runtime = node.game_mode().unwrap();
                assert_eq!(runtime.holder.0, a.node.id());
                assert!(runtime.active);
            }
            api(&b.node, |node| node.on_hotkey("toggle_game_mode")).await;
            wait_for("shortcut off clears only the focus owner's override", 2., || {
                badges([false, false])
            })
            .await;
            for node in [&a.node, &b.node] {
                api(node, |node| {
                    let _gate = node.owner_gate();
                    node.edit_local_input_settings(&serde_json::json!({}), |s| s.game_mode = GameModePreference::On)
                })
                .await
                .unwrap();
            }
            wait_for("both owner game badges with one group focus", 2., || {
                badges([true, true])
            })
            .await;
            let target = b.node.id();
            api(&b.node, move |node| node.lock_focus_to(target.into()))
                .await
                .unwrap();
            wait_for("both badges survive focus handoff", 2., || {
                a.node.focus().is_some_and(|focus| focus.device.0 == target) && badges([true, true])
            })
            .await;
            api(&a.node, |node| {
                let _gate = node.owner_gate();
                node.edit_local_input_settings(&serde_json::json!({}), |s| s.game_mode = GameModePreference::Off)
            })
            .await
            .unwrap();
            wait_for("nonfocus owner's badge clears on both observers", 2., || {
                badges([false, true])
            })
            .await;
            api(&b.node, |node| {
                let _gate = node.owner_gate();
                node.edit_local_input_settings(&serde_json::json!({}), |s| s.game_mode = GameModePreference::Off)
            })
            .await
            .unwrap();
            wait_for("both badges clear", 2., || badges([false, false])).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

#[test]
fn screen_card_commands_release_the_targets_lock_from_either_member() {
    crate::test_watchdog::run(
        module_path!(),
        "screen_card_commands_release_the_targets_lock_from_either_member",
        Some(4),
        async {
            use crate::device_cards::{CardAction, CursorIcon, DeviceControlReply, DeviceControlResult, SharingMode};
            for remote in [true, false] {
                for shared in [true, false] {
                    let a = start("screen-a", 0).await;
                    let b = start("screen-b", 0).await;
                    common::pair(&a.node, &b.node).await;
                    // Input attachment and retained screen rectangles can precede
                    // the executable topology's admitted owner facts.
                    wait_for("screen focus input and topology ready", 5., || {
                        use oink_routing::LayoutQuery;
                        [&a.node, &b.node].into_iter().all(|node| {
                            node.input_status().unwrap().peers.iter().any(|peer| peer.online)
                                && node.inner.group.get().is_some_and(|group| {
                                    let layout = group.router_layout().load();
                                    [a.node.id(), b.node.id()]
                                        .into_iter()
                                        .all(|device| layout.topology.contains_device(device))
                                })
                        })
                    })
                    .await;
                    let requester = if remote { &a.node } else { &b.node };
                    let target = b.node.id();
                    key(&a.input, KeyCode::keyboard(4), true);
                    key(&a.input, KeyCode::keyboard(4), false);
                    wait_for("source participates before target handoff", 5., || {
                        b.node.focus().is_some_and(|focus| focus.device.0 == a.node.id())
                    })
                    .await;
                    api(requester, move |node| node.lock_focus_to(target.into()))
                        .await
                        .unwrap();
                    wait_for("target owner lock on both members", 5., || {
                        [&a.node, &b.node]
                            .into_iter()
                            .all(|node| node.input_status().unwrap().locked_device == Some(target))
                    })
                    .await;
                    let mut token = None;
                    wait_for("locked target screen card", 5., || {
                        let view = requester.device_cards().unwrap();
                        let Some(status) = view
                            .cards
                            .iter()
                            .find(|card| card.device.0 == target)
                            .and_then(|card| card.status.as_ref())
                        else {
                            return false;
                        };
                        if status.cursor.icon != CursorIcon::Locked {
                            return false;
                        }
                        let action = if shared {
                            &status.actions.screen_shared
                        } else {
                            &status.actions.screen_local
                        };
                        if let CardAction::Invoke { token: current, .. } = action {
                            token = Some(current.clone());
                            true
                        } else {
                            false
                        }
                    })
                    .await;
                    let token = token.unwrap();
                    let id = crate::device_controls::runtime::fresh_request_id();
                    requester.invoke_device_card_action(id.clone(), token.clone()).unwrap();
                    wait_for("screen command typed owner completion", 5., || {
                        matches!(
                            requester.invoke_device_card_action(id.clone(), token.clone()).unwrap(),
                            DeviceControlReply::Completed(_)
                        )
                    })
                    .await;
                    let DeviceControlReply::Completed(completed) =
                        requester.invoke_device_card_action(id, token).unwrap()
                    else {
                        unreachable!()
                    };
                    assert!(
                        matches!(
                            completed.result,
                            DeviceControlResult::Applied | DeviceControlResult::NoChange
                        ),
                        "{completed:?}"
                    );
                    wait_for("screen command releases lock on both members", 2., || {
                        [&a.node, &b.node].into_iter().all(|node| {
                            let view = node.device_cards().unwrap();
                            node.input_status().unwrap().locked_device.is_none()
                                && view
                                    .cards
                                    .iter()
                                    .find(|card| card.device.0 == target)
                                    .and_then(|card| card.status.as_ref())
                                    .is_some_and(|status| {
                                        status.cursor.icon != CursorIcon::Locked
                                            && status.screen.configured_mode
                                                == Some(if shared {
                                                    SharingMode::Shared
                                                } else {
                                                    SharingMode::Local
                                                })
                                    })
                        })
                    })
                    .await;
                    assert_eq!(b.node.settings().screen_shared, shared);
                    assert!(a.node.settings().screen_shared);
                    a.node.shutdown().await;
                    b.node.shutdown().await;
                }
            }
        },
    );
}

#[test]
fn focus_lock_api_and_overview_events_name_the_remote_target_on_both_members() {
    crate::test_watchdog::run(
        module_path!(),
        "focus_lock_api_and_overview_events_name_the_remote_target_on_both_members",
        Some(4),
        focus_lock_api_and_overview_events_name_the_remote_target_on_both_members_scenario(),
    );
}

async fn focus_lock_api_and_overview_events_name_the_remote_target_on_both_members_scenario() {
    use crate::api::{ApiEvent, DeviceKey, LiveViews};
    use std::sync::Mutex;
    let a = start("lka", 0).await;
    let b = start("lkb", 0).await;
    common::pair(&a.node, &b.node).await;
    wait_for("attached input", 5., || {
        a.node.input_status().unwrap().peers.iter().any(|p| p.online)
            && b.node.input_status().unwrap().peers.iter().any(|p| p.online)
    })
    .await;
    wait_for("lock topology includes both devices", 5., || {
        use oink_routing::LayoutQuery;
        [&a.node, &b.node].into_iter().all(|node| {
            let layout = node.link_config().unwrap().router_layout().load();
            [a.node.id(), b.node.id()]
                .into_iter()
                .all(|device| layout.topology.contains_device(device))
        })
    })
    .await;
    key(&b.input, KeyCode::keyboard(4), true);
    key(&b.input, KeyCode::keyboard(4), false);
    wait_for("target has focus", 5., || {
        a.node.focus().is_some_and(|f| f.device == DeviceKey(b.node.id()))
    })
    .await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriptions: Vec<_> = [&a.node, &b.node]
        .into_iter()
        .map(|node| {
            let sink = events.clone();
            let id = node.id();
            node.subscribe_views(
                Arc::new(move |event| {
                    if let ApiEvent::Overview(o) = event {
                        sink.lock().unwrap().push((id, o.focus_lock.as_ref().map(|d| d.id)));
                    }
                }),
                LiveViews::default(),
            )
        })
        .collect();
    api(&a.node, |n| n.set_focus_locked(true)).await.unwrap();
    wait_for("group lock status", 5., || {
        [&a.node, &b.node].into_iter().all(|node| {
            node.overview()
                .unwrap()
                .focus_lock
                .is_some_and(|d| d.id == DeviceKey(b.node.id()))
        })
    })
    .await;
    wait_for("lock UI events", 5., || {
        [&a.node, &b.node].into_iter().all(|node| {
            events
                .lock()
                .unwrap()
                .contains(&(node.id(), Some(DeviceKey(b.node.id()))))
        })
    })
    .await;
    assert_eq!(a.node.focus().unwrap().device, DeviceKey(b.node.id()));
    assert_eq!(
        b.node.overview().unwrap().focus_lock.unwrap().name,
        b.node.overview().unwrap().this_device.name
    );
    api(&a.node, |n| n.return_pointer_home()).await.unwrap();
    wait_for("explicit recall unlocks the group", 5., || {
        [&a.node, &b.node]
            .into_iter()
            .all(|n| n.overview().unwrap().focus_lock.is_none())
            && a.node.focus().is_some_and(|f| f.is_local)
    })
    .await;
    for (node, subscription) in [&a.node, &b.node].into_iter().zip(subscriptions) {
        node.unsubscribe(subscription);
    }
    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[test]
#[ignore = "requires a dedicated quiet timing run; process-global idle statistics qualification"]
fn idle_two_nodes_emit_only_requested_link_statistics() {
    const CHILD: &str = "OINK_IDLE_EVENT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Node log listeners observe the process-global logger. Other parallel
        // fixtures legitimately log state changes: give this pair its own logger,
        // keeping Log in the assertion instead of filtering it out.
        let _guard = crate::test_watchdog::Scenario::new(module_path!(), "idle subprocess");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "two_nodes_tests::idle_two_nodes_emit_only_requested_link_statistics",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success());
        return;
    }
    crate::logs::init();
    crate::test_watchdog::run(module_path!(), "idle pair", Some(4), idle_two_nodes_scenario());
}

async fn idle_two_nodes_scenario() {
    use crate::api::{ApiEvent, LiveViews};
    use std::sync::Mutex;
    let a = start("idu-a", 0).await;
    let b = start("idu-b", 0).await;
    common::pair(&a.node, &b.node).await;
    common::active(&[&a.node, &b.node]).await;
    crate::test_watchdog::until("idle pair startup complete", || {
        [&a.node, &b.node].iter().all(|node| {
            node.inner.platform_ready.load(std::sync::atomic::Ordering::Acquire)
                && !node.diagnostics_status().unwrap().recovering
                && node.audio_status().unwrap().device_states.len() == 2
                && node.nearby().unwrap().rows.is_empty()
        })
    })
    .await;
    #[derive(Default)]
    struct Observation {
        bulk_ready: [bool; 2],
        permissions_ready: [bool; 2],
        counting: bool,
        counts: [std::collections::BTreeMap<String, usize>; 2],
        cards: [Option<crate::device_cards::DeviceCardsView>; 2],
        card_changes: Vec<(
            usize,
            crate::device_cards::DeviceCardsView,
            crate::device_cards::DeviceCardsView,
        )>,
    }
    let observation = Arc::new(Mutex::new(Observation::default()));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let subscriptions = [&a.node, &b.node]
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            let observation = observation.clone();
            let send = send.clone();
            node.subscribe_views(
                Arc::new(move |event| {
                    let value = serde_json::to_value(event).unwrap();
                    let name = value["type"].as_str().unwrap().to_owned();
                    if name != "stats" && name != "input_stats" {
                        eprintln!("UI_IDLE_EVENT node={index} event={value}");
                    }
                    let mut observed = observation.lock().unwrap();
                    if let ApiEvent::DeviceCards(view) = event {
                        let mut semantic = view.clone();
                        semantic.revision = crate::device_cards::DecimalU64(0);
                        semantic.generated_at_ms = 0;
                        for card in &mut semantic.cards {
                            if let crate::device_cards::CardConnection::Online { signal } = &mut card.connection {
                                *signal = crate::device_cards::Signal::online(None);
                            }
                        }
                        if observed.counting
                            && let Some(previous) = observed.cards[index].clone()
                            && previous != semantic
                        {
                            // Assert on the test thread after shutdown. Panicking
                            // inside an owner callback poisons the fixture mutex
                            // and prevents the owners from replying to shutdown.
                            observed.card_changes.push((index, previous, semantic.clone()));
                        }
                        observed.cards[index] = Some(semantic);
                        let _ = send.send(index);
                    }
                    if let ApiEvent::Permissions(_) = event {
                        // This fixture disables native permission sensors. Observe
                        // their initial snapshot; it cannot acquire a native grant.
                        observed.permissions_ready[index] = true;
                        let _ = send.send(index);
                    }
                    if let ApiEvent::Group(group) = event {
                        observed.bulk_ready[index] = group.members.len() == 2
                            && group.members.iter().filter(|member| !member.is_self).all(|member| {
                                member
                                    .link
                                    .as_ref()
                                    .is_some_and(|link| link.bulk.as_deref() == Some("available"))
                            });
                        let _ = send.send(index);
                    }
                    // End each node's observation at its sixth source-owned sample.
                    if observed.counting && observed.counts[index].get("stats").copied().unwrap_or(0) < 6 {
                        *observed.counts[index].entry(name.clone()).or_default() += 1;
                        if name == "stats" {
                            let _ = send.send(index);
                        }
                    }
                }),
                LiveViews {
                    stats: true,
                    ..Default::default()
                },
            )
        })
        .collect::<Vec<_>>();
    // Input/audio readiness does not acknowledge the independent TCP bootstrap.
    // Begin only after both subscribers received its semantic group change;
    // queries alone could still leave that startup notification in flight.
    // Include the initial permission snapshot before measuring steady-state silence.
    crate::test_watchdog::phase("idle pair bulk readiness delivered");
    eprintln!("UI_IDLE_WAIT_BULK");
    loop {
        {
            let mut observed = observation.lock().unwrap();
            if observed.bulk_ready.into_iter().all(|ready| ready)
                && observed.permissions_ready.into_iter().all(|ready| ready)
                && observed.cards.iter().all(|view| {
                    view.as_ref().is_some_and(|view| {
                        view.cards.iter().all(|card| {
                            card.status.as_ref().is_some_and(|status| {
                                status.speaker.activity == crate::device_cards::SpeakerActivity::Idle
                            })
                        })
                    })
                })
            {
                observed.counting = true;
                break;
            }
        }
        receive.recv().await.unwrap();
    }
    crate::test_watchdog::phase("six idle link-statistics samples");
    eprintln!("UI_IDLE_WINDOW_START");
    loop {
        receive.recv().await.unwrap();
        if observation
            .lock()
            .unwrap()
            .counts
            .iter()
            .all(|node| node.get("stats") == Some(&6))
        {
            break;
        }
    }
    for (index, node) in [&a.node, &b.node].into_iter().enumerate() {
        node.unsubscribe(subscriptions[index]);
    }
    let counts = observation.lock().unwrap().counts.clone();
    let card_changes = observation.lock().unwrap().card_changes.clone();
    a.node.shutdown().await;
    b.node.shutdown().await;
    assert!(
        card_changes.is_empty(),
        "idle cards changed beyond measurements: {card_changes:?}"
    );
    eprintln!("UI_IDLE six_source_samples={counts:?}");
    for mut node in counts {
        // InputStats uses the same subscribed source clock. Ending at the sixth
        // Stats callback excludes its following input sample; subscription can
        // bisect the first turn, so five or six input samples are delivered.
        assert!(matches!(node.remove("input_stats"), Some(5 | 6)));
        // Complete card snapshots carry RTT on the network sample cadence.
        // The callback above verifies every non-measurement field stays equal.
        node.remove("device_cards");
        assert_eq!(node, std::collections::BTreeMap::from([("stats".to_owned(), 6)]));
    }
}

#[test]
fn diagnostics_control_reaches_ui_without_statistics_or_frontend_poll() {
    crate::test_watchdog::run(
        module_path!(),
        "diagnostics_control_reaches_ui_without_statistics_or_frontend_poll",
        Some(4),
        diagnostics_control_reaches_ui_without_statistics_or_frontend_poll_scenario(),
    );
}

async fn diagnostics_control_reaches_ui_without_statistics_or_frontend_poll_scenario() {
    use crate::api::{ApiEvent, LiveViews};
    let a = start("diag-ui", 0).await;
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let sub = a.node.subscribe_views(
        Arc::new(move |event| {
            if let ApiEvent::DiagnosticsStatus(status) = event
                && !status.configured_enabled
            {
                let _ = send.send(Instant::now());
            }
        }),
        LiveViews::default(),
    );
    let node = a.node.clone();
    let at = Instant::now();
    tokio::task::spawn_blocking(move || node.set_diagnostics_enabled(false))
        .await
        .unwrap()
        .unwrap();
    let observed = receive.recv().await.unwrap();
    assert!(!a.node.diagnostics_status().unwrap().configured_enabled);
    a.node.unsubscribe(sub);
    a.node.shutdown().await;
    eprintln!("UI_LATENCY diagnostics={:?}", observed.duration_since(at));
}

#[test]
fn clipboard_source_settles_then_publishes_both_ui_events() {
    crate::test_watchdog::run(
        module_path!(),
        "clipboard_source_settles_then_publishes_both_ui_events",
        Some(4),
        clipboard_source_settles_then_publishes_both_ui_events_scenario(),
    );
}

async fn clipboard_source_settles_then_publishes_both_ui_events_scenario() {
    use crate::api::{ApiEvent, LiveViews};
    let a = start("clip-a", 0).await;
    let b = start("clip-b", 0).await;
    common::pair(&a.node, &b.node).await;
    common::active(&[&a.node, &b.node]).await;
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let subscriptions: Vec<_> = [&a.node, &b.node]
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            let send = send.clone();
            node.subscribe_views(
                Arc::new(move |event| {
                    if let ApiEvent::ClipActivity(activity) = event
                        && activity.bytes == 19
                    {
                        let _ = send.send((index, Instant::now()));
                    }
                }),
                LiveViews::default(),
            )
        })
        .collect();
    let at = Instant::now();
    a.clip
        .copy(vec![ClipData::Text("clipboard-immediate".into())], Sensitivity::Normal);
    let mut latency = [None, None];
    tokio::time::timeout(Duration::from_secs(10), async {
        while latency.iter().any(Option::is_none) {
            let (index, when) = receive.recv().await.unwrap();
            latency[index].get_or_insert(when.duration_since(at));
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "clipboard UI events: {latency:?}; source={:?}, target={:?}",
            a.node.clipboard_activity(),
            b.node.clipboard_activity()
        )
    });
    for (i, node) in [&a.node, &b.node].into_iter().enumerate() {
        node.unsubscribe(subscriptions[i]);
    }
    a.node.shutdown().await;
    b.node.shutdown().await;
    eprintln!("SOURCE_SETTLE clipboard_change_to_local_peer_ui={latency:?}");
    // The source quiet window is covered by the pure clipboard Ordering tests.
    // Here the payload-specific events prove both real transports deliver after it.
}

mod cache;

#[test]
fn remote_card_modes_apply_persist_and_publish_without_editing_requester() {
    crate::test_watchdog::run(
        module_path!(),
        "remote_card_modes_apply_persist_and_publish_without_editing_requester",
        Some(4),
        async {
            use crate::device_cards::{CardAction, DeviceControlReply, DeviceControlResult};
            use oink_foundation::input::InputMode;
            let a = start("mode-a", 0).await;
            let b = start("mode-b", 0).await;
            common::pair(&a.node, &b.node).await;
            let focus = a.node.focus();
            for keyboard in [false, true] {
                let mut token = None;
                wait_for("current remote owner resource action", 5., || {
                    let view = b.node.device_cards().unwrap();
                    let Some(card) = view
                        .cards
                        .iter()
                        .find(|card| card.device.0 == a.node.id())
                        .and_then(|card| card.status.as_ref())
                    else {
                        return false;
                    };
                    let action = if keyboard {
                        &card.keyboard.resource.action
                    } else {
                        &card.mouse.action
                    };
                    if let CardAction::Invoke { token: current, .. } = action {
                        token = Some(current.clone());
                        true
                    } else {
                        false
                    }
                })
                .await;
                let token = token.unwrap();
                let id = crate::device_controls::runtime::fresh_request_id();
                b.node.invoke_device_card_action(id.clone(), token.clone()).unwrap();
                wait_for("remote owner completion", 5., || {
                    matches!(
                        b.node.invoke_device_card_action(id.clone(), token.clone()).unwrap(),
                        DeviceControlReply::Completed(_)
                    )
                })
                .await;
                let DeviceControlReply::Completed(result) =
                    b.node.invoke_device_card_action(id, token.clone()).unwrap()
                else {
                    panic!("completion missing")
                };
                assert!(
                    matches!(
                        result.result,
                        DeviceControlResult::Applied | DeviceControlResult::NoChange
                    ),
                    "{result:?}"
                );
                let stale_id = crate::device_controls::runtime::fresh_request_id();
                wait_for("stale remote owner revision refused", 5., || {
                    matches!(
                        b.node
                            .invoke_device_card_action(stale_id.clone(), token.clone())
                            .unwrap(),
                        DeviceControlReply::Completed(crate::device_cards::DeviceControlCompleted {
                            result: DeviceControlResult::Refused {
                                reason: crate::device_cards::DeviceControlFailure::StaleRevision,
                                ..
                            },
                            ..
                        })
                    )
                })
                .await;
            }
            assert_eq!(a.node.input_status().unwrap().mouse_mode, InputMode::Local);
            assert_eq!(a.node.input_status().unwrap().keyboard_mode, InputMode::Local);
            assert_eq!(a.node.focus(), focus);
            let saved = crate::config::AgentConfig::load_from(a._dir.path()).unwrap();
            assert_eq!(saved.settings.routing.mouse_mode, InputMode::Local);
            assert_eq!(saved.settings.routing.keyboard_mode, InputMode::Local);
            wait_for("peer observes applied owner modes", 5., || {
                b.node.group().unwrap().members.iter().any(|member| {
                    member.device.id.0 == a.node.id()
                        && member.mouse_mode == Some(InputMode::Local)
                        && member.keyboard_mode == Some(InputMode::Local)
                })
            })
            .await;
            assert_eq!(b.node.input_status().unwrap().mouse_mode, InputMode::Shared);
            assert_eq!(b.node.input_status().unwrap().keyboard_mode, InputMode::Shared);
            api(&a.node, |node| {
                use oink_protocol::v1::{RawValue, dto};
                let option = dto::LocalOption::InputTransform;
                crate::node::lock(&node.inner.config)
                    .update_owner(|store| {
                        let mut raw = store.value(option).unwrap();
                        let RawValue::Map(map) = &mut raw else { unreachable!() };
                        map.insert(99, RawValue::Text("future transform".into()));
                        let RawValue::Map(target) = map.get_mut(&2).unwrap() else {
                            unreachable!()
                        };
                        target.insert(99, RawValue::Text("future target".into()));
                        store
                            .replace_value(option, store.revision(option).unwrap(), raw)
                            .map_err(|_| crate::config::store::StoreError::Invalid)
                    })
                    .unwrap();
                node.publish_owner_preferences().unwrap();
            })
            .await;
            wait_for("remote transform extension publication", 5., || {
                use oink_protocol::v1::dto;
                b.node
                    .device_facts(a.node.id())
                    .and_then(|facts| facts.preferences().ok().flatten())
                    .and_then(|items| {
                        items
                            .0
                            .into_iter()
                            .find(|item| item.option() == Ok(dto::LocalOption::InputTransform))
                    })
                    .and_then(|item| item.value().ok())
                    .is_some_and(|raw| raw.as_map().is_ok_and(|map| map.contains_key(&99)))
            })
            .await;
            let requester_name = b.node.device_name(b.node.id());
            let requester_transform = b.node.settings().input_transform;
            let mut transform = a.node.settings().input_transform;
            transform.windows.reverse_wheel = !transform.windows.reverse_wheel;
            transform.windows.keyboard.preset = oink_foundation::input_transform::KeyPreset::Custom;
            transform
                .windows
                .keyboard
                .table
                .push(oink_foundation::input_transform::KeyMapping {
                    from: KeyCode::keyboard(4),
                    to: KeyCode::keyboard(5),
                });
            transform
                .overrides
                .push(oink_foundation::input_transform::TargetOverride {
                    target: b.node.id(),
                    reverse_wheel: Some(true),
                    keyboard: None,
                });
            let expected_transform = transform.clone();
            let editor = b.node.clone();
            let owner = a.node.id();
            tokio::task::spawn_blocking(move || {
                editor.rename_device(owner.into(), "remote-owner".into()).unwrap();
                editor.set_input_transform(owner.into(), transform.into()).unwrap();
            })
            .await
            .unwrap();
            let saved = crate::config::AgentConfig::load_from(a._dir.path()).unwrap();
            assert_eq!(saved.settings.device_name.as_deref(), Some("remote-owner"));
            assert_eq!(saved.settings.input_transform, expected_transform);
            let raw = saved
                .owner()
                .value(oink_protocol::v1::dto::LocalOption::InputTransform)
                .unwrap();
            assert_eq!(
                raw.as_map().unwrap().get(&99),
                Some(&oink_protocol::v1::RawValue::Text("future transform".into()))
            );
            assert_eq!(
                raw.as_map().unwrap()[&2].as_map().unwrap().get(&99),
                Some(&oink_protocol::v1::RawValue::Text("future target".into()))
            );
            assert_eq!(a.node.settings().input_transform, expected_transform);
            assert_eq!(b.node.settings().input_transform, requester_transform);
            assert_eq!(b.node.device_name(b.node.id()), requester_name);
            wait_for("remote owner name publication", 5., || {
                b.node.device_name(owner) == "remote-owner"
            })
            .await;
            let mut removed = expected_transform;
            removed.windows.keyboard.table.clear();
            removed.overrides.clear();
            let expected = removed.clone();
            // The local API uses the same keyed patch path, including removals.
            api(&a.node, move |node| {
                node.set_input_transform(node.id().into(), removed.into())
            })
            .await
            .unwrap();
            assert_eq!(
                crate::config::AgentConfig::load_from(a._dir.path())
                    .unwrap()
                    .settings
                    .input_transform,
                expected
            );
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

#[test]
fn remote_pause_and_emergency_clear_only_owner_disables_and_publish_release() {
    crate::test_watchdog::run(
        module_path!(),
        "remote_pause_and_emergency_clear_only_owner_disables_and_publish_release",
        Some(4),
        async {
            use crate::device_cards::{CardAction, DeviceControlReply, DeviceControlResult};
            use oink_foundation::input::InputMode;
            use oink_protocol::v1::{dto::LocalOption, schema::WireValue};
            let a = start("disable-owner", 0).await;
            let b = start("pause-requester", 0).await;
            common::pair(&a.node, &b.node).await;
            api(&b.node, |node| {
                node.edit_device_option(node.id().into(), LocalOption::MouseMode, 1u32.write())
                    .unwrap()
            })
            .await;
            for emergency in [false, true] {
                api(&a.node, |node| {
                    node.edit_device_option(node.id().into(), LocalOption::MouseMode, 2u32.write())
                        .unwrap();
                    node.edit_device_option(node.id().into(), LocalOption::KeyboardDisabled, true.write())
                        .unwrap();
                })
                .await;
                if emergency {
                    b.node
                        .pause_all(crate::device_controls::runtime::fresh_request_id())
                        .unwrap();
                } else {
                    let mut token = None;
                    wait_for("remote pause action", 5., || {
                        token = b
                            .node
                            .device_cards()
                            .unwrap()
                            .cards
                            .iter()
                            .find(|card| card.device.0 == a.node.id())
                            .and_then(|card| card.pause.as_ref())
                            .and_then(|pause| match &pause.action {
                                CardAction::Invoke { token, .. } => Some(token.clone()),
                                _ => None,
                            });
                        token.is_some()
                    })
                    .await;
                    let token = token.unwrap();
                    let id = crate::device_controls::runtime::fresh_request_id();
                    b.node.invoke_device_card_action(id.clone(), token.clone()).unwrap();
                    wait_for("received pause receipt", 5., || {
                        match b.node.invoke_device_card_action(id.clone(), token.clone()).unwrap() {
                            DeviceControlReply::Completed(result) => {
                                assert_eq!(result.result, DeviceControlResult::Applied);
                                true
                            }
                            _ => false,
                        }
                    })
                    .await;
                }
                wait_for("received pause durable release", 5., || {
                    let pause = a.node.pause_status();
                    let settings = a.node.settings();
                    pause.paused
                        && !pause.settling
                        && !settings.keyboard_disabled
                        && settings.routing.mouse_mode == InputMode::Shared
                })
                .await;
                let disk = crate::config::AgentConfig::load_from(a._dir.path()).unwrap();
                assert!(!disk.settings.keyboard_disabled);
                assert_eq!(disk.settings.routing.mouse_mode, InputMode::Shared);
                wait_for("release reaches requester facts", 5., || {
                    b.node.group().unwrap().members.iter().any(|member| {
                        member.device.id.0 == a.node.id()
                            && member.keyboard_disabled == Some(false)
                            && member.mouse_mode == Some(InputMode::Shared)
                    })
                })
                .await;
                assert_eq!(b.node.settings().routing.mouse_mode, InputMode::Local);
                a.node
                    .set_local_paused(crate::device_controls::runtime::fresh_request_id(), false)
                    .unwrap();
                wait_for("owner resume stays enabled", 5., || !a.node.pause_status().paused).await;
                assert!(!a.node.input_status().unwrap().keyboard_disabled);
                assert_eq!(a.node.input_status().unwrap().mouse_mode, InputMode::Shared);
            }
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}
