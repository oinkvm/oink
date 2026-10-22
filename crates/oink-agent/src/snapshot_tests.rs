//! Real QUIC/TLS and durable group stores with fake OS input/display/clipboard.
//! These tests exercise the product dispatcher; they are not desktop acceptance.
use crate::api::AgentApi;
use crate::node::{Node, NodeOptions, lock};
use oink_display::{
    ChangeHandler, DisplayBackend, DisplayError,
    wallpaper::{DisplayWallpaper, Thumbnail},
};
use oink_foundation::{DeviceId, Rect, Size, display::DisplayInfo};
use oink_network::{
    NetworkConfig,
    link::{LinkId, StreamId},
};
use oink_test_support::input::FakeInput;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) struct Displays {
    info: Mutex<DisplayInfo>,
    fail: std::sync::atomic::AtomicBool,
    changes: Mutex<Vec<ChangeHandler>>,
    wallpaper_changes: Mutex<Vec<ChangeHandler>>,
}
impl Displays {
    pub(crate) fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            info: Mutex::new(DisplayInfo {
                key: name.into(),
                name: name.into(),
                bounds: Rect::new(0.0, 0.0, 1728.0, 1117.0),
                pixel_width: 3456,
                pixel_height: 2234,
                reference_extent: Some(Size::new(345.0 / 686.0, 223.0 / 686.0)),
                rotation_deg: 0,
                is_primary: true,
                is_builtin: true,
                is_virtual: false,
            }),
            fail: std::sync::atomic::AtomicBool::new(false),
            changes: Mutex::new(Vec::new()),
            wallpaper_changes: Mutex::new(Vec::new()),
        })
    }
    pub(crate) fn info(&self) -> DisplayInfo {
        lock(&self.info).clone()
    }
    fn change_wallpaper(&self) {
        lock(&self.info).pixel_width += 2;
        for callback in lock(&self.wallpaper_changes).clone() {
            callback();
        }
    }
    pub(crate) fn change_pixels(&self) {
        lock(&self.info).pixel_width += 2;
        for callback in lock(&self.changes).clone() {
            callback();
        }
    }
    fn fail_enumeration(&self, fail: bool) {
        self.fail.store(fail, std::sync::atomic::Ordering::Release);
        for callback in lock(&self.changes).clone() {
            callback();
        }
    }
}
impl DisplayBackend for Displays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        if self.fail.load(std::sync::atomic::Ordering::Acquire) {
            return Err(DisplayError::Platform("fake completed enumeration failure".into()));
        }
        Ok(vec![self.info()])
    }
    fn watch(&self, callback: ChangeHandler) -> Result<(), DisplayError> {
        lock(&self.changes).push(callback);
        Ok(())
    }
    fn watch_wallpaper(&self, callback: ChangeHandler) -> Result<(), DisplayError> {
        lock(&self.wallpaper_changes).push(callback);
        Ok(())
    }
    fn wallpapers(&self) -> Option<Vec<DisplayWallpaper>> {
        let info = self.info();
        let mut jpeg = vec![0xff, 0xd8];
        jpeg.resize(64, (info.pixel_width % 255) as u8);
        Some(vec![DisplayWallpaper {
            key: info.key,
            thumbnail: Thumbnail {
                width: 48,
                height: 30,
                jpeg,
            },
        }])
    }
}

