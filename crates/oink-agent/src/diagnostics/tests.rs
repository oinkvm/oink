use super::privacy::KeyClass;
use super::*;
use oink_foundation::input::{InputEvent, KeyCode};
use std::io::Read;
// Historical throughput/clock fixture span, never a recorder retention bound.
const TEST_SPAN_US: i64 = 30 * 60 * 1_000_000;

#[test]
fn heartbeat_samples_export_measurement_age_and_share_the_link_capture_stamp() {
    use oink_foundation::session::{Boot, BootPair, PathId};
    use oink_network::link::{LinkEntry, LinkId, LinkPath, LinkState, LinkTable};
    let temp = tempfile::tempdir().unwrap();
    let me = DeviceId([1; 32]);
    let peer = DeviceId([2; 32]);
    let boot = |device_id| Boot {
        device_id,
        nonce: [1; 16],
    };
    let recorder = isolated(temp.path().into(), me, None, true).unwrap();
    recorder.wait_recovery();
    let measured_at = Instant::now() - Duration::from_millis(400);
    let mut table = LinkTable {
        links: vec![LinkEntry {
            link: LinkId(1),
            peer,
            peer_boot: boot(peer),
            path_id: PathId {
                boot_pair: BootPair::new(boot(me), boot(peer)).unwrap(),
                coordinator_counter: 1,
            },
            route_revision: 0,
            agreement: Default::default(),
            peer_formats: None,
            peer_audio: None,
            state: LinkState::Ready,
            path: LinkPath::Direct,
            remote_address: Some("127.0.0.1:47470".parse().unwrap()),
            rtt: Some(Duration::from_millis(14)),
            rtt_measured_at: Some(measured_at),
            lost_packets: 0,
            sent_packets: 10,
            udp_bytes_sent: 100,
            udp_datagrams_sent: 10,
            udp_bytes_received: 100,
            low_available: true,
            low_failure: None,
        }],
    };
    recorder.sample_links(&table);
    table.links[0].rtt = None;
    table.links[0].rtt_measured_at = None;
    recorder.sample_links(&table);
    let bundle = recorder.save_range(None).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    let samples: Vec<_> = rows
        .iter()
        .filter(|row| row["event"]["Log"]["module"] == "oink_heartbeat")
        .collect();
    assert_eq!(samples.len(), 1, "no measurement is invented before a matched Pong");
    let sample = samples[0];
    let fields = sample["event"]["Log"]["fields"].as_array().unwrap();
    let value = |name| fields.iter().find(|f| f[0] == name).unwrap()[1].as_f64().unwrap();
    assert_eq!(value("rtt_us"), 14_000.);
    assert!(
        value("sample_age_us") >= 400_000.,
        "stale snapshots retain measurement age"
    );
    assert!(value("capture_span_us") >= 0.);
    let link = rows
        .iter()
        .find(|row| row["event"]["Link"]["rtt_us"] == 14_000)
        .unwrap();
    for key in ["mono_us", "original_utc_us", "clock_id"] {
        assert_eq!(sample[key], link[key]);
    }
    assert_eq!(
        sample["event"]["Log"]["labels"][0],
        serde_json::json!(["peer", recorder.anonymizer().device(peer)])
    );
    assert!(!serde_json::to_string(sample).unwrap().contains(&peer.to_hex()));
    recorder.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_environment_is_in_startup_and_rekey_segments_before_any_save() {
    let temp = tempfile::tempdir().unwrap();
    let node = crate::node::Node::start(crate::node::NodeOptions {
        net: oink_test_support::network_config(temp.path().into(), "PRIVATE-NAME", 0),
        input: Arc::new(oink_test_support::input::FakeInput::new()),
        displays: vec![oink_foundation::display::DisplayInfo {
            key: "PRIVATE-SCREEN-KEY".into(),
            name: "PRIVATE-SCREEN-NAME".into(),
            bounds: oink_foundation::Rect::new(0., 0., 800., 600.),
            pixel_width: 800,
            pixel_height: 600,
            reference_extent: Some(oink_foundation::Size::new(300. / 686., 200. / 686.)),
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
    let r = &node.inner.diagnostics.get().unwrap().recorder;
    r.wait_recovery();
    for group in [None, Some(GroupId([103; 16]))] {
        if let Some(group) = group {
            r.rekey(node.id(), Some(group)).unwrap();
        }
        let mut store = lock(&r.inner.store);
        store.append(vec![motion(1)], 0).unwrap();
        // Decode what actually leaves this recorder, before ZIP/export logic.
        let segment = store::decode(&store.read(store.segments.back().unwrap()).unwrap()).unwrap();
        let snapshot = segment.snapshot;
        if let Some(provider) = store.snapshot.file_provider {
            assert_eq!(
                snapshot.routing_numbers.get("file_provider_reason"),
                Some(&provider.code())
            );
        }
        assert_eq!(snapshot.source_tree_sha256, env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256"));
        assert_eq!(snapshot.layout.as_ref().unwrap().saved.len(), 1);
        assert_eq!(
            snapshot.layout.as_ref().unwrap().saved[0].screen,
            r.anonymizer().screen(node.id(), "PRIVATE-SCREEN-KEY")
        );
        assert!(snapshot.permissions.is_some());
        assert!(snapshot.button_handoff.is_some());
        assert!(snapshot.settings.contains_key("forward_gestures"));
        assert!(snapshot.routing_numbers.contains_key("remote_scroll_speed"));
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("PRIVATE"));
        assert!(!encoded.contains(&node.id().to_hex()));
        assert!(!encoded.contains("127.0.0.1"));
    }
    node.shutdown().await;
}

#[test]
fn layout_changes_and_export_are_complete_without_input_trace() {
    crate::test_watchdog::run(
        module_path!(),
        "layout_changes_and_export_are_complete_without_input_trace",
        Some(2),
        layout_changes_and_export_scenario(),
    );
}
async fn layout_changes_and_export_scenario() {
    let temp = tempfile::tempdir().unwrap();
    let node = crate::node::Node::start(crate::node::NodeOptions {
        net: oink_test_support::network_config(temp.path().into(), "PRIVATE-NAME", 0),
        input: Arc::new(oink_test_support::input::FakeInput::new()),
        displays: vec![oink_foundation::display::DisplayInfo {
            key: "PRIVATE-SCREEN-KEY".into(),
            name: "PRIVATE-SCREEN-NAME".into(),
            bounds: oink_foundation::Rect::new(17., 9., 800., 600.),
            pixel_width: 1600,
            pixel_height: 1200,
            reference_extent: Some(oink_foundation::Size::new(300. / 686., 200. / 686.)),
            rotation_deg: 0,
            is_primary: true,
            is_builtin: true,
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
    let recorder = &node.inner.diagnostics.get().unwrap().recorder;
    recorder.wait_recovery();
    node.inner.input_trace.get().unwrap().disable_for_test();
    let config = node.link_config().unwrap();
    config
        .edit_layout(
            Box::new(|layout| layout.screens[0].translate(23., 7.)),
            oink_protocol::v1::dto::LayoutSource::Manual,
        )
        .unwrap();
    let saving = recorder.clone();
    let bundle = tokio::task::spawn_blocking(move || saving.save(Reason::Manual))
        .await
        .unwrap()
        .unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    let changes: Vec<_> = rows.iter().filter_map(|row| row["event"].get("Topology")).collect();
    assert!(
        changes.iter().any(|change| change["kinds"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "Arrangement"))),
        "layout edit must be recorded with tracing off: {changes:?}"
    );
    let change = changes
        .iter()
        .find(|c| {
            c["kinds"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| v == "Arrangement"))
        })
        .unwrap();
    assert_eq!(
        change["after"]["saved"][0]["plane"]["x"].as_f64().unwrap()
            - change["before"]["saved"][0]["plane"]["x"].as_f64().unwrap(),
        23.
    );
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    let environment: serde_json::Value =
        serde_json::from_reader(zip.by_name("devices/device-A/environment.json").unwrap()).unwrap();
    assert_eq!(environment["layout"]["devices"][0]["displays"][0]["pixel_width"], 1600);
    assert_eq!(environment["layout"]["devices"][0]["displays"][0]["scale"], 2.);
    assert_eq!(environment["layout"]["saved"][0]["enabled"], true);
    assert!(environment["layout"]["compiled"].is_object());
    assert!(!serde_json::to_string(&environment).unwrap().contains("PRIVATE"));
    node.shutdown().await;
}

#[test]
fn build_identity_is_in_the_first_segment_and_survives_rekey() {
    let temp = tempfile::tempdir().unwrap();
    let me = DeviceId([101; 32]);
    let r = isolated(temp.path().into(), me, Some(GroupId([102; 16])), true).unwrap();
    r.wait_recovery();
    for group in [None, Some(GroupId([103; 16]))] {
        if let Some(group) = group {
            r.rekey(me, Some(group)).unwrap();
        }
        let mut store = lock(&r.inner.store);
        store.append(vec![motion(1)], 0).unwrap();
        let bytes = store.read(store.segments.back().unwrap()).unwrap();
        let snapshot = store::decode(&bytes).unwrap().snapshot;
        assert_eq!(snapshot.source_tree_sha256, env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256"));
        assert!(!snapshot.source_tree_sha256.is_empty());
        assert_eq!(snapshot.version, env!("OINK_RELEASE_VERSION"));
        assert_eq!(snapshot.os, std::env::consts::OS);
    }
    r.shutdown();
}

#[test]
fn build_identity_does_not_relabel_recovered_segments_or_missing_members() {
    let temp = tempfile::tempdir().unwrap();
    let me = DeviceId([104; 32]);
    let group = GroupId([105; 16]);
    let device = Anonymizer::new(group).device(me);
    let old_sha = "1".repeat(40);
    let mut old = store::Store::open(temp.path().into(), device.clone(), true).unwrap();
    old.recover_for_test().unwrap();
    old.snapshot.version = "old-version".into();
    old.snapshot.source_tree_sha256 = old_sha.clone();
    old.append(vec![motion(1)], 0).unwrap();
    drop(old); // Previous build crashed; preserve its identity in the crash window.
    let r = isolated(temp.path().into(), me, Some(group), true).unwrap();
    r.wait_recovery();
    let bundle = r
        .list()
        .unwrap()
        .into_iter()
        .find(|b| b.reason == Reason::RecoveredCrash)
        .unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
    assert_eq!(manifest["versions"][&device]["source_tree_sha256"], old_sha);
    assert_eq!(manifest["versions"][&device]["version"], "old-version");
    assert_eq!(
        lock(&r.inner.store).snapshot.source_tree_sha256,
        env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256")
    );
    let mut snapshot = lock(&r.inner.store).export_snapshot().unwrap();
    let missing = "f".repeat(24);
    snapshot.missing_members.insert(missing.clone(), "offline".into());
    let bundle = snapshot.export(Reason::Manual, 0, 0).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
    assert_eq!(manifest["versions"][&missing]["source_tree_sha256"], "");
    r.shutdown();
}

pub(super) fn wire_version(counter: u64) -> oink_protocol::v1::dto::ConfigVersion {
    use oink_protocol::v1::{
        RawValue as V, dto,
        schema::{WireDto, WireValue},
    };
    dto::ConfigVersion::from_raw(
        [
            (1, counter.write()),
            (
                3,
                dto::DeviceId::from_raw([(1, V::from(dto::ED25519)), (2, V::Bytes(vec![1; 32]))].into())
                    .unwrap()
                    .write(),
            ),
        ]
        .into(),
    )
    .unwrap()
}
pub(super) fn boot(device: DeviceId) -> oink_foundation::session::Boot {
    oink_foundation::session::Boot {
        device_id: device,
        nonce: [1; 16],
    }
}
fn empty_layout_for_export(count: u64) -> layout::CompleteLayout {
    let version = layout::Version {
        counter: count,
        writer_alias: "a".repeat(24),
    };
    layout::CompleteLayout {
        sequence: count,
        version: version.clone(),
        roster_digest: [0; 32],
        saved_members: vec![],
        applied_roster_digest: [0; 32],
        applied_version: version.clone(),
        owner_preferences: vec![],
        local_pause: layout::LocalPause {
            paused: false,
            reason: 0,
            revision: 0,
        },
        source: Default::default(),
        paused: false,
        saved: vec![],
        settings: layout::Settings {
            effective: Default::default(),
        },
        devices: vec![],
        compiled: layout::Compiled {
            current_group: true,
            stage: layout::CompilationStage::Router,
            sequence: count,
            version: version.clone(),
            settings: Default::default(),
            screens: vec![],
            sources: vec![],
            source_devices: vec![],
            input_online: None,
            edges: vec![],
        },
    }
}

#[test]
fn gathered_environment_updates_live_instance_without_relabelling_crash_history() {
    let temp = tempfile::tempdir().unwrap();
    let local = store(temp.path());
    let peer = "b".repeat(24);
    let old_instance = "c".repeat(24);
    let new_instance = "d".repeat(24);
    let old = Snapshot {
        source_tree_sha256: "old-build".into(),
        layout: Some(empty_layout_for_export(1)),
        ..Snapshot::default()
    };
    let new = Snapshot {
        source_tree_sha256: "new-build".into(),
        layout: Some(empty_layout_for_export(2)),
        ..Snapshot::default()
    };
    for (instance, historical, expected) in [
        (old_instance, old, "old-build"),
        (new_instance.clone(), new.clone(), "new-build"),
    ] {
        let mut staged = local.export_snapshot().unwrap();
        let segment = store::Segment {
            schema: SCHEMA,
            device: peer.clone(),
            instance,
            seq: 1,
            dropped: 0,
            snapshot: historical,
            clock_samples: vec![],
            rows: vec![motion(1)],
        };
        staged
            .add_gathered(&segment, &store::encode(&segment).unwrap())
            .unwrap();
        let mut fresh = new.clone();
        fresh.settings.insert("forward_gestures".into(), false);
        staged
            .gathered_environments
            .insert(peer.clone(), (new_instance.clone(), fresh));
        let bundle = staged.export(Reason::RecoveredCrash, 0, 0).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let m: serde_json::Value = serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
        let alias = m["devices"][&peer].as_str().unwrap();
        let env: serde_json::Value =
            serde_json::from_reader(zip.by_name(&format!("devices/{alias}/environment.json")).unwrap()).unwrap();
        assert_eq!(env["source_tree_sha256"], expected);
        assert_eq!(
            env["layout"]["version"]["counter"], 2,
            "export needs the peer's current layout even when the selected timeline is from its previous instance"
        );
        assert_eq!(
            env["settings"].get("forward_gestures").is_some(),
            expected == "new-build"
        );
        assert_eq!(env["layout_capture"]["kind"], "current");
        assert_eq!(env["layout_capture"]["source_tree_sha256"], "new-build");
        assert_eq!(env["layout_capture"]["instance"], new_instance);
        // Offline peers keep their last retained layout and explicitly say so.
        staged.gathered_environments.clear();
        let bundle = staged.export(Reason::RecoveredCrash, 0, 0).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let env: serde_json::Value =
            serde_json::from_reader(zip.by_name(&format!("devices/{alias}/environment.json")).unwrap()).unwrap();
        assert_eq!(
            env["layout"]["version"]["counter"],
            segment.snapshot.layout.as_ref().unwrap().version.counter
        );
        assert_eq!(env["layout_capture"]["kind"], "retained");
    }
}

#[test]
fn configuration_snapshot_explicitly_reports_unavailable_layout() {
    let mut segment = store::Segment {
        schema: SCHEMA,
        device: "a".repeat(24),
        instance: "b".repeat(24),
        seq: 1,
        dropped: 0,
        snapshot: Snapshot {
            group_state: Some("{}".into()),
            ..Snapshot::default()
        },
        clock_samples: vec![],
        rows: vec![motion(1)],
    };
    assert!(store::decode(&store::encode(&segment).unwrap()).is_ok());
    assert!(segment.snapshot.validate().is_ok());
    let baseline = super::wire_values::snapshot(&segment.snapshot, false).unwrap();
    assert!(baseline.group_state().unwrap().configuration_available().unwrap());
    assert!(!baseline.group_state().unwrap().layout_available().unwrap());
    assert!(baseline.screens().unwrap().0.is_empty());
    assert!(baseline.layout().unwrap().is_none());
    segment.snapshot.layout = Some(empty_layout_for_export(1));
    assert!(segment.snapshot.validate().is_ok());
    assert!(store::decode(&store::encode(&segment).unwrap()).is_ok());
    segment.snapshot = Snapshot::default();
    assert!(segment.snapshot.validate().is_ok());
    assert!(store::decode(&store::encode(&segment).unwrap()).is_ok());
}

#[test]
fn snapshot_without_injection_failure_counter_defaults_to_zero() {
    let mut json = serde_json::to_value(store::Snapshot::default()).unwrap();
    json.as_object_mut().unwrap().remove("injection_failures");
    let snapshot: store::Snapshot = serde_json::from_value(json).unwrap();
    assert_eq!(snapshot.injection_failures, 0);
}

// Storage, clock and lifecycle fixtures own their rows. Production Node tests
// and the dedicated DiagnosticsLayer cases keep the real global registration.
pub(super) fn isolated(dir: PathBuf, me: DeviceId, group: Option<GroupId>, enabled: bool) -> anyhow::Result<Recorder> {
    isolated_suspended(dir, me, group, enabled, false)
}
pub(super) fn isolated_suspended(
    dir: PathBuf,
    me: DeviceId,
    group: Option<GroupId>,
    enabled: bool,
    suspended: bool,
) -> anyhow::Result<Recorder> {
    Recorder::start_inner(
        dir,
        me,
        group,
        enabled,
        suspended,
        clock::Clock::system(),
        None,
        None,
        false,
    )
}
fn isolated_with_clock(
    dir: PathBuf,
    me: DeviceId,
    group: Option<GroupId>,
    enabled: bool,
    clock: Arc<clock::Clock>,
) -> anyhow::Result<Recorder> {
    Recorder::start_inner(dir, me, group, enabled, false, clock, None, None, false)
}
fn event(code: KeyCode) -> Event {
    Event::Input {
        origin: Origin::Physical,
        peer: None,
        original_us: Some(123),
        cursor: None,
        event: SafeInput::capture(&InputEvent::Key { code, pressed: true }),
        success: None,
        modifiers: None,
        disposition: None,
    }
}
fn motion(i: u64) -> Row {
    Row {
        mono_us: i,
        clock_id: 0,
        utc_us: utc_us() + i as i64,
        event: Event::Input {
            origin: Origin::Physical,
            peer: None,
            original_us: Some(i),
            cursor: Some(oink_foundation::Point { x: i as f64, y: 10.0 }),
            event: SafeInput::Motion {
                dx: 1.0,
                dy: -2.0,
                raw: false,
            },
            success: None,
            modifiers: None,
            disposition: None,
        },
    }
}
fn store(dir: &std::path::Path) -> store::Store {
    let mut s = store::Store::open(dir.to_path_buf(), "a".repeat(24), true).unwrap();
    s.recover_for_test().unwrap();
    s
}
#[test]
fn every_key_enters_only_the_explicit_lossy_category() {
    // Exhaust every HID usage in the keyboard and consumer pages, plus unknown/vendor pages.
    // Keys in an allowed category produce byte-identical recorder payloads. There is no code/hash slot.
    let mut groups = std::collections::HashMap::new();
    for page in [KeyCode::PAGE_KEYBOARD, KeyCode::PAGE_CONSUMER, 0xff, 0xffff] {
        for usage in 0..=u16::MAX {
            let code = KeyCode::new(page, usage);
            let ev = event(code);
            let bytes = serde_json::to_vec(&ev).unwrap();
            let class = format!("{:?}", KeyClass::from_code(code));
            if let Some(previous) = groups.insert(class, bytes.clone()) {
                assert_eq!(previous, bytes);
            }
        }
    }
    let json = serde_json::to_string(&event(KeyCode::keyboard(0x1e))).unwrap();
    for forbidden in ["code", "usage", "scan", "458782", "0x1e"] {
        assert!(!json.contains(forbidden), "{json}");
    }
    assert_eq!(
        serde_json::to_vec(&event(KeyCode::keyboard(4))).unwrap(),
        serde_json::to_vec(&event(KeyCode::keyboard(5))).unwrap()
    );
}
#[test]
fn anonymity_is_consistent_across_devices_and_domain_separated() {
    let group = GroupId([9; 16]);
    let a = Anonymizer::new(group);
    let b = Anonymizer::new(group);
    let id = DeviceId([1; 32]);
    assert_eq!(a.device(id), b.device(id));
    assert_eq!(a.screen(id, "private-screen"), b.screen(id, "private-screen"));
    assert_ne!(a.device(id), a.screen(id, "private-screen"));
    assert_ne!(a.device(id), Anonymizer::new(GroupId([8; 16])).device(id));
    assert!(!a.screen(id, "private-screen").contains("private-screen"));
}
#[test]
fn rolling_size_and_duplicate_replay_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    s.append(vec![motion(0)], 0).unwrap();
    let meta = s.segments[0].clone();
    let bytes = s.read(&meta).unwrap();
    let segment = store::decode(&bytes).unwrap();
    s.insert(&segment, &bytes).unwrap();
    assert_eq!(s.segments.len(), 1);
    s.limit = 1;
    s.prune().unwrap();
    assert_eq!(s.bytes, 0);
    assert!(s.segments.is_empty());
    s.limit = MAX_BYTES;
    let mut old = motion(0);
    old.utc_us = utc_us() - TEST_SPAN_US - 1_000_000;
    s.append(vec![old], 0).unwrap();
    assert_eq!(s.segments.len(), 1);
}
#[test]
fn expansion_limit_rejects_compression_bombs() {
    use std::io::Write;
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&vec![0; MAX_SEGMENT_RAW + 1]).unwrap();
    let mut bytes = store::HEADER.to_vec();
    bytes.extend(encoder.finish().unwrap());
    assert!(matches!(
        store::decode_classified(&bytes),
        Err((store::DiscardClass::SizeLimit, _))
    ));
}
#[test]
fn abnormal_restart_exports_persisted_rows_but_clean_shutdown_does_not() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    s.append(vec![motion(0)], 0).unwrap();
    drop(s);
    let s = store(temp.path());
    let list = s.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason, Reason::RecoveredCrash);
    s.clean_shutdown().unwrap();
    drop(s);
    let s = store(temp.path());
    assert_eq!(s.list().unwrap().len(), 1);
    s.clean_shutdown().unwrap();
}
#[test]
fn disabled_recorder_clears_buffers_and_does_not_resume_old_queue() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([3; 32]), Some(GroupId([4; 16])), true).unwrap();
    r.record(event(KeyCode::keyboard(4)));
    r.wait_recovery();
    r.save(Reason::Manual).unwrap();
    assert!(r.status().bytes > 0);
    r.set_enabled(false).unwrap();
    r.record(event(KeyCode::keyboard(5)));
    assert_eq!(r.status().bytes, 0);
    assert_eq!(r.status().queued, 0);
    assert!(r.save(Reason::Manual).is_err());
    assert_eq!(r.list().unwrap().len(), 1);
    r.set_enabled(true).unwrap();
    assert_eq!(r.status().queued, 0);
    r.shutdown();
}
#[test]
fn clock_offset_and_merged_timeline_retain_original_time() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    let now = utc_us();
    let mut sample =
        ClockSample::estimate("b".repeat(24), "c".repeat(24), now, now + 1010, now + 1020, now + 30).unwrap();
    assert_eq!(sample.offset_us, 1000);
    assert_eq!(sample.uncertainty_us, 10);
    assert!(ClockSample::estimate("b".repeat(24), "c".repeat(24), 100, 101, 102, 99).is_none());
    sample.local_instance = s.instance.clone();
    s.clocks.push(sample);
    let mut a = motion(0);
    a.utc_us = now + 100;
    s.append(vec![a], 0).unwrap();
    let mut b = motion(0);
    b.utc_us = now + 1050;
    let remote = store::Segment {
        schema: SCHEMA,
        device: "b".repeat(24),
        instance: "c".repeat(24),
        seq: 1,
        dropped: 0,
        snapshot: Snapshot::default(),
        clock_samples: Vec::new(),
        rows: vec![b],
    };
    let bytes = store::encode(&remote).unwrap();
    s.insert(&remote, &bytes).unwrap();
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let mut merged = String::new();
    zip.by_name("merged.ndjson")
        .unwrap()
        .read_to_string(&mut merged)
        .unwrap();
    let rows = merged
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["device"], "b".repeat(24));
    assert_eq!(rows[0]["original_utc_us"], now + 1050);
    assert_eq!(rows[0]["unified_utc_us"], now + 50);
    assert_eq!(rows[1]["unified_utc_us"], now + 100);
    assert!(rows.iter().all(|row| row["clock_calibrated"] == true));
    for alias in ["device-A", "device-B"] {
        let mut text = String::new();
        zip.by_name(&format!("devices/{alias}/timeline.ndjson"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        for line in text.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(rows.contains(&row), "per-device time must agree with merged time");
        }
    }
}
#[test]
fn diagnostic_trace_ignores_file_filter_and_sensitive_dynamic_fields() {
    use tracing_subscriber::layer::SubscriberExt;
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([5; 32]), Some(GroupId([6; 16])), true).unwrap();
    let subscriber = tracing_subscriber::registry().with(trace::scoped(&r));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target:"oink_test",code=458756,username="secret-user",screen="secret-screen",ip="192.168.33.44",path="/Users/secret-user/password",epoch=73, "secret input {}", "password123");
    });
    r.wait_recovery();
    let b = r.save(Reason::Manual).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(b.path).unwrap()).unwrap();
    for i in 0..zip.len() {
        let mut text = String::new();
        zip.by_index(i).unwrap().read_to_string(&mut text).unwrap();
        for secret in [
            "secret-user",
            "secret-screen",
            "192.168.33.44",
            "password123",
            "458756",
            "/Users/",
        ] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }
    r.shutdown();
}

