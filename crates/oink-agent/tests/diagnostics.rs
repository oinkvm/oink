#![cfg(debug_assertions)]
//! Real QUIC/TLS + fake input: these tests do not claim physical desktop validation.
use oink_agent as test_agent;
mod common;
use oink_agent::{
    api::{AgentApi, ApiEvent, Placement},
    node::{Node, NodeOptions},
};
use oink_foundation::{
    Point, Rect, Size,
    display::DisplayInfo,
    input::{CapturedEvent, InputEvent, KeyCode, SourceKind},
};
use oink_protocol::v1::{
    self, dto as w,
    schema::{Open, WireDto},
};
use oink_test_support::input::FakeInput;
use std::{
    io::Read,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
fn saved_packages(node: &Node) -> Vec<String> {
    std::fs::read_dir(node.diagnostics_folder().unwrap())
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok().filter(|n| n.ends_with(".zip")))
        .collect()
}
async fn start(dir: &Path, name: &str, port: u16) -> (Node, FakeInput) {
    let input = FakeInput::new();
    input.set_cursor(Point::new(400.0, 300.0));
    let mut config = oink_agent::config::AgentConfig::load_from(dir).unwrap();
    config.settings.routing.damping.platform_profile = oink_routing::damping::DampingProfile::DepthOnly { depth: 0.0 };
    config.save().unwrap();
    let node = Node::start_with_fake_platform(NodeOptions {
        net: oink_test_support::network_config(dir.into(), name, port),
        input: Arc::new(input.clone()),
        displays: vec![DisplayInfo {
            key: format!("secret-screen-{name}"),
            name: format!("Private screen {name}"),
            bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
            pixel_width: 800,
            pixel_height: 600,
            reference_extent: Some(Size::new(300.0 / 686., 200.0 / 686.)),
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    })
    .await
    .unwrap();
    wait("diagnostic recovery", || !node.diagnostics_status().unwrap().recovering).await;
    (node, input)
}
async fn call<T: Send + 'static>(n: &Node, f: impl FnOnce(&Node) -> T + Send + 'static) -> T {
    let n = n.clone();
    tokio::task::spawn_blocking(move || f(&n)).await.unwrap()
}
async fn wait(what: &str, mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if condition() {
            return;
        }
        if Instant::now() >= until {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out: {what}");
}
fn feed(input: &FakeInput, code: KeyCode, pressed: bool) {
    input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::Key { code, pressed },
        source: SourceKind::Keyboard,
        time_us: 42,
        cursor: None,
    });
}
fn content(path: &str) -> String {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut all = Vec::new();
    for i in 0..zip.len() {
        zip.by_index(i).unwrap().read_to_end(&mut all).unwrap();
    }
    String::from_utf8_lossy(&all).into_owned()
}
fn wire_environment(zip: &mut zip::ZipArchive<std::fs::File>, alias: &str) -> w::Snapshot {
    let mut bytes = Vec::new();
    zip.by_name(&format!("devices/{alias}/current-environment.cbor"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    w::Snapshot::decode(&bytes, oink_agent::diagnostics::MAX_SEGMENT_RAW).unwrap()
}
fn scalar(values: Vec<w::NamedScalar>, name: &str) -> v1::RawValue {
    values
        .into_iter()
        .find(|v| v.name().unwrap().0 == name)
        .unwrap()
        .value()
        .unwrap()
}
fn device_count(path: &str) -> usize {
    let zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    zip.file_names()
        .map(Result::unwrap)
        .filter(|n| n.ends_with("/timeline.ndjson"))
        .count()
}
fn manifest(path: &str) -> serde_json::Value {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap()
}
fn low_operations(node: &Node) -> u64 {
    node.links()
        .unwrap()
        .links()
        .links
        .iter()
        .filter_map(|l| node.links().unwrap().low_status(l.link))
        .map(|s| s.connections_opened + s.connections_reused)
        .sum()
}
#[test]
fn each_member_exports_its_current_runtime_environment_without_a_local_save() {
    common::watchdog::run(
        module_path!(),
        "each_member_exports_its_current_runtime_environment_without_a_local_save",
        Some(4),
        async {
            let temp = tempfile::tempdir().unwrap();
            let (a, _) = start(&temp.path().join("a"), "secret-computer-alpha", 0).await;
            let (b, _) = start(&temp.path().join("b"), "secret-computer-beta", 0).await;
            let (c, _) = start(&temp.path().join("c"), "secret-computer-gamma", 0).await;
            common::pair(&a, &b).await;
            common::pair(&a, &c).await;
            common::active(&[&a, &b, &c]).await;
            wait("three-screen environments", || {
                [&a, &b, &c].iter().all(|n| n.layout().0.screens.len() == 3)
            })
            .await;
            // Change AFTER startup/rekey. A startup-only snapshot must not pass,
            // nor may peers inherit the exporter's settings during collection.
            for round in 0..2 {
                let excluded = b.id();
                let expected_version = a.link_config().unwrap().applied().version.clone();
                call(&a, move |n| n.set_screen_disabled(excluded.into(), round == 0))
                    .await
                    .unwrap();
                wait("current screen owner preference", || {
                    [&a, &b, &c].iter().all(|n| {
                        n.link_config()
                            .unwrap()
                            .applied()
                            .owners
                            .get(&excluded)
                            .is_some_and(|p| p.screen_shared == (round != 0))
                    })
                })
                .await;
                assert_eq!(a.link_config().unwrap().applied().version, expected_version);
                for (index, node) in [&a, &b, &c].into_iter().enumerate() {
                    call(node, move |n| {
                        let mut settings = n.input_settings().unwrap();
                        settings.pointer.drag_across_devices = (index + round) % 2 == 1;
                        settings.gestures_enabled = index != round;
                        settings.scroll.speed = (index + round + 1) as f64;
                        n.set_input_settings(settings.into()).unwrap();
                    })
                    .await;
                }
                let bundle = call(&a, |n| n.export_diagnostics()).await.unwrap();
                assert_eq!(bundle.included_device_count, Some(3));
                assert_eq!(bundle.missing_device_count, Some(0));
                let m = manifest(&bundle.path);
                let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
                for (index, node) in [&a, &b, &c].into_iter().enumerate() {
                    let group = node.link_config().unwrap().applied().group;
                    let device = oink_agent::diagnostics::Anonymizer::new(group).device(node.id());
                    let alias = m["devices"][&device].as_str().unwrap();
                    if index > 0 {
                        let env = wire_environment(&mut zip, alias);
                        assert_eq!(
                            env.source_tree_sha256().unwrap().0,
                            m["source_tree_sha256"].as_str().unwrap()
                        );
                        let layout = env.layout().unwrap().unwrap();
                        assert_eq!(layout.saved().unwrap().0.len(), 3);
                        let excluded_alias = oink_agent::diagnostics::Anonymizer::new(group).device(excluded);
                        assert!(
                            layout
                                .saved()
                                .unwrap()
                                .0
                                .iter()
                                .filter(|s| s.device().unwrap().0 == excluded_alias)
                                .all(|s| s.enabled() == Ok(round != 0))
                        );
                        let own = layout
                            .devices()
                            .unwrap()
                            .0
                            .into_iter()
                            .find(|d| d.device().unwrap().0 == device)
                            .unwrap();
                        let display = own.displays().unwrap().0.remove(0);
                        assert_eq!(display.pixel_width().unwrap(), 800);
                        assert_eq!(display.reference_extent().unwrap().unwrap().w().unwrap(), 300. / 686.);
                        assert_eq!(display.scale().unwrap(), 1.);
                        let permissions = env.permissions().unwrap().unwrap();
                        assert_eq!(
                            permissions.accessibility().unwrap(),
                            w::DiagPermissionState::NotApplicable
                        );
                        assert_eq!(
                            permissions.input_monitoring().unwrap(),
                            w::DiagPermissionState::NotApplicable
                        );
                        assert!(!permissions.secure_input_active().unwrap());
                        assert_eq!(
                            scalar(env.settings().unwrap().0, "forward_gestures"),
                            v1::RawValue::Bool(index != round)
                        );
                        assert_eq!(
                            scalar(env.settings().unwrap().0, "diagnostics_enabled"),
                            v1::RawValue::Bool(true)
                        );
                        assert_eq!(
                            scalar(env.routing_numbers().unwrap().0, "remote_scroll_speed"),
                            v1::RawValue::F64((index + round + 1) as f64)
                        );
                        assert_eq!(
                            env.button_handoff().unwrap().unwrap().mode().unwrap(),
                            if (index + round) % 2 == 1 {
                                w::DiagButtonHandoff::Allow
                            } else {
                                w::DiagButtonHandoff::Block
                            }
                        );
                        continue;
                    }
                    let env: serde_json::Value =
                        serde_json::from_reader(zip.by_name(&format!("devices/{alias}/environment.json")).unwrap())
                            .unwrap();
                    assert_eq!(env["layout"]["saved"].as_array().unwrap().len(), 3, "{alias}: {env}");
                    assert_eq!(env["layout_capture"]["kind"], "current");
                    assert_eq!(env["layout_capture"]["source_tree_sha256"], m["source_tree_sha256"]);
                    let excluded_alias = oink_agent::diagnostics::Anonymizer::new(group).device(excluded);
                    assert!(
                        env["layout"]["saved"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|s| s["device"] == excluded_alias)
                            .all(|s| s["enabled"] == (round != 0))
                    );
                    let own = env["layout"]["devices"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|d| d["device"] == device)
                        .unwrap();
                    assert_eq!(own["displays"][0]["pixel_width"], 800);
                    assert_eq!(own["displays"][0]["reference_extent"]["w"], 300. / 686.);
                    assert_eq!(own["displays"][0]["scale"], 1.);
                    assert!(env["layout"]["compiled"]["screens"].is_array());
                    assert_eq!(env["permissions"]["accessibility"], "NotApplicable");
                    assert_eq!(env["permissions"]["input_monitoring"], "NotApplicable");
                    assert_eq!(env["permissions"]["secure_input_active"], false);
                    assert_eq!(env["settings"]["forward_gestures"], index != round);
                    assert_eq!(env["settings"]["diagnostics_enabled"], true);
                    assert_eq!(
                        env["routing_numbers"]["remote_scroll_speed"],
                        (index + round + 1) as f64
                    );
                    assert_eq!(
                        env["button_handoff"],
                        if (index + round) % 2 == 1 { "Allow" } else { "Block" }
                    );
                    assert_eq!(env["source_tree_sha256"], m["source_tree_sha256"]);
                }
                let all = content(&bundle.path);
                for private in ["secret-computer", "secret-screen", "Private screen", "127.0.0.1"] {
                    assert!(!all.contains(private), "runtime environment leaked {private}");
                }
                for node in [&a, &b, &c] {
                    assert!(!all.contains(&node.id().to_hex()));
                }
                assert!(saved_packages(&b).is_empty() && saved_packages(&c).is_empty());
            }
            for node in [&a, &b, &c] {
                node.shutdown().await;
            }
        },
    );
}
#[test]
fn local_recording_three_member_save_offline_pause_emergency_and_privacy() {
    common::watchdog::run(
        module_path!(),
        "local_recording_three_member_save_offline_pause_emergency_and_privacy",
        Some(4),
        local_recording_three_member_save_offline_pause_emergency_and_privacy_scenario(),
    );
}

async fn local_recording_three_member_save_offline_pause_emergency_and_privacy_scenario() {
    oink_agent::logs::init();
    let temp = tempfile::tempdir().unwrap();
    let (a, ai) = start(&temp.path().join("a"), "secret-computer-alpha", 0).await;
    let (b, bi) = start(&temp.path().join("b"), "secret-computer-beta", 0).await;
    let (c, ci) = start(&temp.path().join("c"), "secret-computer-gamma", 0).await;
    common::pair(&a, &b).await;
    common::pair(&a, &c).await;
    common::active(&[&a, &b, &c]).await;
    // Pairing now starts capability-gated profile catalog transfers as well as
    // the existing Low modules. Establish an idle baseline before attributing
    // any new Low operation to local diagnostic recording.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    let mut previous = [&a, &b, &c].map(low_operations);
    let mut unchanged_since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let current = [&a, &b, &c].map(low_operations);
        if current != previous {
            previous = current;
            unchanged_since = tokio::time::Instant::now();
        }
        if unchanged_since.elapsed() >= Duration::from_secs(3) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "initial Low synchronization did not settle"
        );
    }
    let before = [&a, &b, &c].map(low_operations);
    for (input, code) in [(&ai, 4), (&bi, 5), (&ci, 6)] {
        feed(input, KeyCode::keyboard(code), true);
        feed(input, KeyCode::keyboard(code), false);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        before,
        [&a, &b, &c].map(low_operations),
        "recording sends no Low operations"
    );
    wait("three-screen facts", || {
        [&a, &b, &c].iter().all(|n| n.layout().0.screens.len() == 3)
    })
    .await;
    let placements = [&a, &b, &c]
        .iter()
        .enumerate()
        .map(|(i, n)| Placement {
            device: n.id().into(),
            origin: Point::new(i as f64 * 300.0 / 686., 0.0),
            scale: 300.0 / 686.0 / 800.0,
        })
        .collect();
    call(&a, move |n| n.save_layout(placements)).await.unwrap();
    wait("row compiled everywhere", || {
        let expected = a.layout().0;
        [&a, &b, &c].iter().all(|n| {
            let compiled = n.link_config().unwrap().router_layout().load();
            n.layout().0 == expected
                && expected.screens.iter().all(|screen| {
                    compiled
                        .topology
                        .topology()
                        .screen(&screen.id)
                        .is_some_and(|actual| actual.placement == *screen)
                        && compiled.topology.topology().is_active(&screen.id)
                })
        })
    })
    .await;
    // Focus locking pins the current holder; returning home explicitly selects the source.
    call(&a, |n| n.return_pointer_home()).await.unwrap();
    wait("source holds focus", || {
        a.input_status().unwrap().focus_info.as_ref().map(|f| f.holder) == Some(a.id())
    })
    .await;
    ai.set_cursor(Point::new(799.0, 300.0));
    let until = Instant::now() + Duration::from_secs(10);
    while a.input_status().unwrap().focus_info.as_ref().map(|f| f.holder) != Some(b.id()) {
        assert!(Instant::now() < until, "fake pointer did not cross the edge");
        ai.feed(&CapturedEvent {
            physical: true,
            event: InputEvent::PointerDelta { dx: 10.0, dy: 0.0 },
            source: SourceKind::Mouse,
            time_us: 43,
            cursor: Some(Point::new(799.0, 300.0)),
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait("receiver restored", || {
        b.input_status().unwrap().focus_info.as_ref().map(|f| f.holder) == Some(b.id())
    })
    .await;
    for (phase, progress) in [
        (oink_foundation::input::GesturePhase::Began, 0.),
        (oink_foundation::input::GesturePhase::Changed, -0.8),
        (oink_foundation::input::GesturePhase::Ended, -0.4),
    ] {
        ci.feed(&CapturedEvent {
            physical: true,
            source: SourceKind::Touchpad,
            time_us: 44,
            cursor: None,
            event: InputEvent::Gesture(oink_foundation::input::Gesture::DockSwipe {
                axis: oink_foundation::input::SwipeAxis::Vertical,
                phase,
                progress,
                velocity: 1.25,
            }),
        });
    }
    // Export samples and drains each recording owner before taking its snapshot.
    let bundle = call(&a, |n| n.export_diagnostics()).await.unwrap();
    assert_eq!(
        (
            bundle.included_device_count,
            bundle.missing_device_count,
            bundle.cutoff_complete
        ),
        (Some(3), Some(0), Some(true))
    );
    assert_eq!(device_count(&bundle.path), 3);
    assert!(
        saved_packages(&b).is_empty() && saved_packages(&c).is_empty(),
        "only initiator saves"
    );
    let all = content(&bundle.path);
    for required in [
        "FocusEnterSent",
        "Topology",
        "Crossing",
        "RouterState",
        "clock_uncertainty_us",
        "message_identity",
    ] {
        assert!(all.contains(required), "missing local evidence: {required}");
    }
    let mut archive = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    let mut received = false;
    let mut gesture = false;
    let mut peer_keys = 0;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        if !file.name().unwrap().contains("/segments/") {
            continue;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        let segment = w::DiagnosticSegment::decode(&bytes, oink_agent::diagnostics::MAX_SEGMENT_RAW).unwrap();
        for row in segment.rows().unwrap().0 {
            let event = row.event().unwrap();
            let fields = event.fields().unwrap();
            match event.kind().unwrap().0.as_str() {
                "handoff/1" => {
                    let h = w::DiagHandoff::from_raw(fields).unwrap();
                    received |= h.action() == Ok(w::DiagHandoffAction::FocusEnterReceived);
                }
                "gesture/1" => {
                    let g = w::DiagGestureEvent::from_raw(fields).unwrap().summary().unwrap();
                    gesture |= g.peak_abs_progress().unwrap() >= 0.8 && g.changed_events().unwrap() > 0;
                }
                "input/1" => {
                    let input = w::DiagInput::from_raw(fields).unwrap().event().unwrap();
                    if input.variant() == Ok(Open::Known(w::DiagSafeInputKind::Key)) {
                        let key = w::DiagKey::from_raw(input.body().unwrap()).unwrap();
                        if key.class() == Ok(w::DiagKeyClass::Ordinary) {
                            peer_keys += 1;
                            assert_eq!(key.raw().len(), 4);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    assert!(received, "peer handoff receipt missing");
    assert!(gesture, "peer gesture summary missing");
    assert!(peer_keys > 0, "peer classified key records missing");

    for private in ["secret-computer", "secret-screen", "Private screen", "127.0.0.1"] {
        assert!(!all.contains(private), "leak: {private}");
    }
    // Bare numeric substring searches collide with clock samples (e.g. a
    // legitimate mono_us value of 458757). Inspect the captured key payload,
    // where a raw HID code would be a privacy leak, separately from timing.
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    let mut key_rows = 0;
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).unwrap();
        if !file.name().unwrap().ends_with("timeline.ndjson") {
            continue;
        }
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        for line in text.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(key) = row.pointer("/event/Input/event/Key") {
                key_rows += 1;
                assert_eq!(key["class"], "Ordinary");
                for field in ["code", "key_code", "hid_code"] {
                    assert!(key.get(field).is_none(), "raw key field leaked: {key}");
                }
                // Diagnostic hardware flags may be present, raw key identities may not.
                assert!(!key.to_string().contains("458756"));
                assert!(!key.to_string().contains("458757"));
            }
        }
    }
    assert!(key_rows > 0, "gathered captured key payloads must be checked");
    for node in [&a, &b, &c] {
        assert!(!all.contains(&node.id().to_hex()));
    }
    // Every retained segment belongs to its own recorder, even after gathering.
    for node in [&a, &b, &c] {
        let devices = std::fs::read_dir(node.data_dir().join("Logs/Diagnostics/buffer"))
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|n| n[..24].to_owned()))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(devices.len(), 1);
    }
    c.shutdown().await;
    wait("offline member", || !a.links().unwrap().links().ready(c.id())).await;
    let partial = call(&a, |n| n.export_diagnostics()).await.unwrap();
    assert_eq!(
        (
            partial.included_device_count,
            partial.missing_device_count,
            partial.cutoff_complete
        ),
        (Some(2), Some(1), Some(false))
    );
    assert!(
        manifest(&partial.path)["missing_members"]
            .as_object()
            .unwrap()
            .values()
            .any(|v| v == "offline")
    );
    // Recording switch remains local; disabling a member is reported on the next save.
    call(&b, |n| n.set_diagnostics_enabled(false)).await.unwrap();
    assert!(a.diagnostics_status().unwrap().configured_enabled);
    let disabled = call(&a, |n| n.export_diagnostics()).await.unwrap();
    assert_eq!(disabled.included_device_count, Some(1));
    assert!(
        manifest(&disabled.path)["missing_members"]
            .as_object()
            .unwrap()
            .values()
            .any(|v| v == "recording_disabled")
    );
    call(&b, |n| n.set_diagnostics_enabled(true)).await.unwrap();
    feed(&bi, KeyCode::keyboard(7), true);
    feed(&bi, KeyCode::keyboard(7), false);
    feed(&ai, KeyCode::ESCAPE, true);
    wait("full pause settled", || {
        [&a, &b]
            .iter()
            .all(|n| n.pause_status().paused && !n.pause_status().settling)
    })
    .await;
    wait("emergency group bundles", || {
        [&a, &b]
            .iter()
            .all(|n| saved_packages(n).iter().any(|v| v.contains("-emergency")))
    })
    .await;
    let paused = call(&a, |n| n.export_diagnostics()).await.unwrap();
    assert_eq!(paused.included_device_count, Some(2), "paused members still answer");
    assert_eq!(paused.missing_device_count, Some(1));
    let m = manifest(&paused.path);
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&paused.path).unwrap()).unwrap();
    for node in [&a, &b] {
        let device =
            oink_agent::diagnostics::Anonymizer::new(node.link_config().unwrap().applied().group).device(node.id());
        let alias = m["devices"][&device].as_str().unwrap();
        if node.id() != a.id() {
            let env = wire_environment(&mut zip, alias);
            assert_eq!(
                scalar(env.settings().unwrap().0, "sharing_enabled"),
                v1::RawValue::Bool(false)
            );
            assert!(env.permissions().unwrap().is_some());
            continue;
        }
        let env: serde_json::Value =
            serde_json::from_reader(zip.by_name(&format!("devices/{alias}/environment.json")).unwrap()).unwrap();
        assert_eq!(env["settings"]["sharing_enabled"], false);
        assert!(env["permissions"].is_object());
    }
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn recovered_bundle_survives_without_saved_package_list() {
    common::watchdog::run(
        module_path!(),
        "recovered_bundle_survives_without_saved_package_list",
        Some(4),
        recovered_bundle_survives_without_saved_package_list_scenario(),
    );
}

async fn recovered_bundle_survives_without_saved_package_list_scenario() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("recovery");
    // Recovery depends on the data directory, not the network address. Let the OS
    // allocate each port: shutdown/rebind can leave Quinn's old socket alive until
    // its driver and aborted tasks are dropped, especially under CPU load.
    let (node, _) = start(&dir, "temporary-recovery-device", 0).await;
    call(&node, |n| n.export_diagnostics()).await.unwrap();
    let marker = dir.join("Logs/Diagnostics/running");
    let previous = std::fs::read(&marker).unwrap();
    node.shutdown().await;
    drop(node);
    // Only a temporary fake-backend node: no installed service or real desktop is touched.
    std::fs::write(marker, previous).unwrap();
    let (node, _) = start(&dir, "temporary-recovery-device", 0).await;
    wait("background crash recovery", || {
        !node.diagnostics_status().unwrap().recovering
    })
    .await;
    assert!(saved_packages(&node).iter().any(|v| v.contains("-recovered_crash")));
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    node.subscribe(Arc::new(move |event| {
        if matches!(event, ApiEvent::DiagnosticsStatus(_)) {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }));
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    node.shutdown().await;
}