pub(crate) async fn start_with_clip(
    config: &NetworkConfig,
    display: &Arc<Displays>,
    clipboard: Option<Arc<dyn oink_clipboard::ClipboardBackend>>,
) -> Node {
    Node::start(NodeOptions {
        net: config.clone(),
        input: Arc::new(FakeInput::new()),
        displays: vec![],
        display_backend: Some(display.clone()),
        clipboard,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap()
}

use oink_test_support::clipboard::FakeClipboard as Clipboard;

#[test]
fn display_alerts_follow_native_failure_and_recovery_without_connect_flashes() {
    crate::test_watchdog::run(module_path!(), "display_alert_native_outcomes", Some(4), async {
        use crate::api::{AlertKind, ApiEvent};
        let dir = tempfile::tempdir().unwrap();
        let displays = [Displays::new("display-a"), Displays::new("display-b")];
        let a = start_with_clip(
            &oink_test_support::network_config(dir.path().join("a"), "a", 0),
            &displays[0],
            None,
        )
        .await;
        let b = start_with_clip(
            &oink_test_support::network_config(dir.path().join("b"), "b", 0),
            &displays[1],
            None,
        )
        .await;
        let seen: Arc<Mutex<Vec<crate::api::Alert>>> = Arc::default();
        for node in [&a, &b] {
            let seen = seen.clone();
            node.subscribe(Arc::new(move |event| {
                if let ApiEvent::Overview(view) = event {
                    lock(&seen).extend(
                        view.alerts
                            .iter()
                            .filter(|alert| {
                                matches!(
                                    alert.kind,
                                    AlertKind::DisplayInfoUnavailable { .. }
                                        | AlertKind::DisplayEnumerationFailed { .. }
                                )
                            })
                            .cloned(),
                    );
                }
            }));
        }
        pair(&a, &b).await;
        active(&a, &b).await;
        assert!(
            lock(&seen).is_empty(),
            "normal initial Facts must not manufacture display errors"
        );
        let alerts = |node: &Node| {
            node.overview()
                .unwrap()
                .alerts
                .into_iter()
                .filter(|alert| {
                    matches!(
                        alert.kind, AlertKind::DisplayEnumerationFailed { device } if device == a.id().into()
                    )
                })
                .count()
        };
        displays[0].fail_enumeration(true);
        until("explicit enumeration failure reaches local and peer views", || {
            alerts(&a) == 1 && alerts(&b) == 1
        })
        .await;
        assert!(
            has_display(&a, a.id(), &displays[0].info()),
            "retain last good local geometry"
        );
        assert!(
            has_display(&b, a.id(), &displays[0].info()),
            "retain last good peer geometry"
        );
        displays[0].fail_enumeration(false);
        until("successful enumeration clears local and peer errors", || {
            alerts(&a) == 0 && alerts(&b) == 0
        })
        .await;
        a.shutdown().await;
        b.shutdown().await;
    });
}

use crate as test_agent;
#[path = "../tests/common/mod.rs"]
pub(crate) mod common;
pub(crate) use common::{pair, until};
pub(crate) async fn active(a: &Node, b: &Node) {
    common::active(&[a, b]).await;
}
fn has_display(n: &Node, peer: DeviceId, expected: &DisplayInfo) -> bool {
    n.device_facts(peer)
        .and_then(|facts| facts.displays().ok())
        .is_some_and(|displays| {
            displays
                .0
                .iter()
                .filter_map(crate::agent_api::display_fact)
                .any(|display| {
                    display.key == expected.key
                        && display.pixel_width == expected.pixel_width
                        && display.bounds == expected.bounds
                })
        })
}
async fn pause(node: &Node, paused: bool) {
    node.set_local_paused(crate::device_controls::runtime::fresh_request_id(), paused)
        .unwrap();
    until("local pause owner applied", || {
        let status = node.pause_status();
        status.paused == paused && !status.settling
    })
    .await;
}
async fn input_preferences(node: &Node, settings: crate::api::InputSettings) {
    let node = node.clone();
    tokio::task::spawn_blocking(move || node.set_input_settings(settings.into()))
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn late_subscriber_receives_current_group_without_a_state_change() {
    crate::test_watchdog::run(module_path!(), "late group subscription", Some(2), async {
        let dir = tempfile::tempdir().unwrap();
        let node = start_with_clip(
            &oink_test_support::network_config(dir.path().into(), "late", 0),
            &Displays::new("late"),
            None,
        )
        .await;
        until("platform startup", || {
            node.inner.platform_ready.load(std::sync::atomic::Ordering::Acquire)
        })
        .await;
        let first = node.subscribe_views(Arc::new(|_| {}), crate::api::LiveViews::default());
        node.unsubscribe(first);
        let (send, receive) = std::sync::mpsc::channel();
        let second = node.subscribe_views(
            Arc::new(move |event| {
                if let crate::api::ApiEvent::Group(group) = event {
                    let _ = send.send(group.clone());
                }
            }),
            crate::api::LiveViews::default(),
        );
        let initial = receive.try_recv();
        node.unsubscribe(second);
        node.shutdown().await;
        assert!(
            initial.is_ok(),
            "late subscriber needs an initial group even when the global comparator is primed"
        );
    });
}

#[test]
fn pairing_applies_group_state_before_input_and_keeps_facts_separate() {
    crate::test_watchdog::run(
        module_path!(),
        "pairing_applies_group_state_before_input_and_keeps_facts_separate",
        Some(4),
        pairing_applies_group_state_before_input_and_keeps_facts_separate_scenario(),
    );
}

async fn pairing_applies_group_state_before_input_and_keeps_facts_separate_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let displays = [Displays::new("a"), Displays::new("b")];
    let a = start_with_clip(
        &oink_test_support::network_config(dir.path().join("a"), "a", 0),
        &displays[0],
        None,
    )
    .await;
    let b = start_with_clip(
        &oink_test_support::network_config(dir.path().join("b"), "b", 0),
        &displays[1],
        None,
    )
    .await;
    assert!(a.group_id().is_some());
    assert!(b.group_id().is_some());
    pair(&a, &b).await;
    active(&a, &b).await;
    until("both owner facts applied after pairing", || {
        let state = a.link_config().unwrap().applied();
        state.owners.len() == 2 && state == b.link_config().unwrap().applied()
    })
    .await;
    assert_eq!(a.link_config().unwrap().applied(), b.link_config().unwrap().applied());
    assert!(input_id(&a, b.id()).is_some());
    until("initial display facts", || {
        has_display(&a, b.id(), &displays[1].info()) && has_display(&b, a.id(), &displays[0].info())
    })
    .await;
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn pause_keeps_link_and_facts_but_replaces_input_streams_on_resume() {
    crate::test_watchdog::run(
        module_path!(),
        "pause_keeps_link_and_facts_but_replaces_input_streams_on_resume",
        Some(4),
        pause_keeps_link_and_facts_but_replaces_input_streams_on_resume_scenario(),
    );
}

async fn pause_keeps_link_and_facts_but_replaces_input_streams_on_resume_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let displays = [Displays::new("a"), Displays::new("b")];
    let a = start_with_clip(
        &oink_test_support::network_config(dir.path().join("a"), "a", 0),
        &displays[0],
        None,
    )
    .await;
    let b = start_with_clip(
        &oink_test_support::network_config(dir.path().join("b"), "b", 0),
        &displays[1],
        None,
    )
    .await;
    pair(&a, &b).await;
    active(&a, &b).await;
    let old = input_id(&a, b.id()).unwrap();
    pause(&a, true).await;
    until("pause applied everywhere", || {
        a.fully_paused() && !b.fully_paused() && !a.pause_status().settling
    })
    .await;
    assert!(link_id(&a, b.id()).is_some());
    assert!(input_id(&a, b.id()).is_none());
    displays[0].change_pixels();
    until("paused display facts still arrive", || {
        has_display(&b, a.id(), &displays[0].info())
    })
    .await;
    pause(&a, false).await;
    active(&a, &b).await;
    assert_ne!(input_id(&a, b.id()).unwrap(), old);
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn restart_adopts_resident_layout_and_current_displays() {
    crate::test_watchdog::run(
        module_path!(),
        "restart_adopts_resident_layout_and_current_displays",
        Some(4),
        restart_adopts_resident_layout_and_current_displays_scenario(),
    );
}

async fn restart_adopts_resident_layout_and_current_displays_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let configs = [
        oink_test_support::network_config(dir.path().join("a"), "a", 0),
        oink_test_support::network_config(dir.path().join("b"), "b", 0),
    ];
    let displays = [Displays::new("a"), Displays::new("b")];
    let a = start_with_clip(&configs[0], &displays[0], None).await;
    let b = start_with_clip(&configs[1], &displays[1], None).await;
    pair(&a, &b).await;
    active(&a, &b).await;
    until("both display updates applied before restart baseline", || {
        a.layout().0.screens.len() == 2 && a.layout() == b.layout()
    })
    .await;
    let old_boot = b.boot();
    b.shutdown().await;
    a.link_config()
        .unwrap()
        .edit_settings(Box::new(|settings| settings.gap_travel = !settings.gap_travel))
        .unwrap();
    let expected = a.layout().0;
    displays[1].change_pixels();
    let b = start_with_clip(&configs[1], &displays[1], None).await;
    assert_ne!(b.boot(), old_boot);
    active(&a, &b).await;
    until("resident layout adopted", || {
        a.layout().0 == expected && b.layout().0 == expected
    })
    .await;
    until("restarted facts", || has_display(&a, b.id(), &displays[1].info())).await;
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn restart_one_of_three_preserves_resident_input_stream() {
    crate::test_watchdog::run(
        module_path!(),
        "restart_one_of_three_preserves_resident_input_stream",
        Some(4),
        restart_one_of_three_preserves_resident_input_stream_scenario(),
    );
}

async fn restart_one_of_three_preserves_resident_input_stream_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ["a", "b", "c"].map(|name| oink_test_support::network_config(dir.path().join(name), name, 0));
    let displays = [Displays::new("a"), Displays::new("b"), Displays::new("c")];
    let a = start_with_clip(&configs[0], &displays[0], None).await;
    let b = start_with_clip(&configs[1], &displays[1], None).await;
    let c = start_with_clip(&configs[2], &displays[2], None).await;
    pair(&a, &b).await;
    pair(&a, &c).await;
    common::active(&[&a, &b, &c]).await;
    until("three display updates applied before restart baseline", || {
        let state = a.link_config().unwrap().applied();
        state.layout.screens.len() == 3
            && state == b.link_config().unwrap().applied()
            && state == c.link_config().unwrap().applied()
    })
    .await;
    let resident = input_id(&a, b.id()).unwrap();
    let old_boot = c.boot();
    let expected = a.link_config().unwrap().applied();
    c.shutdown().await;
    let c = start_with_clip(&configs[2], &displays[2], None).await;
    assert_ne!(c.boot(), old_boot);
    common::active(&[&a, &b, &c]).await;
    assert_eq!(input_id(&a, b.id()).unwrap(), resident);
    until("resident owner facts restored after restart", || {
        c.link_config().unwrap().applied() == expected
    })
    .await;
    assert_eq!(input_id(&a, b.id()).unwrap(), resident);
    assert_eq!(c.link_config().unwrap().applied(), expected);
    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

#[test]
fn restart_both_selects_one_seed_and_rejoins_without_pairing() {
    crate::test_watchdog::run(
        module_path!(),
        "restart_both_selects_one_seed_and_rejoins_without_pairing",
        Some(4),
        restart_both_selects_one_seed_and_rejoins_without_pairing_scenario(),
    );
}

async fn restart_both_selects_one_seed_and_rejoins_without_pairing_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let configs = ["a", "b"].map(|name| oink_test_support::network_config(dir.path().join(name), name, 0));
    let displays = [Displays::new("a"), Displays::new("b")];
    let a = start_with_clip(&configs[0], &displays[0], None).await;
    let b = start_with_clip(&configs[1], &displays[1], None).await;
    pair(&a, &b).await;
    until("both owner facts applied before saving restart baseline", || {
        let state = a.link_config().unwrap().applied();
        state.layout.screens.len() == 2 && state.owners.len() == 2 && state == b.link_config().unwrap().applied()
    })
    .await;
    let boots = [a.boot(), b.boot()];
    let expected = a.link_config().unwrap().applied();
    a.shutdown().await;
    b.shutdown().await;
    let (a, b) = tokio::join!(
        start_with_clip(&configs[0], &displays[0], None),
        start_with_clip(&configs[1], &displays[1], None)
    );
    assert_ne!(a.boot(), boots[0]);
    assert_ne!(b.boot(), boots[1]);
    active(&a, &b).await;
    until("both owner facts restored after restart", || {
        a.link_config().unwrap().applied() == expected && b.link_config().unwrap().applied() == expected
    })
    .await;
    assert_eq!(a.link_config().unwrap().applied(), expected);
    assert_eq!(b.link_config().unwrap().applied(), expected);
    assert!(a.pairings().unwrap().sessions.is_empty());
    assert!(b.pairings().unwrap().sessions.is_empty());
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn replacement_stream_receives_current_large_clipboard_offer() {
    crate::test_watchdog::run(
        module_path!(),
        "replacement_stream_receives_current_large_clipboard_offer",
        Some(4),
        replacement_stream_receives_current_large_clipboard_offer_scenario(),
    );
}

async fn replacement_stream_receives_current_large_clipboard_offer_scenario() {
    for source_is_coordinator in [true, false] {
        replacement_clipboard_from_either_session_role(source_is_coordinator).await;
    }
}

async fn replacement_clipboard_from_either_session_role(source_is_coordinator: bool) {
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(Clipboard::new());
    let target = Arc::new(Clipboard::new());
    let a = start_with_clip(
        &oink_test_support::network_config(dir.path().join("a"), "a", 0),
        &Displays::new("a"),
        Some(source.clone()),
    )
    .await;
    let b = start_with_clip(
        &oink_test_support::network_config(dir.path().join("b"), "b", 0),
        &Displays::new("b"),
        Some(target.clone()),
    )
    .await;
    let (a, b, source, target) = if (a.id() < b.id()) == source_is_coordinator {
        (a, b, source, target)
    } else {
        (b, a, target, source)
    };
    eprintln!("replacement clipboard source_is_coordinator={source_is_coordinator}");
    pair(&a, &b).await;
    active(&a, &b).await;
    let old = input_id(&a, b.id()).unwrap();
    pause(&a, true).await;
    until("input streams withdrawn", || input_id(&a, b.id()).is_none()).await;
    let expected: Vec<_> = (0..600)
        .map(|i| oink_clipboard::ClipData::Text(format!("value-{i}")))
        .collect();
    target.set_deferred_native(true);
    // Known small sizes automatically prefetch all 600 representations. This
    // scenario checks manifest replacement and two demand reads, not throughput.
    source.set_unknown_representation_sizes(true);
    source.copy(expected.clone(), oink_clipboard::Sensitivity::Normal);
    pause(&a, false).await;
    until("new session after explicit owner revoke", || {
        input_id(&a, b.id()).is_some_and(|id| id != old)
    })
    .await;
    until("complete current clipboard inventory", || {
        target
            .native_spec()
            .is_some_and(|spec| spec.inventory.items.len() == expected.len())
    })
    .await;
    let spec = target.native_spec().unwrap();
    let descriptors = &spec.inventory.items;
    assert_eq!(
        descriptors.iter().map(|item| item.id.0).collect::<Vec<_>>(),
        (1..=600).collect::<Vec<_>>()
    );
    // Assembly validates every descriptor; selected lazy reads validate the
    // payload contract without turning a manifest test into a throughput run.
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        for index in [0, 1] {
            let mut bytes = Vec::new();
            spec.contents
                .open(spec.inventory.items[index].representations[0].id)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, format!("value-{index}").as_bytes());
        }
    })
    .await
    .unwrap();
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn independent_pause_status_reads_do_not_report_a_resume() {
    crate::test_watchdog::run(
        module_path!(),
        "independent_pause_status_reads_do_not_report_a_resume",
        Some(2),
        independent_pause_status_reads_do_not_report_a_resume_scenario(),
    );
}

async fn independent_pause_status_reads_do_not_report_a_resume_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let a = start_with_clip(
        &oink_test_support::network_config(dir.path().into(), "a", 0),
        &Displays::new("a"),
        None,
    )
    .await;
    pause(&a, true).await;
    until("local pause settled", || {
        a.pause_status().paused && !a.pause_status().settling
    })
    .await;
    for _ in 0..100 {
        let status = a.pause_status();
        assert!(status.paused && !status.settling);
    }
    a.shutdown().await;
}