#[test]
fn killed_process_child() {
    let Ok(dir) = std::env::var("OINK_DIAGNOSTIC_CRASH_TEST") else {
        return;
    };
    let r = isolated(dir.into(), DeviceId([21; 32]), Some(GroupId([22; 16])), true).unwrap();
    r.record(motion(1).event);
    {
        let mut s = lock(&r.inner.store);
        r.drain(&mut s).unwrap();
    }
    println!("diagnostic-child-ready");
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}
#[test]
fn killed_process_recovers_durable_rows() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["diagnostics::tests::killed_process_child", "--exact", "--nocapture"])
        .env("OINK_DIAGNOSTIC_CRASH_TEST", dir.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        assert!(out.read_line(&mut line).unwrap() > 0);
        if line.contains("diagnostic-child-ready") {
            break;
        }
        line.clear();
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let r = isolated(dir.path().into(), DeviceId([21; 32]), Some(GroupId([22; 16])), true).unwrap();
    r.wait_recovery();
    assert_eq!(r.list().unwrap()[0].reason, Reason::RecoveredCrash);
    assert!(r.status().segments > 0);
    r.shutdown();
}

#[test]
fn gathered_sources_are_not_limited_by_old_package_cap() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    s.append(vec![motion(0)], 0).unwrap();
    for i in 0..34 {
        let segment = store::Segment {
            schema: SCHEMA,
            device: format!("{i:024x}"),
            instance: "c".repeat(24),
            seq: 1,
            dropped: 0,
            snapshot: Snapshot::default(),
            clock_samples: Vec::new(),
            rows: vec![motion(0)],
        };
        let encoded = store::encode(&segment).unwrap();
        s.insert(&segment, &encoded).unwrap();
    }
    assert_eq!(s.segments.len(), 35);
}

#[test]
fn peer_lifecycle_only_stores_anonymous_identity_and_instance() {
    let temp = tempfile::tempdir().unwrap();
    let recorder = isolated(temp.path().into(), DeviceId([71; 32]), Some(GroupId([72; 16])), true).unwrap();
    let peer = DeviceId([73; 32]);
    for phase in ["connected", "disconnected", "path_changed", "session_changed"] {
        recorder.peer_lifecycle(peer, phase, Some(&"a".repeat(24)));
        let row = recorder.inner.queue.load().pop().unwrap();
        let Event::Log { labels, .. } = row.event else {
            panic!("expected log")
        };
        assert!(labels.contains(&("phase".into(), phase.into())));
        assert!(labels.contains(&("peer".into(), recorder.anonymizer().device(peer))));
        assert!(!serde_json::to_string(&labels).unwrap().contains(&peer.to_hex()));
    }
    recorder.shutdown();
}
#[test]
fn recovery_rejects_malformed_payloads_without_consulting_the_build() {
    for producer in ["unrelated-build", env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256"), "unknown"] {
        let dir = tempfile::tempdir().unwrap();
        let mut old = store(dir.path());
        old.snapshot.source_tree_sha256 = producer.into();
        old.append(vec![motion(1)], 0).unwrap();
        let meta = old.segments.back().unwrap();
        let segment = store::decode(&old.read(meta).unwrap()).unwrap();
        let mut raw = serde_json::to_vec(&segment).unwrap();
        raw.pop();
        let bytes = store::encode_raw(&raw).unwrap();
        assert!(store::decode(&bytes).is_err());
        let name = format!("{}-{}-{}.dseg", meta.device, meta.instance, meta.seq);
        std::fs::write(dir.path().join("buffer").join(&name), bytes).unwrap();
        old.clean_shutdown().unwrap();
        drop(old);
        let recovered = store(dir.path());
        assert_eq!(recovered.status().corrupt_segments, 1);
        assert_eq!(recovered.status().discarded_segments.decode, 1);
        assert!(recovered.segments.is_empty());
    }
}

#[test]
fn recovery_discard_stage_counts_reach_background_status() {
    let dir = tempfile::tempdir().unwrap();
    let me = DeviceId([107; 32]);
    let device = Anonymizer::for_membership(me, None).device(me);
    let mut old = store::Store::open(dir.path().into(), device, true).unwrap();
    old.recover_for_test().unwrap();
    old.snapshot = Snapshot::current_build();
    old.append(vec![motion(1)], 0).unwrap();
    let meta = old.segments.back().unwrap();
    let bytes = old.read(meta).unwrap();
    // Valid current-build bytes with a wrong filename fail validation.
    std::fs::write(dir.path().join("buffer/wrong-name.dseg"), &bytes).unwrap();
    let segment = store::decode(&bytes).unwrap();
    let mut raw = serde_json::to_vec(&segment).unwrap();
    raw.pop(); // Current-build truncated payload, valid compressed stream.
    let compressed = store::encode_raw(&raw).unwrap();
    let name = format!("{}-{}-{}.dseg", meta.device, meta.instance, meta.seq);
    std::fs::write(dir.path().join("buffer").join(name), compressed).unwrap();
    // Reserved DEFLATE block type: no producer can be inferred from these bytes.
    std::fs::write(dir.path().join("buffer/corrupt.dseg"), b"unrecognized format").unwrap();
    old.clean_shutdown().unwrap();
    drop(old);
    let recorder = Recorder::start(dir.path().into(), me, None, true).unwrap();
    recorder.wait_recovery();
    let status = recorder.status();
    assert_eq!(status.corrupt_segments, 3);
    assert_eq!(
        status.discarded_segments,
        DiscardCounts {
            read: 0,
            decompress: 0,
            decode: 1,
            validation: 1,
            format: 1,
            size_limit: 0,
        }
    );
    assert_eq!(status.errors, 0);
    assert!(!status.recovering);
    let statistics = crate::api::DiagnosticStatistics::from(&status);
    assert_eq!(statistics.discarded_segments, status.discarded_segments);
    recorder.shutdown();
}

#[test]
fn unreadable_old_segments_are_discarded_and_reported() {
    let temp = tempfile::tempdir().unwrap();
    let s = store(temp.path());
    s.clean_shutdown().unwrap();
    drop(s);
    let old = temp.path().join("buffer/unrecognized.dseg");
    std::fs::write(&old, b"old unpublished format").unwrap();
    let s = store(temp.path());
    assert_eq!(s.corrupt, 1);
    assert_eq!(s.status().corrupt_segments, 1);
    assert!(!old.exists());
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
    assert_eq!(manifest["schema"], 1);
    assert_eq!(manifest["corrupt_segments"], 1);
}

#[test]
fn save_result_has_coverage_without_scanning_saved_packages() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    s.append(vec![motion(0)], 0).unwrap();
    s.missing_members.insert("b".repeat(24), "offline".into());
    let bundle = s.export_sync(Reason::Manual, 0, 0, Some(false)).unwrap();
    assert_eq!(bundle.included_device_count, Some(1));
    assert_eq!(bundle.missing_device_count, Some(1));
    assert_eq!(bundle.cutoff_complete, Some(false));
    s.set_enabled(false).unwrap();
    assert!(std::path::Path::new(&bundle.path).exists());
    assert_eq!(s.folder().unwrap(), temp.path().to_string_lossy());
}

#[test]
fn all_save_reasons_and_startup_recovery_notify() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([82; 32]), Some(GroupId([83; 16])), true).unwrap();
    r.wait_recovery();
    let mut saved = r.saved_bundles();
    for reason in [Reason::Manual, Reason::Emergency, Reason::PeerRestart] {
        r.save(reason).unwrap();
        assert!(saved.has_changed().unwrap());
        saved.borrow_and_update();
    }
    // Simulate an unclean previous run entirely within a temporary recorder directory.
    r.inner.stopped.store(true, Ordering::SeqCst);
    drop(r);
    let r = isolated(temp.path().into(), DeviceId([82; 32]), Some(GroupId([83; 16])), true).unwrap();
    r.wait_recovery();
    assert_eq!(*r.saved_bundles().borrow(), 1);
    assert!(r.list().unwrap().iter().any(|v| v.reason == Reason::RecoveredCrash));
    r.shutdown();
}

#[test]
fn local_api_names_never_enter_exported_or_gathered_data() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    s.append(vec![motion(0)], 0).unwrap();
    s.missing_members.insert("b".repeat(24), "offline".into());
    let name = "private-display-device-name";
    let meta = &s.segments[0];
    // These are the exact segment bytes read by the production collection sender.
    let recorded = s.read(meta).unwrap();
    let segment = store::decode(&recorded).unwrap();
    assert!(!serde_json::to_string(&segment).unwrap().contains(name));
    assert!(
        !serde_json::to_vec(&segment)
            .unwrap()
            .windows(name.len())
            .any(|w| w == name.as_bytes())
    );
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    let mut archive = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    for i in 0..archive.len() {
        let mut text = String::new();
        archive.by_index(i).unwrap().read_to_string(&mut text).unwrap();
        assert!(!text.contains(name));
        assert!(!text.contains("device_name"));
    }
}

struct TestClock {
    mono: AtomicU64,
    wall: std::sync::atomic::AtomicI64,
}
impl TestClock {
    fn new(wall: i64) -> Arc<Self> {
        Arc::new(Self {
            mono: AtomicU64::new(0),
            wall: std::sync::atomic::AtomicI64::new(wall),
        })
    }
    fn advance(&self, mono: u64, wall: i64) {
        self.mono.fetch_add(mono, Ordering::SeqCst);
        self.wall.fetch_add(wall, Ordering::SeqCst);
    }
}
impl clock::Source for TestClock {
    fn read(&self) -> (u64, i64) {
        (self.mono.load(Ordering::SeqCst), self.wall.load(Ordering::SeqCst))
    }
}
pub(super) fn timeline(path: &str, name: &str) -> Vec<serde_json::Value> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut text = String::new();
    zip.by_name(name).unwrap().read_to_string(&mut text).unwrap();
    text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}
#[test]
fn suspend_and_clock_steps_keep_new_rows_at_actual_wall_time() {
    let gap = TEST_SPAN_US * 4;
    for (mono, wall) in [(0, gap), (gap as u64, 0), (1_000, -gap), (gap as u64, gap)] {
        let dir = tempfile::tempdir().unwrap();
        let source = TestClock::new(utc_us());
        let clock = clock::Clock::new(source.clone());
        let recorder = isolated_with_clock(
            dir.path().into(),
            DeviceId([1; 32]),
            Some(GroupId([2; 16])),
            true,
            clock,
        )
        .unwrap();
        recorder.wait_recovery();
        recorder.record(motion(0).event);
        recorder.drain(&mut lock(&recorder.inner.store)).unwrap();
        source.advance(mono, wall);
        recorder.record(motion(1).event);
        let bundle = recorder.save_range(None).unwrap();
        // Other tests can install the process-wide tracing subscriber. Its
        // background logs are legitimate arrivals, independent of these two
        // explicit input rows and the save's cutoff.
        let rows: Vec<_> = timeline(&bundle.path, "merged.ndjson")
            .into_iter()
            .filter(|row| row["event"].get("Input").is_some())
            .collect();
        let expected = source.wall.load(Ordering::SeqCst);
        assert!(
            rows.iter()
                .any(|r| r["original_utc_us"] == expected && r["mono_us"] == mono)
        );
        assert_eq!(rows.len(), 2);
        assert!(recorder.status().bytes > 0);
        assert_eq!(bundle.included_device_count, Some(1));
        // Freeze producers before asking for an idle tick: a save does not
        // promise that the live queue remains empty after its cutoff.
        recorder.set_suspended(true);
        recorder.drain(&mut lock(&recorder.inner.store)).unwrap();
        let s = lock(&recorder.inner.store);
        let tick = store::decode(&s.read(s.segments.back().unwrap()).unwrap()).unwrap();
        assert_eq!(tick.rows[0].utc_us, expected);
        drop(s);
        recorder.shutdown();
    }
}
#[test]
fn wall_backsteps_and_overlapping_segments_export_in_total_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path());
    let now = utc_us();
    for times in [[30, 10], [20, 5], [15, 25]] {
        let rows = times
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let mut row = motion(i as u64);
                row.utc_us = now + t;
                row
            })
            .collect();
        s.append(rows, 0).unwrap();
    }
    assert_eq!(s.status().oldest_utc_us, Some(now + 5));
    assert_eq!(s.status().newest_utc_us, Some(now + 30));
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    for name in ["merged.ndjson", "devices/device-A/timeline.ndjson"] {
        let rows = timeline(&bundle.path, name);
        let times = rows
            .iter()
            .map(|r| r["unified_utc_us"].as_i64().unwrap() - now)
            .collect::<Vec<_>>();
        assert_eq!(times, [5, 10, 15, 20, 25, 30]);
    }
}
#[test]
fn clock_epoch_changes_prevent_stale_calibration_after_sleep_or_time_steps() {
    let dir = tempfile::tempdir().unwrap();
    let now = utc_us();
    let source = TestClock::new(now);
    let clock = clock::Clock::new(source.clone());
    let mut s = store::Store::open_with_clock(dir.path().into(), "a".repeat(24), true, clock.clone()).unwrap();
    let remote = "b".repeat(24);
    let instance = "c".repeat(24);
    let sample = |source_clock, local_clock, offset, uncertainty| ClockSample {
        device: remote.clone(),
        instance: instance.clone(),
        at_us: now,
        source_clock,
        local_clock,
        local_instance: s.instance.clone(),
        first_source_mono_us: 0,
        last_source_mono_us: 0,
        offset_us: offset,
        uncertainty_us: uncertainty,
    };
    // A low-RTT old winner must not calibrate a new source epoch.
    s.clocks = vec![sample(0, 0, 500, 1), sample(1, 0, 9_000_000, 100)];
    let mut row = motion(0);
    row.utc_us = now + 9_000_000;
    row.clock_id = 1;
    let segment = store::Segment {
        schema: SCHEMA,
        device: remote,
        instance,
        seq: 1,
        dropped: 0,
        snapshot: Snapshot::default(),
        clock_samples: Vec::new(),
        rows: vec![row],
    };
    s.insert(&segment, &store::encode(&segment).unwrap()).unwrap();
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    assert_eq!(rows[0]["unified_utc_us"], now);
    assert_eq!(rows[0]["clock_calibrated"], true);
    source.advance(0, TEST_SPAN_US * 4);
    let stamp = clock.now();
    assert_ne!(stamp.clock_id, 0);
    // Historical rows inside an observed old interval keep their original calibration.
    // A future row cannot extend that closed interval after the receiver changes epoch.
    assert!(s.offset(&s.segments[0], stamp.clock_id).is_some());
    s.prune().unwrap();
    assert_eq!(s.segments.len(), 1);
    let now_after_wake = source.wall.load(Ordering::SeqCst);
    s.clocks.push(ClockSample {
        device: segment.device.clone(),
        instance: segment.instance.clone(),
        at_us: now_after_wake,
        source_clock: 2,
        local_clock: stamp.clock_id,
        local_instance: s.instance.clone(),
        first_source_mono_us: 0,
        last_source_mono_us: 0,
        offset_us: 42_000,
        uncertainty_us: 400_100,
    });
    let mut fresh = segment.clone();
    fresh.seq = 2;
    fresh.rows[0].clock_id = 2;
    fresh.rows[0].utc_us = now_after_wake + 42_000;
    s.insert(&fresh, &store::encode(&fresh).unwrap()).unwrap();
    let mut local = motion(0);
    local.utc_us = now_after_wake;
    local.clock_id = stamp.clock_id;
    s.append(vec![local], 0).unwrap();
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    assert_eq!(bundle.included_device_count, Some(2));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["unified_utc_us"], now);
    assert!(rows.iter().all(|row| row["clock_calibrated"] == true));
    assert!(rows[1..].iter().all(|row| row["unified_utc_us"] == now_after_wake));
    source.advance(0, -TEST_SPAN_US * 4);
    assert_ne!(
        clock.now().clock_id,
        0,
        "returning to the old wall value is a new epoch"
    );
}