#[test]
fn runtime_shutdown_cancels_observers_after_unwind() {
    let (done, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let _node = start_with_clip(
                    &oink_test_support::network_config(dir.path().into(), "panic", 0),
                    &Displays::new("panic"),
                    None,
                )
                .await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                panic!("intentional fixture unwind");
            });
        });
        done.send(result.is_err()).unwrap();
    });
    assert!(
        ready
            .recv_timeout(Duration::from_secs(5))
            .expect("runtime must not join an immortal blocking observer")
    );
}

#[cfg(windows)]
#[test]
fn firewall_view_receives_inbound_low_proof_and_last_disconnect() {
    crate::test_watchdog::run(
        module_path!(),
        "firewall_view_receives_inbound_low_proof_and_last_disconnect",
        Some(4),
        firewall_view_receives_inbound_low_proof_and_last_disconnect_scenario(),
    );
}

#[cfg(windows)]
async fn firewall_view_receives_inbound_low_proof_and_last_disconnect_scenario() {
    use crate::api::{ApiEvent, PermissionKind};
    use oink_foundation::input::PermissionState;
    let dir = tempfile::tempdir().unwrap();
    let a = start_with_clip(
        &oink_test_support::network_config(dir.path().join("a"), "a", 0),
        &Displays::new("a"),
        None,
    )
    .await;
    let b = start_with_clip(
        &oink_test_support::network_config(dir.path().join("b"), "b", 0),
        &Displays::new("b"),
        None,
    )
    .await;
    let events: [Arc<Mutex<Vec<PermissionState>>>; 2] = std::array::from_fn(|_| Arc::default());
    for (node, observed) in [&a, &b].into_iter().zip(&events) {
        let observed = observed.clone();
        node.subscribe(Arc::new(move |event| {
            if let ApiEvent::Permissions(items) = event {
                lock(&observed).extend(
                    items
                        .iter()
                        .filter(|item| item.kind == PermissionKind::Firewall)
                        .map(|item| item.state),
                );
            }
        }));
    }
    pair(&a, &b).await;
    until("actual authenticated incoming Low socket", || {
        [&a, &b].iter().any(|node| node.links().unwrap().tcp_inbound_verified())
    })
    .await;
    let proof_index = usize::from(!a.links().unwrap().tcp_inbound_verified());
    let nodes = [&a, &b];
    let receiver = nodes[proof_index];
    let sender = nodes[1 - proof_index];
    until("inbound proof reached subscribed view", || {
        lock(&events[proof_index]).contains(&PermissionState::Granted)
    })
    .await;
    assert_eq!(
        lock(&events[proof_index])
            .iter()
            .filter(|state| **state == PermissionState::Granted)
            .count(),
        1
    );
    sender.shutdown().await;
    until("last connection proof removed from view", || {
        !receiver.links().unwrap().tcp_inbound_verified()
            && lock(&events[proof_index]).last() == Some(&PermissionState::NotDetermined)
    })
    .await;
    receiver.shutdown().await;
}

pub(crate) fn input_id(node: &Node, peer: DeviceId) -> Option<StreamId> {
    node.input_status()
        .ok()?
        .peers
        .iter()
        .find(|p| p.peer == peer && p.attached)
        .and_then(|p| p.outbound)
}
pub(crate) fn link_id(node: &Node, peer: DeviceId) -> Option<LinkId> {
    node.links()
        .ok()?
        .links()
        .links
        .iter()
        .find(|l| l.peer == peer)
        .map(|l| l.link)
}

#[test]
fn relayed_wallpaper_resume_uses_latest_displays() {
    crate::test_watchdog::run(
        module_path!(),
        "relayed_wallpaper_resume_uses_latest_displays",
        Some(4),
        relayed_wallpaper_resume_uses_latest_displays_scenario(),
    );
}

async fn relayed_wallpaper_resume_uses_latest_displays_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let ds = [Displays::new("a"), Displays::new("b"), Displays::new("relay")];
    let mut nodes = Vec::new();
    for (name, display) in ["a", "b", "relay"].into_iter().zip(&ds) {
        nodes.push(
            start_with_clip(
                &oink_test_support::network_config(dir.path().join(name), name, 0),
                display,
                None,
            )
            .await,
        );
    }
    let (a, b, r) = (&nodes[0], &nodes[1], &nodes[2]);
    oink_network::link::testing::set_direct_blocked(&a.links().unwrap(), b.id(), true);
    oink_network::link::testing::set_direct_blocked(&b.links().unwrap(), a.id(), true);
    pair(r, a).await;
    pair(r, b).await;
    common::active(&[a, b, r]).await;
    until("actual relay", || {
        a.links()
            .unwrap()
            .links()
            .links
            .iter()
            .any(|l| l.peer == b.id() && matches!(l.path, oink_network::link::LinkPath::Relayed { .. }))
    })
    .await;
    until("initial wallpaper", || {
        crate::wallpaper::list(b).iter().any(|w| w.key.device.0 == a.id())
    })
    .await;
    let link = link_id(a, b.id()).unwrap();
    for _ in 0..3 {
        pause(a, true).await;
        until("group paused", || {
            a.fully_paused() && !b.fully_paused() && !r.fully_paused() && input_id(a, b.id()).is_none()
        })
        .await;
        let before = crate::wallpaper::list(a)
            .into_iter()
            .find(|w| w.key.device.0 == a.id())
            .unwrap()
            .rev;
        ds[0].change_pixels();
        until("local wallpaper refreshed while paused", || {
            crate::wallpaper::list(a)
                .iter()
                .any(|w| w.key.device.0 == a.id() && w.rev != before)
        })
        .await;
        let latest = crate::wallpaper::list(a)
            .into_iter()
            .find(|w| w.key.device.0 == a.id())
            .unwrap()
            .rev;
        pause(a, false).await;
        common::active(&[a, b, r]).await;
        until("resume transmits current wallpaper", || {
            crate::wallpaper::list(b)
                .iter()
                .any(|w| w.key.device.0 == a.id() && w.rev == latest)
        })
        .await;
        assert_eq!(link_id(a, b.id()), Some(link));
    }
    for node in nodes {
        node.shutdown().await;
    }
}