#[test]
fn strict_segment_decode_rejects_trailing_data_and_unrecognized_headers() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path());
    s.append(vec![motion(0)], 0).unwrap();
    let bytes = s.read(&s.segments[0]).unwrap();
    let segment = store::decode(&bytes).unwrap();
    let mut raw = serde_json::to_vec(&segment).unwrap();
    raw.extend_from_slice(b" trailing");
    assert!(store::decode(&store::encode_raw(&raw).unwrap()).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(store::decode(&trailing).is_err());
    assert!(store::decode(&bytes[12..]).is_err());
    let mut unknown = bytes;
    unknown[8] = 2;
    assert!(store::decode(&unknown).is_err());
}

#[test]
fn concurrent_clock_steps_never_share_an_unrelated_calibration_epoch() {
    struct Stepping(AtomicU64);
    impl clock::Source for Stepping {
        fn read(&self) -> (u64, i64) {
            let mono = self.0.fetch_add(1, Ordering::SeqCst);
            (mono, 1_000_000_000 + mono as i64 * 1_000_000)
        }
    }
    let clock = clock::Clock::new(Arc::new(Stepping(AtomicU64::new(0))));
    let threads = (0..4)
        .map(|_| {
            let clock = clock.clone();
            std::thread::spawn(move || (0..1000).map(|_| clock.now()).collect::<Vec<_>>())
        })
        .collect::<Vec<_>>();
    let mut epochs = std::collections::BTreeMap::new();
    for stamp in threads.into_iter().flat_map(|thread| thread.join().unwrap()) {
        if stamp.clock_id >= 0 {
            let basis = stamp.utc_us - stamp.mono_us as i64;
            if let Some(previous) = epochs.insert(stamp.clock_id, basis) {
                assert!(basis.abs_diff(previous) <= 2 * clock::EPOCH_TOLERANCE_US);
            }
        }
    }
}

#[test]
fn receiver_time_step_does_not_rewrite_remote_history_with_new_offset() {
    let dir = tempfile::tempdir().unwrap();
    let now = utc_us();
    let source = TestClock::new(now);
    let clock = clock::Clock::new(source.clone());
    let mut s = store::Store::open_with_clock(dir.path().into(), "a".repeat(24), true, clock.clone()).unwrap();
    let mut old = ClockSample::estimate("b".repeat(24), "c".repeat(24), now, now + 510, now + 520, now + 30).unwrap();
    old.local_instance = s.instance.clone();
    old.first_source_mono_us = 10;
    old.last_source_mono_us = 100;
    s.clocks.push(old.clone());
    let mut local = motion(50);
    local.utc_us = now + 50;
    s.append(vec![local], 0).unwrap();
    let step = 10_000_000;
    source.advance(200, step + 200);
    let current = clock.now().clock_id;
    let mut new = old.clone();
    new.local_clock = current;
    new.first_source_mono_us = 200;
    new.last_source_mono_us = 300;
    new.offset_us = 500 - step;
    s.clocks.push(new);
    for (seq, mono) in [(1, 50), (2, 150), (3, 250)] {
        let mut row = motion(mono);
        row.utc_us = now + 500 + mono as i64;
        let segment = store::Segment {
            schema: SCHEMA,
            device: old.device.clone(),
            instance: old.instance.clone(),
            seq,
            dropped: 0,
            snapshot: Snapshot::default(),
            clock_samples: vec![],
            rows: vec![row],
        };
        s.insert(&segment, &store::encode(&segment).unwrap()).unwrap();
    }
    let mut local = motion(250);
    local.clock_id = current;
    local.utc_us = now + step + 250;
    s.append(vec![local], 0).unwrap();
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    for (mono, unified, calibrated) in [
        (50, now + 50, true),
        (150, now + 650, false),
        (250, now + step + 250, true),
    ] {
        let row = rows
            .iter()
            .find(|r| r["device"] == old.device && r["mono_us"] == mono)
            .unwrap();
        assert_eq!(row["unified_utc_us"], unified);
        assert_eq!(row["clock_calibrated"], calibrated);
    }
    for row in timeline(&bundle.path, "devices/device-B/timeline.ndjson") {
        assert!(rows.contains(&row));
    }
}

#[test]
fn recovered_clock_samples_union_observed_intervals() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(dir.path());
    let mut sample = ClockSample::estimate("b".repeat(24), "c".repeat(24), 100, 110, 120, 130).unwrap();
    sample.local_instance = s.instance.clone();
    sample.first_source_mono_us = 10;
    sample.last_source_mono_us = 20;
    s.clocks.push(sample);
    s.append(vec![motion(0)], 0).unwrap();
    s.clocks[0].last_source_mono_us = 30;
    s.append(vec![motion(1)], 0).unwrap();
    s.clean_shutdown().unwrap();
    drop(s);
    let s = store(dir.path());
    assert_eq!(s.clocks.len(), 1);
    assert_eq!(s.clocks[0].first_source_mono_us, 10);
    assert_eq!(s.clocks[0].last_source_mono_us, 30);
    s.clean_shutdown().unwrap();
}

// A bounded synthetic sample with distinct numeric values and overlapping clocks.
fn sample_buffer(s: &mut store::Store, target: u64) -> usize {
    let now = utc_us();
    let mut random = 0x1234_5678_9abc_def0_u64;
    let mut rows = 0;
    for seq in 0..16_u64 {
        let events = (0..64)
            .map(|index| {
                let fields = (0..16)
                    .map(|field| {
                        random ^= random << 13;
                        random ^= random >> 7;
                        random ^= random << 17;
                        (format!("n{field}"), (random as i64) as f64 / 1024.0)
                    })
                    .collect();
                Row {
                    mono_us: seq * 1_000_000 + index * 1000,
                    clock_id: 0,
                    utc_us: now - TEST_SPAN_US
                        + 1_000_000
                        // Repeated wall-clock backsteps create dozens of overlapping
                        // runs in each band across the complete retention window.
                        + (seq % 32) as i64 * ((TEST_SPAN_US - 1_512_000) / 31)
                        + index as i64 * 1000,
                    event: Event::Log {
                        module: "synthetic".into(),
                        line: 1,
                        level: 2,
                        fields,
                        labels: vec![],
                    },
                }
            })
            .collect::<Vec<_>>();
        rows += events.len();
        s.append(events, 0).unwrap();
        if s.bytes >= target || (target > MAX_BYTES && s.evicted > 0) {
            break;
        }
    }
    rows
}

#[test]
fn export_numeric_sample_preserves_both_timelines() {
    let temp = tempfile::tempdir().unwrap();
    // Stable timestamps keep the synthetic fixture reproducible.
    let source = TestClock::new(utc_us());
    let mut s =
        store::Store::open_with_clock(temp.path().into(), "a".repeat(24), true, clock::Clock::new(source)).unwrap();
    let target = 16 * 1024;
    let rows = sample_buffer(&mut s, target);
    assert!(s.bytes >= target, "fixture never reached compressed retention limit");
    verify_export(&s, rows);
}

fn verify_export(s: &store::Store, rows: usize) {
    let snapshot = s.export_snapshot().unwrap();
    let bundle = snapshot.export(Reason::Manual, 0, 0).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    let mut digests = Vec::new();
    for name in ["devices/device-A/timeline.ndjson", "merged.ndjson"] {
        let mut entry = zip.by_name(name).unwrap();
        let mut digest = blake3::Hasher::new();
        let mut count = 0;
        let mut bytes = [0; 64 * 1024];
        loop {
            let len = entry.read(&mut bytes).unwrap();
            if len == 0 {
                break;
            }
            digest.update(&bytes[..len]);
            count += bytes[..len].iter().filter(|b| **b == b'\n').count();
        }
        assert_eq!(count, rows);
        digests.push(digest.finalize());
    }
    assert_eq!(digests[0], digests[1]);
}

#[test]
fn export_does_not_block_recording_flushing_and_pins_cutoff() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([61; 32]), Some(GroupId([62; 16])), true).unwrap();
    r.wait_recovery();
    r.record(motion(0).event);
    let (ready, waiting) = std::sync::mpsc::channel();
    let (resume, paused) = std::sync::mpsc::channel();
    let paused = Mutex::new(paused);
    *lock(&r.inner.export_hook) = Some(Arc::new(move || {
        ready.send(()).unwrap();
        lock(&paused).recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    let saver = r.clone();
    let export = std::thread::spawn(move || saver.save(Reason::Manual).unwrap());
    waiting
        .recv_timeout(Duration::from_secs(5))
        .expect("save did not reach detached export");
    let writer = r.clone();
    let (done, completed) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let begin = Instant::now();
        // More than one queue capacity while export is deliberately suspended.
        for batch in 0..3 {
            for i in 0..4096 {
                writer.record(motion(1 + batch * 4096 + i).event);
            }
            let mut s = lock(&writer.inner.store);
            writer.drain(&mut s).unwrap();
            let replica = store::Segment {
                schema: SCHEMA,
                device: "b".repeat(24),
                instance: "c".repeat(24),
                seq: batch,
                dropped: 0,
                snapshot: Snapshot::default(),
                clock_samples: vec![],
                rows: vec![motion(10)],
            };
            s.insert(&replica, &store::encode(&replica).unwrap()).unwrap();
        }
        assert_eq!(writer.inner.dropped.load(Ordering::Relaxed), 0);
        // Delete original paths before resuming the exporter; pinned links survive.
        let mut s = lock(&writer.inner.store);
        s.limit = 0;
        s.prune().unwrap();
        assert!(s.segments.is_empty());
        done.send(begin.elapsed()).unwrap();
    });
    let elapsed = completed
        .recv_timeout(Duration::from_secs(5))
        .expect("export blocked the recorder/replica writer");
    resume.send(()).unwrap();
    writer.join().unwrap();
    let bundle = export.join().unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    let inputs = rows
        .iter()
        .filter(|row| row["event"]["Input"].is_object())
        .collect::<Vec<_>>();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["event"]["Input"]["original_us"], 0);
    eprintln!(
        "DIAG_CONCURRENT rows=12288 dropped=0 elapsed_ms={}",
        elapsed.as_millis()
    );
    *lock(&r.inner.export_hook) = None;
    r.shutdown();
}

#[test]
fn simultaneous_save_requests_share_success_and_error_results() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([63; 32]), Some(GroupId([64; 16])), true).unwrap();
    r.wait_recovery();
    let count = Arc::new(AtomicU64::new(0));
    for fail in [false, true] {
        // All clients observed this generation before any completion, without
        // relying on scheduler timing or sleeps to make concurrent callers overlap.
        let generation = r.inner.save_generation.load(Ordering::Acquire);
        let handles = (0..8)
            .map(|_| {
                let r = r.clone();
                let count = count.clone();
                std::thread::spawn(move || {
                    r.finish_save(generation, Reason::Manual, None, || {
                        count.fetch_add(1, Ordering::Relaxed);
                        if fail {
                            anyhow::bail!("synthetic export failure");
                        }
                        r.export_cutoff(Reason::Manual, None)
                    })
                })
            })
            .collect::<Vec<_>>();
        let results = handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>();
        if fail {
            assert!(
                results
                    .iter()
                    .all(|r| r.as_ref().unwrap_err().to_string() == "synthetic export failure")
            );
        } else {
            assert!(
                results
                    .iter()
                    .all(|r| r.as_ref().unwrap().path == results[0].as_ref().unwrap().path)
            );
        }
    }
    assert_eq!(count.load(Ordering::Relaxed), 2);
    let waiting_generation = r.inner.save_generation.load(Ordering::Acquire) - 1;
    r.save(Reason::Emergency).unwrap();
    let error = r
        .finish_save(waiting_generation, Reason::Manual, None, || {
            anyhow::bail!("must join the earlier manual result")
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "synthetic export failure");
    assert_eq!(r.list().unwrap().len(), 2);
    r.shutdown();
}

#[test]
fn export_pointer_sample_preserves_both_timelines() {
    let temp = tempfile::tempdir().unwrap();
    let now = utc_us();
    let clock = clock::Clock::new(TestClock::new(now));
    let mut s = store::Store::open_with_clock(temp.path().into(), "a".repeat(24), true, clock).unwrap();
    for second in 0..6_u64 {
        let rows = (0..64)
            .map(|index| {
                let n = second * 64 + index;
                let mut row = motion(n);
                row.utc_us = now - TEST_SPAN_US + n as i64 * 1000 + 1;
                row
            })
            .collect();
        s.append(rows, 0).unwrap();
    }
    assert_eq!(s.segments.len(), 6);
    verify_export(&s, 384);
}

#[test]
fn failed_export_removes_private_spool_and_partial_archive() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    s.append(vec![motion(0)], 0).unwrap();
    // Immutable cutoff is readable until a segment is deliberately corrupted.
    let snapshot = s.export_snapshot().unwrap();
    for entry in std::fs::read_dir(temp.path().join("buffer")).unwrap() {
        std::fs::write(entry.unwrap().path(), b"broken").unwrap();
    }
    assert!(snapshot.export(Reason::Manual, 0, 0).is_err());
    drop(snapshot);
    assert!(std::fs::read_dir(temp.path()).unwrap().all(|e| {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        !name.ends_with(".partial") && !name.starts_with("export-") && !name.ends_with(".zip")
    }));
}

#[test]
fn collection_range_reuses_whole_segments_and_filters_pinned_edges() {
    let directory = tempfile::tempdir().unwrap();
    let mut source = store(directory.path());
    let now = utc_us();
    for offsets in [[-30, -20], [-15, -5], [-4, -3], [5, 6]] {
        let rows = offsets
            .into_iter()
            .enumerate()
            .map(|(i, offset)| {
                let mut row = motion(i as u64);
                row.utc_us = now + offset;
                row
            })
            .collect();
        source.append(rows, 0).unwrap();
    }
    let whole = source.read(&source.segments[2]).unwrap();
    let mut snapshot = source.export_snapshot().unwrap();
    snapshot.restrict_range(now - 10, now).unwrap();
    assert_eq!(snapshot.segments.len(), 2);
    assert_eq!(snapshot.read(&snapshot.segments[1]).unwrap(), whole);
    let filtered = store::decode(&snapshot.read(&snapshot.segments[0]).unwrap()).unwrap();
    assert_eq!(filtered.rows.len(), 1);
    assert_eq!(filtered.rows[0].utc_us, now - 5);
    assert_eq!(
        store::decode(&source.read(&source.segments[1]).unwrap())
            .unwrap()
            .rows
            .len(),
        2,
        "replacing a staging hard link must not change the live recorder"
    );
    assert_eq!(
        source
            .collection_segment(&source.segments[1], now - 10, now)
            .unwrap()
            .unwrap(),
        snapshot.read(&snapshot.segments[0]).unwrap()
    );
    assert!(
        source
            .collection_segment(&source.segments[0], now - 10, now)
            .unwrap()
            .is_none()
    );
}

#[test]
fn startup_removes_export_pins_and_partials_left_by_a_fatal_exit() {
    let temp = tempfile::tempdir().unwrap();
    let s = store(temp.path());
    let bundle = s.export(Reason::Manual, 0, 0).unwrap();
    s.clean_shutdown().unwrap();
    drop(s);
    let stale = temp.path().join("export-stale");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("pinned.dseg"), b"synthetic").unwrap();
    std::fs::write(temp.path().join("123-manual-stale.partial"), b"synthetic").unwrap();
    let s = store(temp.path());
    assert!(!stale.exists());
    assert!(!temp.path().join("123-manual-stale.partial").exists());
    assert_eq!(s.list().unwrap()[0].path, bundle.path);
}

// The child uses only a fake input backend, an isolated data directory and test
// network ports. No installed process or desktop input is involved.
#[cfg(unix)]
#[test]
fn startup_child() {
    use crate::host::{AgentHost, IpcMode};
    use std::io::Write;
    let Ok(dir) = std::env::var("OINK_DIAG_STARTUP_HOME") else {
        return;
    };
    let mode = std::env::var("OINK_DIAG_STARTUP_MODE").unwrap();
    let opts = crate::node::NodeOptions {
        net: oink_test_support::network_config(dir.into(), "synthetic-startup", 0),
        input: Arc::new(oink_test_support::input::FakeInput::new()),
        displays: vec![],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    };
    println!("startup-before-host");
    std::io::stdout().flush().unwrap();
    let started = AgentHost::start_with(
        opts,
        if mode == "fixture" {
            IpcMode::Off
        } else {
            IpcMode::Required
        },
    );
    let host = match started {
        Ok(host) => host,
        Err(error) if error.downcast_ref::<crate::host::StartupTerminated>().is_some() => {
            assert_eq!(mode, "serve");
            println!("startup-terminated");
            return;
        }
        Err(error) => panic!("host startup failed: {error:#}"),
    };
    if mode == "fixture" {
        let r = &host.node().inner.diagnostics.get().unwrap().recorder;
        r.wait_recovery();
        let mut s = lock(&r.inner.store);
        let now = utc_us();
        let mut random = 0x1234_5678_9abc_def0_u64;
        for seq in 0..8_u64 {
            let rows = (0..8)
                .map(|index| Row {
                    mono_us: seq * 1_000_000 + index * 1000,
                    clock_id: 0,
                    utc_us: now - TEST_SPAN_US + 1_000_000 + seq as i64 * 999_000 + index as i64 * 1000,
                    event: Event::Log {
                        module: "synthetic".into(),
                        line: 1,
                        level: 2,
                        fields: (0..16)
                            .map(|field| {
                                random ^= random << 13;
                                random ^= random >> 7;
                                random ^= random << 17;
                                (format!("n{field}"), random as i64 as f64 / 1024.0)
                            })
                            .collect(),
                        labels: vec![],
                    },
                })
                .collect();
            s.append(rows, 0).unwrap();
        }
        assert_eq!(s.segments.len(), 8);
        println!("STARTUP_FIXTURE bytes={} segments={}", s.bytes, s.segments.len());
        let instance = s.instance.clone();
        drop(s);
        let marker = host.node().data_dir().join("Logs/Diagnostics/running");
        host.shutdown();
        std::fs::write(marker, instance).unwrap();
    } else {
        println!("startup-ipc-ready");
        println!(
            "startup-recovery-state:{}",
            host.node()
                .inner
                .diagnostics
                .get()
                .unwrap()
                .recorder
                .status()
                .recovering
        );
        std::io::stdout().flush().unwrap();
        host.block_on(host.node().wait_for_stop());
        host.shutdown();
    }
}

#[cfg(unix)]
#[test]
fn isolated_process_startup_and_term_are_bounded() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let temp = tempfile::tempdir().unwrap();
    let spawn = |mode| {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["diagnostics::tests::startup_child", "--exact", "--nocapture"])
            .env("OINK_DIAG_STARTUP_HOME", temp.path())
            .env("OINK_DIAG_STARTUP_MODE", mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    };
    let fixture = spawn("fixture").output().unwrap();
    assert!(fixture.status.success(), "{}", String::from_utf8_lossy(&fixture.stderr));
    eprint!("{}", String::from_utf8_lossy(&fixture.stdout));
    let marker = temp.path().join("Logs/Diagnostics/running");
    let previous = std::fs::read(&marker).unwrap();
    for term_during_start in [false, true] {
        std::fs::write(&marker, &previous).unwrap();
        let begin = Instant::now();
        let mut child = spawn("serve").stderr(Stdio::inherit()).spawn().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let out = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let line = line.unwrap();
                if line.starts_with("startup-recovery-state:") {
                    eprintln!("{line}");
                }
                if line.starts_with("startup-") {
                    let _ = send.send(line);
                }
            }
        });
        let marker_text = if term_during_start {
            "startup-before-host"
        } else {
            "startup-ipc-ready"
        };
        loop {
            let line = match receive.recv_timeout(Duration::from_secs(30)) {
                Ok(line) => line,
                Err(e) => {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("startup marker: {e}");
                }
            };
            if line == marker_text {
                break;
            }
        }
        if term_during_start {
            // The private child registers its stop signal before recorder startup.
            std::thread::sleep(Duration::from_millis(100));
        } else {
            crate::ipc::Client::connect(temp.path())
                .unwrap()
                .hello_with_timeout(Duration::from_secs(1))
                .unwrap();
            let elapsed = begin.elapsed();
            eprintln!("STARTUP_PERF ipc_ms={}", elapsed.as_millis());
        }
        let term = Instant::now();
        // SAFETY: only our isolated child with fake input receives the signal.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if term.elapsed() > Duration::from_secs(15) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("TERM did not stop startup");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        reader.join().unwrap();
        eprintln!(
            "STARTUP_PERF term_during_start={term_during_start} exit_ms={}",
            term.elapsed().as_millis()
        );
        assert!(status.success(), "child failed: {status}");
    }
}

#[test]
fn open_returns_before_any_segment_content_is_read_or_validated() {
    let temp = tempfile::tempdir().unwrap();
    let mut old = store(temp.path());
    old.append(vec![motion(0)], 0).unwrap();
    old.clean_shutdown().unwrap();
    drop(old);
    std::fs::write(temp.path().join("buffer/unrecognized.dseg"), b"corrupt").unwrap();
    let mut s = store::Store::open(temp.path().into(), "a".repeat(24), true).unwrap();
    assert_eq!(s.segment_reads.load(Ordering::Relaxed), 0);
    assert_eq!(s.corrupt, 0);
    assert!(s.segments.is_empty());
    assert_eq!(std::fs::read_dir(temp.path().join("buffer")).unwrap().count(), 0);
    s.recover_for_test().unwrap();
    assert_eq!(s.segment_reads.load(Ordering::Relaxed), 2);
    assert_eq!(s.corrupt, 1);
    assert_eq!(s.segments.len(), 1);
}

#[test]
fn paused_recovery_records_new_events_rejects_saves_and_cancels_durably() {
    let temp = tempfile::tempdir().unwrap();
    let me = DeviceId([101; 32]);
    let group = GroupId([102; 16]);
    let mut old = store::Store::open(temp.path().into(), Anonymizer::new(group).device(me), true).unwrap();
    old.recover_for_test().unwrap();
    old.append(vec![motion(0)], 0).unwrap();
    let previous = old.instance.clone();
    drop(old); // simulate a prior crash, preserving its marker
    let (ready, waiting) = std::sync::mpsc::channel();
    let r = Recorder::start_inner(
        temp.path().into(),
        me,
        Some(group),
        true,
        false,
        clock::Clock::system(),
        None,
        Some(Arc::new(move |cancelled| {
            ready.send(()).unwrap();
            while !cancelled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })),
        false,
    )
    .unwrap();
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(r.status().recovering);
    assert!(
        r.save(Reason::Manual)
            .unwrap_err()
            .to_string()
            .contains("diagnostics recovering")
    );
    for batch in 0..3 {
        for i in 0..4096 {
            r.record(motion(1 + batch * 4096 + i).event);
        }
        r.drain(&mut lock(&r.inner.store)).unwrap();
    }
    assert_eq!(r.status().dropped, 0);
    assert_eq!(r.status().queued, 0);
    let begin = Instant::now();
    r.shutdown();
    assert!(begin.elapsed() < Duration::from_secs(5));
    assert_eq!(std::fs::read_to_string(temp.path().join("running")).unwrap(), previous);
    let r = isolated(temp.path().into(), me, Some(group), true).unwrap();
    r.wait_recovery();
    let recovered = r
        .list()
        .unwrap()
        .into_iter()
        .find(|b| b.reason == Reason::RecoveredCrash)
        .unwrap();
    let rows = timeline(&recovered.path, "merged.ndjson");
    assert_eq!(
        rows.iter().filter(|row| row["event"]["Input"].is_object()).count(),
        12289
    );
    assert_eq!(r.status().dropped, 0);
    r.shutdown();
    assert!(!temp.path().join("running").exists());
}