#[test]
fn trail_preferences_remain_local_and_survive_restart() {
    crate::test_watchdog::run(
        module_path!(),
        "trail_preferences_remain_local_and_survive_restart",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let configs = [
                oink_test_support::network_config(dir.path().join("a"), "a", 0),
                oink_test_support::network_config(dir.path().join("b"), "b", 0),
            ];
            let a = start_with_clip(&configs[0], &Displays::new("a"), None).await;
            let b = start_with_clip(&configs[1], &Displays::new("b"), None).await;
            pair(&a, &b).await;
            let version = a.layout().1;
            let mut settings = a.input_settings().unwrap();
            for duration in [99, 60_001] {
                settings.cursor_trail_duration_ms = duration;
                assert!(a.set_input_settings(settings.clone().into()).is_err());
            }
            settings.cursor_trail_duration_ms = 10_000;
            settings.cursor_trail = false;
            input_preferences(&a, settings).await;
            assert!(b.input_settings().unwrap().cursor_trail);
            assert_eq!(b.input_settings().unwrap().cursor_trail_duration_ms, 270);
            assert_eq!(a.layout().1, version);
            let local = crate::config::AgentConfig::load_from(&configs[0].data_dir).unwrap();
            assert!(!local.settings.routing.cursor_trail);
            assert_eq!(local.settings.cursor_trail_duration_ms, 10_000);
            a.shutdown().await;
            b.shutdown().await;
            let restarted = start_with_clip(&configs[0], &Displays::new("a"), None).await;
            assert!(!restarted.input_settings().unwrap().cursor_trail);
            assert_eq!(restarted.input_settings().unwrap().cursor_trail_duration_ms, 10_000);
            restarted.shutdown().await;
        },
    );
}