#[test]
fn disabling_during_recovery_does_not_resurrect_the_discarded_window() {
    let temp = tempfile::tempdir().unwrap();
    let me = DeviceId([103; 32]);
    let group = GroupId([104; 16]);
    let mut old = store::Store::open(temp.path().into(), Anonymizer::new(group).device(me), true).unwrap();
    old.recover_for_test().unwrap();
    old.append(vec![motion(0)], 0).unwrap();
    drop(old);
    let (ready, waiting) = std::sync::mpsc::channel();
    let r = Recorder::start_inner(
        temp.path().into(),
        me,
        Some(group),
        true,
        false,
        clock::Clock::system(),
        None,
        Some(Arc::new(move |cancelled| {
            ready.send(()).unwrap();
            while !cancelled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })),
        false,
    )
    .unwrap();
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    r.set_enabled(false).unwrap();
    assert_eq!(r.status().segments, 0);
    r.set_enabled(true).unwrap();
    r.record(motion(17).event);
    r.shutdown();
    let r = isolated(temp.path().into(), me, Some(group), true).unwrap();
    r.wait_recovery();
    assert!(r.list().unwrap().is_empty());
    let bundle = r.save(Reason::Manual).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    let inputs = rows
        .iter()
        .filter(|row| row["event"]["Input"].is_object())
        .collect::<Vec<_>>();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["event"]["Input"]["original_us"], 17);
    r.shutdown();
}
#[test]
fn suspension_and_local_save_preserve_incident_without_waiting_for_export() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([71; 32]), Some(GroupId([72; 16])), true).unwrap();
    r.record(motion(0).event);
    // Suspension may win the scheduling race against the emergency saver.
    r.set_suspended(true);
    let (ready, waiting) = std::sync::mpsc::channel();
    let (resume, paused) = std::sync::mpsc::channel();
    let paused = Mutex::new(paused);
    *lock(&r.inner.export_hook) = Some(Arc::new(move || {
        ready.send(()).unwrap();
        lock(&paused).recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    let saver = r.clone();
    let export = std::thread::spawn(move || saver.save(Reason::Emergency).unwrap());
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let suspended = r.clone();
    let (done, completed) = std::sync::mpsc::channel();
    let suspender = std::thread::spawn(move || {
        suspended.set_suspended(true);
        assert!(!suspended.enabled());
        suspended.record(motion(1).event);
        done.send(()).unwrap();
    });
    completed
        .recv_timeout(Duration::from_secs(1))
        .expect("pause waited for compression");
    resume.send(()).unwrap();
    suspender.join().unwrap();
    let emergency = export.join().unwrap();
    assert_eq!(emergency.reason, Reason::Emergency);
    assert_eq!(emergency.cutoff_complete, None);
    assert_eq!(timeline(&emergency.path, "merged.ndjson").len(), 1);
    *lock(&r.inner.export_hook) = None;
    let manual = r.save(Reason::Manual).unwrap();
    assert_eq!(manual.cutoff_complete, None);
    assert_eq!(timeline(&manual.path, "merged.ndjson").len(), 1);
    r.shutdown();
}

#[test]
fn rekey_while_suspended_preserves_recording_preference() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([73; 32]), Some(GroupId([74; 16])), true).unwrap();
    r.set_suspended(true);
    r.rekey(DeviceId([73; 32]), Some(GroupId([75; 16]))).unwrap();
    assert!(!r.enabled());
    r.set_suspended(false);
    assert!(r.enabled());
    r.shutdown();
}

#[test]
fn suspended_start_recovers_crash_and_emergency_waits_for_the_complete_window() {
    let _scenario = crate::test_watchdog::Scenario::new(
        module_path!(),
        "suspended_start_recovers_crash_and_emergency_waits_for_the_complete_window",
    );
    let input_rows = |path: &str| {
        timeline(path, "merged.ndjson")
            .into_iter()
            .filter(|row| row["event"].get("Input").is_some())
            .collect::<Vec<_>>()
    };
    for suspended_at_start in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let me = DeviceId([76; 32]);
        let group = GroupId([77; 16]);
        let mut old = store::Store::open(temp.path().into(), Anonymizer::new(group).device(me), true).unwrap();
        old.recover_for_test().unwrap();
        old.append(vec![motion(42)], 0).unwrap();
        drop(old); // crash before entering full pause
        let (ready, waiting) = std::sync::mpsc::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        let paused = Mutex::new(paused);
        let r = Recorder::start_inner(
            temp.path().into(),
            me,
            Some(group),
            true,
            suspended_at_start,
            clock::Clock::system(),
            None,
            Some(Arc::new(move |cancelled| {
                ready.send(()).unwrap();
                loop {
                    if cancelled.load(Ordering::Acquire) || lock(&paused).try_recv().is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })),
            false,
        )
        .unwrap();
        waiting.recv().unwrap();
        assert_eq!(r.enabled(), !suspended_at_start);
        r.record(motion(43).event);
        r.set_suspended(true);
        assert!(!r.enabled());
        assert_eq!(r.status().queued, 0);
        assert_eq!(r.status().segments, usize::from(!suspended_at_start));
        assert!(r.status().recovering);
        // An unchanged preference must not cancel recovery or erase the crash.
        r.set_enabled(true).unwrap();
        assert!(r.status().recovering);
        assert!(
            r.save(Reason::Manual)
                .unwrap_err()
                .to_string()
                .contains("diagnostics recovering")
        );
        let (save_ready, save_waiting) = std::sync::mpsc::channel();
        *lock(&r.inner.automatic_wait_hook) = Some(Arc::new(move || save_ready.send(()).unwrap()));
        let saver = r.clone();
        let export = std::thread::spawn(move || saver.save(Reason::Emergency));
        save_waiting.recv().unwrap();
        // The save is waiting for recovery, without owning the export gate.
        assert!(r.inner.save.try_lock().is_ok());
        assert!(r.status().recovering);
        resume.send(()).unwrap();
        crate::test_watchdog::phase("emergency save receives complete recovery window");
        let emergency = export.join().unwrap().unwrap();
        r.wait_recovery();
        assert!(!r.enabled());
        assert!(!r.status().recovering);
        assert_eq!(r.status().errors, 0);
        assert_eq!(emergency.cutoff_complete, None);
        let rows = input_rows(&emergency.path);
        assert_eq!(rows.len(), if suspended_at_start { 1 } else { 2 });
        assert!(rows.iter().any(|row| row["event"]["Input"]["original_us"] == 42));
        let crash = r
            .list()
            .unwrap()
            .into_iter()
            .find(|b| b.reason == Reason::RecoveredCrash)
            .unwrap();
        assert_eq!(input_rows(&crash.path).len(), 1);
        assert_eq!(input_rows(&r.save(Reason::Manual).unwrap().path).len(), rows.len());
        r.shutdown();
    }
}

#[test]
fn peer_restart_save_waits_for_recovery_and_cancels_on_shutdown() {
    let _scenario = crate::test_watchdog::Scenario::new(
        module_path!(),
        "peer_restart_save_waits_for_recovery_and_cancels_on_shutdown",
    );
    for cancel in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (resume, paused) = std::sync::mpsc::channel();
        let paused = Mutex::new(paused);
        let r = Recorder::start_inner(
            temp.path().into(),
            DeviceId([105; 32]),
            None,
            true,
            false,
            clock::Clock::system(),
            None,
            Some(Arc::new(move |cancelled| {
                while !cancelled.load(Ordering::Acquire) && lock(&paused).try_recv().is_err() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })),
            false,
        )
        .unwrap();
        let (event, events) = std::sync::mpsc::channel();
        let waiting = event.clone();
        *lock(&r.inner.automatic_wait_hook) = Some(Arc::new(move || waiting.send(true).unwrap()));
        let saver = r.clone();
        let export = std::thread::spawn(move || {
            let result = saver.save(Reason::PeerRestart);
            event.send(false).unwrap();
            result
        });
        let waited = events.recv().unwrap();
        let save_lock_free = r.inner.save.try_lock().is_ok();
        if cancel {
            r.shutdown();
        } else {
            resume.send(()).unwrap();
        }
        let result = export.join().unwrap();
        r.wait_recovery();
        r.shutdown();
        assert!(waited, "automatic save returned before recovery: {result:?}");
        assert!(save_lock_free, "waiting must not hold the recovery export gate");
        if cancel {
            assert!(result.unwrap_err().to_string().contains("cancelled"));
        } else {
            assert_eq!(result.unwrap().reason, Reason::PeerRestart);
            assert_eq!(r.list().unwrap().len(), 1, "one saved peer-restart bundle");
        }
    }
}

#[test]
fn startup_cancellation_wakes_emergency_save_and_preserves_unfinished_crash() {
    let temp = tempfile::tempdir().unwrap();
    let me = DeviceId([78; 32]);
    let group = GroupId([79; 16]);
    let mut old = store::Store::open(temp.path().into(), Anonymizer::new(group).device(me), true).unwrap();
    old.recover_for_test().unwrap();
    old.append(vec![motion(44)], 0).unwrap();
    let previous = old.instance.clone();
    drop(old);
    let (ready, waiting) = std::sync::mpsc::channel();
    let r = Recorder::start_inner(
        temp.path().into(),
        me,
        Some(group),
        true,
        true,
        clock::Clock::system(),
        None,
        Some(Arc::new(move |cancelled| {
            ready.send(()).unwrap();
            while !cancelled.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })),
        false,
    )
    .unwrap();
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let (save_ready, save_waiting) = std::sync::mpsc::channel();
    *lock(&r.inner.automatic_wait_hook) = Some(Arc::new(move || save_ready.send(()).unwrap()));
    let saver = r.clone();
    let export = std::thread::spawn(move || saver.save(Reason::Emergency));
    save_waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(StartupRecoveryGuard(Some(r.clone()))); // Node future cancelled before host ownership
    let error = export.join().unwrap().unwrap_err().to_string();
    assert!(error.contains("cancelled"), "{error}");
    assert!(!r.status().recovering);
    assert_eq!(std::fs::read_to_string(temp.path().join("running")).unwrap(), previous);
    let restarted = isolated_suspended(temp.path().into(), me, Some(group), true, true).unwrap();
    restarted.wait_recovery();
    assert!(!restarted.enabled());
    let crash = restarted
        .list()
        .unwrap()
        .into_iter()
        .find(|b| b.reason == Reason::RecoveredCrash)
        .unwrap();
    assert!(
        timeline(&crash.path, "merged.ndjson")
            .iter()
            .any(|row| row["event"]["Input"]["original_us"] == 44)
    );
    restarted.shutdown();
}

#[test]
fn panic_during_startup_keeps_running_marker() {
    let temp = tempfile::tempdir().unwrap();
    let recorder = isolated(temp.path().into(), DeviceId([93; 32]), Some(GroupId([94; 16])), true).unwrap();
    recorder.wait_recovery();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = StartupRecoveryGuard(Some(recorder.clone()));
        panic!("synthetic startup panic");
    }));
    assert!(result.is_err());
    assert!(temp.path().join("running").exists(), "startup panic was marked clean");
    recorder.shutdown();
}

fn key_observation(
    r: &Recorder,
    code: KeyCode,
    pressed: bool,
    repeat: Option<bool>,
    stream: Origin,
    peer: Option<DeviceId>,
) -> SafeInput {
    let mut event = SafeInput::capture(&InputEvent::Key { code, pressed });
    if let SafeInput::Key { repeat: out, .. } = &mut event {
        *out = repeat;
    }
    let _ = r.annotate_key(&mut event, Some(code), stream, peer);
    event
}
fn press_id(event: &SafeInput) -> Option<u64> {
    let SafeInput::Key { diagnostic, .. } = event else {
        panic!("expected key")
    };
    diagnostic.press_id
}
#[test]
fn press_sequence_is_key_independent_and_releases_forget_identity() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([1; 32]), Some(GroupId([2; 16])), true).unwrap();
    let sequence = |code| {
        [
            key_observation(&r, code, true, Some(false), Origin::Physical, None),
            key_observation(&r, code, true, Some(true), Origin::Physical, None),
            key_observation(&r, code, false, Some(false), Origin::Physical, None),
            key_observation(&r, code, false, Some(false), Origin::Physical, None),
        ]
    };
    let first = sequence(KeyCode::keyboard(4));
    let second = sequence(KeyCode::keyboard(5));
    let third = sequence(KeyCode::keyboard(4));
    for (index, sequence) in [&first, &second, &third].into_iter().enumerate() {
        assert_eq!(
            sequence[..3].iter().map(press_id).collect::<Vec<_>>(),
            vec![Some(index as u64 + 1); 3]
        );
        assert_eq!(press_id(&sequence[3]), None);
    }
    let normalized = |mut sequence: [SafeInput; 4]| {
        for event in &mut sequence {
            if let SafeInput::Key { diagnostic, .. } = event {
                diagnostic.press_id = None;
            }
        }
        serde_json::to_vec(&sequence).unwrap()
    };
    assert_eq!(normalized(first), normalized(second));
    assert_eq!(normalized(third), normalized(sequence(KeyCode::keyboard(0x38))));
    r.shutdown();
}
#[test]
fn remote_and_injected_presses_are_partitioned_by_origin_device() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([1; 32]), Some(GroupId([2; 16])), true).unwrap();
    let code = KeyCode::keyboard(4);
    let a = Some(DeviceId([3; 32]));
    let b = Some(DeviceId([4; 32]));
    for stream in [Origin::Remote, Origin::Injection] {
        let down_a = key_observation(&r, code, true, Some(false), stream, a);
        let down_b = key_observation(&r, code, true, Some(false), stream, b);
        assert_ne!(press_id(&down_a), press_id(&down_b));
        let duplicate = key_observation(&r, code, true, Some(false), stream, a);
        assert!(matches!(
            duplicate,
            SafeInput::Key {
                diagnostic: privacy::KeyDiagnostic {
                    pressed_while_held: true,
                    ..
                },
                ..
            }
        ));
        assert_eq!(press_id(&down_a), press_id(&duplicate));
        assert_eq!(
            press_id(&down_a),
            press_id(&key_observation(&r, code, true, Some(true), stream, a))
        );
        assert_eq!(
            press_id(&down_a),
            press_id(&key_observation(&r, code, false, None, stream, a))
        );
        assert_eq!(
            press_id(&down_b),
            press_id(&key_observation(&r, code, false, None, stream, b))
        );
    }
    r.shutdown();
}
#[test]
fn capture_metadata_and_held_table_never_export_key_identity() {
    use oink_input::diagnostics::{CaptureMetadata, RawKeyEvent};
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([1; 32]), Some(GroupId([2; 16])), true).unwrap();
    r.wait_recovery();
    let code = KeyCode::keyboard(0x17);
    let captured = |pressed| oink_foundation::input::CapturedEvent {
        physical: true,
        event: InputEvent::Key { code, pressed },
        source: oink_foundation::input::SourceKind::Keyboard,
        time_us: 123,
        cursor: None,
    };
    let down = CaptureMetadata {
        raw: RawKeyEvent::Down,
        repeat: Some(false),
        ..Default::default()
    };
    r.captured_metadata(&captured(true), None, down);
    r.captured_metadata(
        &captured(true),
        None,
        CaptureMetadata {
            repeat: Some(true),
            ..down
        },
    );
    r.captured_metadata(
        &captured(false),
        None,
        CaptureMetadata {
            backend_generated: Some(true),
            repeat: Some(false),
            ..Default::default()
        },
    );
    r.captured_metadata(
        &captured(true),
        None,
        CaptureMetadata {
            pressed_while_held: true,
            ..down
        },
    );
    // The final held key stays in the in-memory table while all serializable
    // containers (segments, manifests, timelines, snapshots) are exported.
    let probe = key_observation(&r, code, true, Some(true), Origin::Physical, None);
    assert_eq!(press_id(&probe), Some(2));
    r.record(Event::KeyboardAnomaly {
        nonmodifier_flags: true,
    });
    for kind in [StateKind::CapsLock, StateKind::SecureInput] {
        r.record(Event::State {
            kind,
            active: true,
            count: 0,
            value: 0.0,
        });
    }
    let bundle = r.save(Reason::Manual).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let mut merged = String::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).unwrap();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        for forbidden in [
            "458775",
            "0x17",
            "key_code",
            "usage",
            "scan_code",
            "held_table",
            "press_sequence",
            "InputSourceID",
        ] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} leaked in {}",
                entry.name().unwrap()
            );
        }
        if entry.name().unwrap() == "merged.ndjson" {
            merged = text;
        }
    }
    let rows: Vec<serde_json::Value> = merged.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let keys: Vec<_> = rows
        .iter()
        .filter_map(|row| row["event"]["Input"]["event"].get("Key"))
        .collect();
    assert_eq!(keys.len(), 4, "{merged}");
    assert_eq!(
        keys.iter()
            .map(|key| key["diagnostic"]["press_id"].as_u64())
            .collect::<Vec<_>>(),
        vec![Some(1), Some(1), Some(1), Some(2)]
    );
    assert_eq!(keys[0]["diagnostic"]["raw"], "Down");
    assert_eq!(keys[1]["repeat"], true);
    assert_eq!(keys[2]["diagnostic"]["backend_generated"], true);
    assert_eq!(keys[3]["diagnostic"]["pressed_while_held"], true);
    assert!(merged.contains("nonmodifier_flags"));
    for kind in ["CapsLock", "SecureInput"] {
        assert!(merged.contains(kind));
    }
    r.shutdown();
}
#[test]
fn press_state_is_invalidated_by_pause_and_nonblocking_contention() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([1; 32]), Some(GroupId([2; 16])), true).unwrap();
    let key = |pressed| key_observation(&r, KeyCode::keyboard(4), pressed, Some(false), Origin::Physical, None);
    assert_eq!(press_id(&key(true)), Some(1));
    r.set_suspended(true);
    r.set_suspended(false);
    assert_eq!(press_id(&key(false)), None);
    assert_eq!(press_id(&key(true)), Some(2));
    let busy = r.inner.presses.pop().unwrap();
    assert_eq!(press_id(&key(false)), None); // no blocking or raw-key queue fallback
    assert!(r.inner.dropped.load(Ordering::Relaxed) > 0);
    assert!(r.inner.presses.push(busy).is_ok());
    assert_eq!(press_id(&key(false)), None);
    assert_eq!(press_id(&key(true)), Some(3));
    r.shutdown();
}

#[test]
fn remote_projection_and_injection_source_context_survive_forwarding() {
    use oink_routing::{Packet as Message, input::InputMsg};
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([1; 32]), Some(GroupId([2; 16])), true).unwrap();
    r.wait_recovery();
    let relay = DeviceId([3; 32]);
    let origin = DeviceId([4; 32]);
    let code = KeyCode::keyboard(4);
    for pressed in [true, false] {
        let msg = Message::Input(InputMsg::Forwarded {
            origin: boot(origin),
            hops: 1,
            msg: Box::new(InputMsg::Key { code, pressed }),
        });
        r.remote_message(relay, &msg);
        let from = keyboard::message_source(relay, &msg);
        assert_eq!(from, Some(origin));
        keyboard::with_source(from, || {
            assert_eq!(keyboard::source(), Some(origin));
            r.injected(
                &oink_foundation::input::InjectEvent::Key { code, pressed },
                keyboard::source(),
                true,
            );
        });
        assert_eq!(keyboard::source(), None);
    }
    let bundle = r.save(Reason::Manual).unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let mut merged = String::new();
    zip.by_name("merged.ndjson")
        .unwrap()
        .read_to_string(&mut merged)
        .unwrap();
    let rows: Vec<serde_json::Value> = merged.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let input: Vec<_> = rows.iter().filter_map(|row| row["event"].get("Input")).collect();
    assert_eq!(input.len(), 4);
    assert!(input.iter().all(|row| row["peer"] == r.anonymizer().device(origin)));
    let ids: Vec<_> = input
        .iter()
        .map(|row| row["event"]["Key"]["diagnostic"]["press_id"].as_u64())
        .collect();
    assert_eq!(ids, vec![Some(1), Some(2), Some(1), Some(2)]);
    assert!(
        input
            .iter()
            .all(|row| row["event"]["Key"]["diagnostic"]["raw"] == "Unknown")
    );
    assert!(
        input
            .iter()
            .all(|row| row["event"]["Key"]["diagnostic"]["backend_generated"].is_null())
    );
    r.shutdown();
}

#[test]
fn unscoped_injection_cleanup_pairs_only_an_unambiguous_source() {
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([1; 32]), Some(GroupId([2; 16])), true).unwrap();
    let code = KeyCode::keyboard(4);
    let a = Some(DeviceId([3; 32]));
    let b = Some(DeviceId([4; 32]));
    let down = key_observation(&r, code, true, None, Origin::Injection, a);
    let mut up = SafeInput::capture(&InputEvent::Key { code, pressed: false });
    assert_eq!(r.annotate_key(&mut up, Some(code), Origin::Injection, None), a);
    assert_eq!(press_id(&up), press_id(&down));
    key_observation(&r, code, true, None, Origin::Injection, a);
    key_observation(&r, code, true, None, Origin::Injection, b);
    let mut up = SafeInput::capture(&InputEvent::Key { code, pressed: false });
    assert_eq!(r.annotate_key(&mut up, Some(code), Origin::Injection, None), None);
    assert_eq!(press_id(&up), None);
    let next_a = key_observation(&r, code, true, None, Origin::Injection, a);
    let next_b = key_observation(&r, code, true, None, Origin::Injection, b);
    assert_eq!(press_id(&next_a), Some(4));
    assert_eq!(press_id(&next_b), Some(5));
    r.shutdown();
}

#[test]
fn independent_diagnostic_namespace_is_local_and_distinct_from_group_membership() {
    let me = DeviceId([201; 32]);
    let other = DeviceId([202; 32]);
    let independent = Anonymizer::for_membership(me, None);
    assert_ne!(
        independent.device(me),
        Anonymizer::for_membership(other, None).device(me)
    );
    assert_ne!(
        independent.device(me),
        Anonymizer::for_membership(me, Some(GroupId([0; 16]))).device(me)
    );
    let group = Some(GroupId([203; 16]));
    assert_eq!(
        Anonymizer::for_membership(me, group).device(other),
        Anonymizer::for_membership(other, group).device(other)
    );
}

#[test]
fn native_input_lifecycle_roundtrips_numeric_state() {
    use oink_input::native_lifecycle::{Data, Kind};
    let native = Event::NativeInputLifecycle {
        at_us: 9,
        sequence: 12,
        kind: Kind::Mode,
        data: Data {
            extra: u64::MAX,
            ..Data::default()
        },
        holder: Some("anonymized".into()),
        epoch: Some(u64::MAX),
    };
    let bytes = serde_json::to_vec(&native).unwrap();
    let restored: Event = serde_json::from_slice(&bytes).unwrap();
    let json = serde_json::to_string(&restored).unwrap();
    let parsed: Event = serde_json::from_str(&json).unwrap();
    let Event::NativeInputLifecycle { data, epoch, .. } = parsed else {
        panic!("wrong variant")
    };
    assert_eq!(data.extra, u64::MAX);
    assert_eq!(epoch, Some(u64::MAX));
}

#[test]
fn pointer_transition_preserves_fractional_bases_and_native_deltas_in_the_bundle() {
    use oink_foundation::Point;
    use oink_input::pointer_transition::{Observation, Placement, PlacementKind};
    use oink_input::warp_delta::WarpDeltaFix;
    let mut fix = WarpDeltaFix::new();
    let old = Point::new(318.51171875, 595.390625);
    let at = Point::new(4.234375, 552.390625);
    fix.motion((0., 0.), old, 1, true);
    fix.posted_absolute_motion(at, 2);
    let before = fix.snapshot();
    let corrected_delta = fix.motion((0., 0.), at, 3, true);
    let accounting = Observation {
        remote: true,
        placements: 37,
        last_placement: Some(Placement {
            kind: PlacementKind::NativeMotion,
            at,
            delta: (-2., 3.),
            timestamp_ns: 2,
            source_state: 1,
        }),
        event_position: at,
        event_timestamp_ns: 3,
        raw_delta: (0., 0.),
        corrected_delta,
        before,
        after: fix.snapshot(),
        ..Default::default()
    };
    let event = Event::PointerTransition {
        sequence: 4,
        holder: Some("anonymized-holder".into()),
        epoch: Some(10),
        accounting,
    };
    let bytes = serde_json::to_vec(&event).unwrap();
    let restored: Event = serde_json::from_slice(&bytes).unwrap();
    let Event::PointerTransition {
        accounting: actual,
        holder,
        epoch,
        ..
    } = restored
    else {
        panic!("wrong event variant");
    };
    assert_eq!(actual, accounting);
    assert_eq!(actual.corrected_delta, (314.27734375, 43.));
    assert_eq!(holder.as_deref(), Some("anonymized-holder"));
    assert_eq!(epoch, Some(10));
}

#[test]
fn transfer_local_target_is_excluded_and_bundle_projection_excludes_file_identity() {
    use tracing_subscriber::layer::SubscriberExt;
    let temp = tempfile::tempdir().unwrap();
    let r = isolated(temp.path().into(), DeviceId([81; 32]), Some(GroupId([82; 16])), true).unwrap();
    r.wait_recovery();
    let subscriber = tracing_subscriber::registry().with(trace::scoped(&r));
    tracing::subscriber::with_default(subscriber, || {
        // Even allowed scalars and WARN/ERROR cannot get a local-only line in.
        tracing::info!(target: "oink_local_transfer", bytes=987654321_u64, duration_ms=99.0, path="relayed", "private transfer start");
        tracing::debug!(target: "oink_local_transfer", bytes=987654321_u64, "private progress");
        tracing::warn!(target: "oink_local_transfer", count=991_u64, "private warning");
        tracing::error!(target: "oink_local_transfer", count=992_u64, "private error");
        tracing::info!(target: "oink_transfer_diagnostics", kind="transfer_start", transfer_id=17_u64, role="sender", result="pending", duration_ms=0.0, total_bytes=300_u64, file_count=1_u64, network_path="relayed", channel="low_tcp", path="/Users/private/secret-name.bin", filename="secret-name.bin", "ignored free text");
        tracing::info!(target: "oink_transfer_diagnostics", kind="transfer_progress", transfer_id=17_u64, role="sender", bytes=200_u64, duration_ms=20.0, network_path="relayed", channel="low_tcp", "observation");
        tracing::info!(target: "oink_transfer_diagnostics", kind="transfer_end", transfer_id=17_u64, role="sender", result="completed", duration_ms=42.0, bytes=300_u64, "boundary");
        tracing::info!(target: "oink_pacing_diagnostics", kind="pacing_state", limited=true, local_input=true, group_input=false, audio=false, probe_timeout=true, rate_bytes_per_second=987654321_u64, "state");
        tracing::info!(target: "oink_pacing_diagnostics", kind="pacing_sample", limited=true, rtt_ms=Some(12.0), target_ms=10.0, path="/Users/private/secret-name.bin", filename="secret-name.bin", "sample");
        tracing::info!(target: "oink_transfer_diagnostics", kind="transfer_start", transfer_id=18_u64, role="receiver", network_path="/Users/private/secret-name.bin", channel="secret-name.bin", path="/Users/private/secret-name.bin", filename="secret-name.bin", private_debug=?std::path::Path::new("/Users/private/secret-name.bin"), "must ignore {}", "/Users/private/secret-name.bin");
    });
    r.record(Event::Clipboard {
        kind_mask: 1,
        bytes: 4321,
        outcome: 0,
    });
    r.record(Event::Files {
        files: 1,
        files_done: 1,
        bytes: 5432,
        bytes_done: 5432,
        phase: 2,
    });
    let bundle = r.save(Reason::Manual).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    let logs: Vec<_> = rows.iter().filter_map(|row| row["event"].get("Log")).collect();
    assert_eq!(logs.len(), 6);
    assert!(
        logs.iter()
            .all(|l| l["module"].as_str().unwrap().ends_with("_diagnostics"))
    );
    let text = serde_json::to_string(&rows).unwrap();
    for forbidden in [
        "987654321",
        "/Users/private",
        "secret-name.bin",
        "rate_bytes_per_second",
        "private transfer",
    ] {
        assert!(!text.contains(forbidden), "leaked {forbidden}");
    }
    assert!(text.contains("relayed"));
    assert!(text.contains("low_tcp"));
    assert!(text.contains("total_bytes"));
    assert!(text.contains("4321"));
    assert!(text.contains("5432"));
    assert!(text.contains("300"));
    let labels = &logs[2]["labels"];
    assert!(
        labels
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == &serde_json::json!(["result", "completed"]))
    );
    let fields = &logs[4]["fields"];
    assert!(
        fields
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == &serde_json::json!(["rtt_ms", 12.0]))
    );
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    for i in 0..zip.len() {
        let mut text = String::new();
        zip.by_index(i).unwrap().read_to_string(&mut text).unwrap();
        assert!(!text.contains("/Users/private"));
        assert!(!text.contains("secret-name.bin"));
    }
    r.shutdown();
}