#[test]
fn unavailable_contacts_are_effectively_disabled_without_rewriting_preference() {
    crate::test_watchdog::run(module_path!(), "unavailable contacts settings", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let config = oink_test_support::network_config(dir.path().join("a"), "a", 0);
        let node = start_with_clip(&config, &Displays::new("a"), None).await;
        assert!(node.settings().accept_contacts);
        let settings = serde_json::to_value(node.input_settings().unwrap()).unwrap();
        assert_eq!(settings["accept_contacts"], false);
        assert_eq!(settings["contacts_injection_available"], false);
        assert!(!node.status().unwrap().accept_contacts);
        assert!(
            node.set_input_settings(serde_json::from_value(serde_json::json!({"accept_contacts":true})).unwrap())
                .is_err()
        );
        node.set_input_settings(serde_json::from_value(serde_json::json!({"event_frequency":"hz125"})).unwrap())
            .unwrap();
        assert!(
            node.settings().accept_contacts,
            "unrelated patch must preserve stored true"
        );
        node.shutdown().await;
    });
}

#[test]
fn local_contacts_capability_and_effective_preference_recover() {
    crate::test_watchdog::run(module_path!(), "contacts capability facts", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let configs = [
            oink_test_support::network_config(dir.path().join("a"), "a", 0),
            oink_test_support::network_config(dir.path().join("b"), "b", 0),
        ];
        let input = Arc::new(FakeInput::new());
        input.set_touchpad_contacts(Some(5));
        let a = Node::start(NodeOptions {
            net: configs[0].clone(),
            input: input.clone(),
            displays: vec![],
            display_backend: Some(Displays::new("a")),
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        })
        .await
        .unwrap();
        let b = start_with_clip(&configs[1], &Displays::new("b"), None).await;
        pair(&a, &b).await;
        let (send, receive) = std::sync::mpsc::channel();
        let subscription = a.subscribe(Arc::new(move |event| {
            if let crate::api::ApiEvent::InputSettings(settings) = event {
                let _ = send.send((settings.accept_contacts, settings.contacts_injection_available));
            }
        }));
        receive.try_iter().for_each(drop);
        for available in [false, true, false] {
            input.set_touchpad_contacts(available.then_some(5));
            a.publish_local_options();
            until("local settings view follows injector changes", || {
                receive.try_iter().any(|v| v == (available, available))
            })
            .await;
            assert_eq!(a.input_settings().unwrap().accept_contacts, available);
            assert!(a.settings().accept_contacts);
            if !available {
                let requester = b.clone();
                let target = a.id();
                let result = tokio::task::spawn_blocking(move || {
                    requester.edit_device_option(
                        target.into(),
                        oink_protocol::v1::dto::LocalOption::AcceptContacts,
                        oink_protocol::v1::RawValue::Bool(true),
                    )
                })
                .await
                .unwrap();
                assert!(
                    result.is_err(),
                    "remote owner command cannot enable an unavailable injector"
                );
                assert!(a.settings().accept_contacts);
            }
            // Existing owner facts retain the stored preference; runtime
            // injector availability stays local to this receiver/UI API.
            let preference = b
                .device_facts(a.id())
                .unwrap()
                .preferences()
                .unwrap()
                .unwrap()
                .0
                .into_iter()
                .find(|p| p.option() == Ok(oink_protocol::v1::dto::LocalOption::AcceptContacts))
                .unwrap();
            assert_eq!(preference.value().unwrap(), oink_protocol::v1::RawValue::Bool(true));
        }
        assert!(a.input_settings().unwrap().mac_scroll_feel);
        assert!(b.input_settings().unwrap().mac_scroll_feel);
        assert!(!a.input_settings().unwrap().forward_injected_input);
        assert!(!b.input_settings().unwrap().forward_injected_input);
        for enabled in [true, false] {
            let node = a.clone();
            tokio::task::spawn_blocking(move || {
                node.set_input_settings(
                    serde_json::from_value(serde_json::json!({"forward_injected_input": enabled})).unwrap(),
                )
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(a.input_settings().unwrap().forward_injected_input, enabled);
            assert_eq!(
                a.inner.input_settings.load().forward_injected_input,
                enabled,
                "live capture snapshot"
            );
            assert_eq!(
                crate::config::AgentConfig::load_from(&configs[0].data_dir)
                    .unwrap()
                    .settings
                    .routing
                    .forward_injected_input,
                enabled
            );
            let stored: serde_json::Value =
                serde_json::from_slice(&std::fs::read(configs[0].data_dir.join("settings.json")).unwrap()).unwrap();
            assert_eq!(stored["local"]["routing"]["forward_injected_input"], enabled);
            assert!(
                !b.input_settings().unwrap().forward_injected_input,
                "capture preference must not propagate to peer"
            );
        }
        for enabled in [false, true] {
            let node = a.clone();
            tokio::task::spawn_blocking(move || {
                node.set_input_settings(serde_json::from_value(serde_json::json!({"mac_scroll_feel":enabled})).unwrap())
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(a.input_settings().unwrap().mac_scroll_feel, enabled);
            assert_eq!(
                crate::config::AgentConfig::load_from(&configs[0].data_dir)
                    .unwrap()
                    .settings
                    .routing
                    .mac_scroll_feel,
                enabled
            );
            let stored: serde_json::Value =
                serde_json::from_slice(&std::fs::read(configs[0].data_dir.join("settings.json")).unwrap()).unwrap();
            assert_eq!(stored["local"]["routing"]["mac_scroll_feel"], enabled);
            assert!(
                b.input_settings().unwrap().mac_scroll_feel,
                "receiver preference is never synchronized"
            );
        }
        a.unsubscribe(subscription);
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn contact_acceptance_is_owned_by_the_receiver_and_survives_restart() {
    crate::test_watchdog::run(
        module_path!(),
        "contact_acceptance_is_owned_by_the_receiver_and_survives_restart",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let configs = [
                oink_test_support::network_config(dir.path().join("a"), "a", 0),
                oink_test_support::network_config(dir.path().join("b"), "b", 0),
            ];
            let a = start_with_clip(&configs[0], &Displays::new("a"), None).await;
            let b = start_with_clip(&configs[1], &Displays::new("b"), None).await;
            pair(&a, &b).await;
            let version = a.layout().1;
            let mut settings = a.input_settings().unwrap();
            settings.accept_contacts = false;
            input_preferences(&a, settings).await;
            assert!(!b.input_settings().unwrap().accept_contacts);
            assert!(b.settings().accept_contacts);
            assert_eq!(a.layout().1, version);
            until("receiver preference reaches peer facts", || {
                b.device_facts(a.id())
                    .and_then(|facts| facts.preferences().ok().flatten())
                    .is_some_and(|items| {
                        items.0.iter().any(|item| {
                            item.option() == Ok(oink_protocol::v1::dto::LocalOption::AcceptContacts)
                                && item.value() == Ok(oink_protocol::v1::RawValue::Bool(false))
                        })
                    })
            })
            .await;
            assert!(
                !crate::config::AgentConfig::load_from(&configs[0].data_dir)
                    .unwrap()
                    .settings
                    .accept_contacts
            );
            a.shutdown().await;
            b.shutdown().await;
            let restarted = start_with_clip(&configs[0], &Displays::new("a"), None).await;
            assert!(!restarted.input_settings().unwrap().accept_contacts);
            restarted.shutdown().await;
        },
    );
}

/// Exercise the whole owner -> dispatcher -> API-event path with real QUIC.
/// Fake OS callbacks do not access the user's display or wallpaper.
#[test]
fn display_callbacks_are_immediate_and_wallpaper_reads_settle() {
    crate::test_watchdog::run(
        module_path!(),
        "display_callbacks_are_immediate_and_wallpaper_reads_settle",
        Some(4),
        display_callbacks_are_immediate_and_wallpaper_reads_settle_scenario(),
    );
}

async fn display_callbacks_are_immediate_and_wallpaper_reads_settle_scenario() {
    use crate::api::{ApiEvent, LiveViews};
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let displays = [Displays::new("event-a"), Displays::new("event-b")];
    let a = start_with_clip(
        &oink_test_support::network_config(dir.path().join("a"), "event-a", 0),
        &displays[0],
        None,
    )
    .await;
    let b = start_with_clip(
        &oink_test_support::network_config(dir.path().join("b"), "event-b", 0),
        &displays[1],
        None,
    )
    .await;
    pair(&a, &b).await;
    active(&a, &b).await;
    until("initial wallpapers", || {
        a.wallpapers(false).unwrap().len() == 2 && b.wallpapers(false).unwrap().len() == 2
    })
    .await;
    // Neither the platform sensor poll nor periodic fact publication can repair
    // a missed callback. The changed value must traverse the event path.
    for node in [&a, &b] {
        node.hold_platform_sensor_clock().await;
        node.link_config().unwrap().hold_periodic_facts();
    }
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let source = a.id();
    let expected = displays[0].info().pixel_width + 2;
    let subscriptions: Vec<_> = [&a, &b]
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            let send = send.clone();
            let observed = node.clone();
            node.subscribe_views(
                Arc::new(move |event| {
                    let kind = match event {
                        ApiEvent::Layout(layout)
                            if layout
                                .geometry
                                .screens
                                .iter()
                                .any(|s| s.key.device.0 == source && s.display.pixel_width == expected) =>
                        {
                            0
                        }
                        ApiEvent::Wallpapers
                            if observed.wallpapers(false).unwrap().iter().any(|wallpaper| {
                                wallpaper.key.device.0 == source && wallpaper.image == expected_wallpaper(expected + 2)
                            }) =>
                        {
                            1
                        }
                        _ => return,
                    };
                    let _ = send.send((kind, index, Instant::now()));
                }),
                LiveViews::default(),
            )
        })
        .collect();
    while receive.try_recv().is_ok() {}
    // Display owner and C0 fact publication: one source callback, two UI events.
    let at = Instant::now();
    displays[0].change_pixels();
    let mut display = [None, None];
    {
        while display.iter().any(Option::is_none) {
            let (kind, index, when) = receive.recv().await.unwrap();
            if kind == 0 {
                display[index].get_or_insert(when.duration_since(at));
            }
        }
    }
    // Wait for the precise display revision, not equality of two stale snapshots.
    crate::test_watchdog::until("display wallpaper revision", || {
        [&a, &b].iter().all(|node| {
            node.wallpapers(false)
                .unwrap()
                .iter()
                .any(|wallpaper| wallpaper.key.device.0 == source && wallpaper.image == expected_wallpaper(expected))
        })
    })
    .await;
    let at = Instant::now();
    displays[0].change_wallpaper();
    let mut wallpaper = [None, None];
    {
        while wallpaper.iter().any(Option::is_none) {
            let (kind, index, when) = receive.recv().await.unwrap();
            if kind == 1 {
                wallpaper[index].get_or_insert(when.duration_since(at));
            }
        }
    }
    for (i, node) in [&a, &b].into_iter().enumerate() {
        node.unsubscribe(subscriptions[i]);
    }
    a.shutdown().await;
    b.shutdown().await;
    eprintln!("UI_LATENCY display={display:?} wallpaper={wallpaper:?}");
    // Source quiet-window and callback wake boundaries have separate virtual /
    // causal tests; this integration test checks the exact resulting content.
}

fn expected_wallpaper(width: u32) -> String {
    use base64::Engine;
    let mut jpeg = vec![0xff, 0xd8];
    jpeg.resize(64, (width % 255) as u8);
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    )
}

#[cfg(windows)]
#[test]
fn healthy_keyboard_card_does_not_depend_on_optional_service_permission() {
    crate::test_watchdog::run(
        module_path!(),
        "healthy_keyboard_card_does_not_depend_on_optional_service_permission",
        Some(2),
        async {
            use crate::api::PermissionKind;
            use crate::device_cards::{CardAction, CardCondition, ResourceFaultCode, SharingIcon};
            let dir = tempfile::tempdir().unwrap();
            let node = start_with_clip(
                &oink_test_support::network_config(dir.path().into(), "keyboard", 0),
                &Displays::new("keyboard"),
                None,
            )
            .await;
            // The recording native backend reports real capture attachment to
            // the driver, while installer/service observation is unavailable.
            until("fake backend capture attached", || node.capture_active() == Some(true)).await;
            assert!(
                crate::agent_api::permission_items(&node)
                    .iter()
                    .any(|item| item.kind == PermissionKind::SystemService
                        && item.state == oink_foundation::input::PermissionState::NotDetermined)
            );
            until("healthy capture outcome reaches keyboard card", || {
                node.device_cards().unwrap().cards.iter().any(|card| {
                    card.is_self
                        && card.status.as_ref().is_some_and(|status| {
                            let keyboard = &status.keyboard.resource;
                            keyboard.condition == CardCondition::Ready
                                && keyboard.icon == SharingIcon::Shared
                                && keyboard
                                    .fault
                                    .as_ref()
                                    .is_some_and(|fault| fault.code == ResourceFaultCode::None)
                                && matches!(keyboard.action, CardAction::Invoke { .. })
                        })
                })
            })
            .await;
            node.shutdown().await;
        },
    );
}