#[test]
fn recorder_control_changes_notify_without_a_statistics_tick() {
    let temp = tempfile::tempdir().unwrap();
    let recorder = Recorder::start(temp.path().into(), DeviceId([91; 32]), None, true).unwrap();
    recorder.wait_recovery();
    let mut changes = recorder.status_changes();
    let at = Instant::now();
    recorder.set_suspended(true);
    assert!(changes.has_changed().unwrap());
    assert!(recorder.status().suspended);
    changes.borrow_and_update();
    recorder.set_suspended(true);
    assert!(!changes.has_changed().unwrap(), "unchanged value must not notify");
    recorder.set_enabled(false).unwrap();
    assert!(changes.has_changed().unwrap());
    assert!(!recorder.status().configured_enabled);
    changes.borrow_and_update();
    recorder.set_enabled(false).unwrap();
    assert!(!changes.has_changed().unwrap());
    eprintln!(
        "UI_LATENCY recorder_control_source_ms={:.3}",
        at.elapsed().as_secs_f64() * 1000.
    );
    recorder.shutdown();
}

#[test]
fn size_only_store_keeps_old_rows_and_saves_them() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = store(temp.path());
    let mut old = motion(42);
    old.utc_us = utc_us() - 24 * 60 * 60 * 1_000_000;
    s.append(vec![old], 0).unwrap();
    assert_eq!(s.segments.len(), 1);
    let b = s.export(Reason::Manual, 0, 0).unwrap();
    assert_eq!(timeline(&b.path, "merged.ndjson").len(), 1);
}

// Fixture inspection only: no product API, folder watcher or summary cache.
pub(super) fn bundle_summary(path: &std::path::Path) -> Option<(usize, usize, Option<bool>)> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let value: serde_json::Value = serde_json::from_reader(archive.by_name("manifest.json").ok()?).ok()?;
    let included = value["coverage"]
        .as_array()?
        .iter()
        .filter_map(|v| v["device"].as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    Some((
        included,
        value["devices"].as_object()?.len() - included,
        value["cutoff_complete"].as_bool(),
    ))
}

#[test]
fn provider_availability_is_change_driven_and_survives_a_later_package_snapshot() {
    use oink_clipboard::FileProviderReason as Fp;
    let dir = tempfile::tempdir().unwrap();
    let r = isolated(dir.path().to_path_buf(), DeviceId([1; 32]), None, true).unwrap();
    r.wait_recovery();
    r.snapshot(Snapshot {
        version: "fixture".into(),
        ..Default::default()
    });
    // A concurrent periodic snapshot can observe the new reason before its
    // change callback. That must not swallow the typed availability event.
    r.snapshot(Snapshot {
        version: "sampled".into(),
        file_provider: Some(Fp::DomainDisabled.into()),
        ..Default::default()
    });
    r.file_provider_changed(Fp::DomainDisabled);
    r.file_provider_changed(Fp::UrlsReady);
    r.file_provider_changed(Fp::BoundExpired);
    r.snapshot(Snapshot {
        version: "later".into(),
        ..Default::default()
    });
    let bundle = r.save(Reason::Manual).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    let availability: Vec<_> = rows
        .iter()
        .filter_map(|row| row["event"].get("FileProviderAvailability"))
        .collect();
    assert_eq!(availability.len(), 3);
    assert_eq!(availability[0]["reason"], "domain_disabled");
    assert_eq!(availability[0]["available"], false);
    let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
    let mut environments = 0;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).unwrap();
        if file.name().unwrap().ends_with("/environment.json") {
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["file_provider"]["reason"], "bound_expired");
            environments += 1;
        }
    }
    assert_eq!(environments, 1);
    r.shutdown();
}

#[test]
fn provider_snapshot_roundtrips_the_typed_scalar() {
    use oink_clipboard::FileProviderReason as Fp;
    let mut snapshot = Snapshot::default();
    let before = serde_json::to_vec(&snapshot).unwrap();
    snapshot.file_provider = Some(Fp::DomainDisabled.into());
    assert_eq!(serde_json::to_vec(&snapshot).unwrap(), before);
    snapshot.routing_numbers.insert(
        "file_provider_reason".into(),
        ProviderAvailability::from(Fp::DomainDisabled).code(),
    );
    let decoded: Snapshot = serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert!(decoded.file_provider.is_none());
    assert_eq!(
        ProviderAvailability::from_code(decoded.routing_numbers["file_provider_reason"]),
        Some(Fp::DomainDisabled.into())
    );
}

#[test]
fn provider_preparation_exports_numeric_boundaries() {
    use oink_clipboard::{
        FileProviderReason,
        provider_preparation::{PlaceholderFallback, PlaceholderTiming, PreparationPath, PreparationTiming},
    };
    let temp = tempfile::tempdir().unwrap();
    let recorder = isolated(temp.path().into(), DeviceId([71; 32]), Some(GroupId([72; 16])), true).unwrap();
    recorder.wait_recovery();
    let drag = recorder.anonymizer().control_identity(b"private-drag-identity");
    let event = Event::FileProviderPreparation {
        drag: drag.clone(),
        epoch: 19,
        path: PreparationPath::Placeholder,
        reason: FileProviderReason::UrlsReady,
        timing: PreparationTiming {
            spare_ready: true,
            spare_preparation_us: Some(900),
            parent_visible_us: Some(0),
            children_enumerated_us: Some(20),
            first_url_us: Some(30),
            registration_us: 40,
            native_begin_us: Some(50),
            placeholder: PlaceholderTiming {
                enabled: true,
                used: true,
                prepared_files: 5,
                prepared_folders: 5,
                used_files: 2,
                preparation_us: Some(900),
                final_urls_us: Some(30),
                fallback: PlaceholderFallback::None,
                ..Default::default()
            },
            ..Default::default()
        },
        items: 2,
        bytes: 4096,
    };
    let bytes = serde_json::to_vec(&event).unwrap();
    let restored: Event = serde_json::from_slice(&bytes).unwrap();
    recorder.record(restored);
    let bundle = recorder.save(Reason::Manual).unwrap();
    let rows = timeline(&bundle.path, "merged.ndjson");
    let rows: Vec<_> = rows
        .iter()
        .filter_map(|row| row["event"].get("FileProviderPreparation"))
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["drag"], drag);
    assert_eq!(rows[0]["timing"]["first_url_us"], 30);
    assert_eq!(rows[0]["path"], "Placeholder");
    assert_eq!(rows[0]["timing"]["placeholder"]["used"], true);
    assert_eq!(rows[0]["timing"]["placeholder"]["prepared_files"], 5);
    assert_eq!(rows[0]["timing"]["placeholder"]["final_urls_us"], 30);
    assert!(!serde_json::to_string(&rows).unwrap().contains("private-drag-identity"));
    recorder.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostic_preference_commit_precedes_apply_and_failed_marker_can_be_retried() {
    use crate::{
        api::AgentApi,
        node::{Node, NodeOptions},
    };
    let dir = tempfile::tempdir().unwrap();
    let node = Node::start(NodeOptions {
        net: oink_test_support::network_config(dir.path().into(), "synthetic", 0),
        input: Arc::new(oink_test_support::input::FakeInput::new()),
        displays: vec![],
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: false,
    })
    .await
    .unwrap();
    let recorder = &node.inner.diagnostics.get().unwrap().recorder;
    recorder.wait_recovery();
    let settings = dir.path().join(crate::config::SETTINGS_FILE);
    let backup = dir.path().join("saved-settings");
    std::fs::rename(&settings, &backup).unwrap();
    std::fs::create_dir(&settings).unwrap();
    assert!(node.set_diagnostics_enabled(false).is_err());
    assert!(recorder.enabled());
    assert!(recorder.status().configured_enabled);
    std::fs::remove_dir(&settings).unwrap();
    std::fs::rename(backup, settings).unwrap();
    node.set_diagnostics_enabled(false).unwrap();
    assert!(!recorder.enabled());
    let marker = PathBuf::from(recorder.folder().unwrap()).join("running");
    std::fs::create_dir(&marker).unwrap();
    assert!(node.set_diagnostics_enabled(true).is_err());
    assert!(node.settings().diagnostics_enabled);
    assert!(recorder.status().configured_enabled);
    assert!(!recorder.status().enabled);
    std::fs::remove_dir(&marker).unwrap();
    node.set_diagnostics_enabled(true).unwrap();
    assert!(recorder.enabled());
    node.set_diagnostics_enabled(false).unwrap();
    std::fs::create_dir(&marker).unwrap();
    assert!(node.set_diagnostics_enabled(false).is_err());
    std::fs::remove_dir(&marker).unwrap();
    node.set_diagnostics_enabled(false).unwrap();
    assert!(!recorder.status().configured_enabled);
    node.shutdown().await;
}

#[test]
fn baseline_collection_omits_rich_layout_and_keeps_typed_availability() {
    use oink_protocol::v1::{dto as w, schema::WireDto};
    let layout = empty_layout_for_export(1);
    let snapshot = Snapshot {
        group_state: Some("{}".into()),
        layout: Some(layout.clone()),
        ..Snapshot::current_build()
    };
    let baseline = super::wire_values::snapshot(&snapshot, false).unwrap();
    assert!(!baseline.raw().contains_key(&16));
    assert!(baseline.layout().unwrap().is_none());
    assert!(baseline.group_state().unwrap().layout_available().unwrap());
    assert_eq!(
        baseline.group_state().unwrap().omitted_details().unwrap().0[0].0,
        "oink.diagnostics.layout/1"
    );
    assert!(
        super::wire_values::snapshot(&snapshot, true)
            .unwrap()
            .layout()
            .unwrap()
            .is_some()
    );
    let event = Event::Topology(Box::new(layout::LayoutChange {
        kinds: vec![layout::ChangeKind::Initialized],
        before: None,
        after: layout,
    }));
    let base = event.network(false).unwrap();
    assert_eq!(base.kind().unwrap().0, "topology/1");
    assert!(
        !w::DiagTopologySummary::from_raw(base.fields().unwrap())
            .unwrap()
            .detail_available()
            .unwrap()
    );
    assert_eq!(event.network(true).unwrap().kind().unwrap().0, "layout-change/1");

    // Building the owning segment validates raw child rows recursively and
    // preserves the same rich/baseline projection as standalone typed events.
    let mut segment = store::Segment {
        schema: SCHEMA,
        device: "a".repeat(24),
        instance: "b".repeat(24),
        seq: 1,
        dropped: 0,
        snapshot,
        clock_samples: vec![],
        rows: vec![Row {
            mono_us: 1,
            clock_id: 2,
            utc_us: 3,
            event,
        }],
    };
    for rich in [false, true] {
        let projected = super::wire_values::segment(&segment, rich).unwrap();
        assert_eq!(
            projected.rows().unwrap().0[0].event().unwrap(),
            segment.rows[0].event.network(rich).unwrap()
        );
        assert_eq!(projected.snapshot().unwrap().layout().unwrap().is_some(), rich);
        let bytes = projected.encode(oink_protocol::v1::limits::DIAGNOSTIC_SEGMENT).unwrap();
        assert_eq!(w::DiagnosticSegment::decode(&bytes, bytes.len()).unwrap(), projected);
    }
    segment.rows[0].event = Event::State {
        kind: StateKind::RecorderTick,
        active: true,
        count: 1,
        value: f64::NAN,
    };
    assert!(super::wire_values::segment(&segment, true).is_err());
}
