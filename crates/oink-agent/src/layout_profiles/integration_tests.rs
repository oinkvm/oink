//! Real Node/IPC/Low/C0 behavior with injected displays. These do not certify an OS write.
use super::{model, types::*};
use crate::{
    api::{Request, dispatch},
    node::{Node, NodeOptions, lock},
    snapshot_tests::common,
};
use oink_display::{
    ChangeHandler, DisplayBackend, DisplayError,
    arrangement::{NativeConfiguration, NativeDisplay, NativeOrigin},
};
use oink_foundation::{Point, Rect, Size, display::DisplayInfo};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
struct Displays {
    state: Mutex<NativeConfiguration>,
    calls: AtomicUsize,
    reads: AtomicUsize,
    read_fail: std::sync::atomic::AtomicBool,
    watchers: Mutex<Vec<ChangeHandler>>,
    fail: std::sync::atomic::AtomicBool,
}
impl Displays {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(NativeConfiguration {
                displays: vec![
                    NativeDisplay {
                        key: "main".into(),
                        bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                        primary: true,
                        mirror_of: None,
                        mode: "mode-a".into(),
                    },
                    NativeDisplay {
                        key: "second".into(),
                        bounds: Rect::new(100.0, 0.0, 100.0, 100.0),
                        primary: false,
                        mirror_of: None,
                        mode: "mode-b".into(),
                    },
                ],
            }),
            calls: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            read_fail: std::sync::atomic::AtomicBool::new(false),
            watchers: Mutex::new(Vec::new()),
            fail: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn changed(&self) {
        for watcher in lock(&self.watchers).clone() {
            watcher();
        }
    }
}
impl DisplayBackend for Displays {
    fn displays(&self) -> std::result::Result<Vec<DisplayInfo>, DisplayError> {
        Ok(lock(&self.state)
            .displays
            .iter()
            .map(|s| DisplayInfo {
                key: s.key.clone(),
                name: s.key.clone(),
                bounds: s.bounds,
                pixel_width: 100,
                pixel_height: 100,
                reference_extent: Some(Size::new(1.0, 1.0)),
                rotation_deg: 0,
                is_primary: s.primary,
                is_builtin: false,
                is_virtual: false,
            })
            .collect())
    }
    fn watch(&self, watcher: ChangeHandler) -> std::result::Result<(), DisplayError> {
        lock(&self.watchers).push(watcher);
        Ok(())
    }
    fn native_configuration(&self) -> std::result::Result<NativeConfiguration, DisplayError> {
        let fail = self.read_fail.load(Ordering::SeqCst);
        self.reads.fetch_add(1, Ordering::SeqCst);
        if fail {
            return Err(DisplayError::Platform("Injected native read failure".into()));
        }
        Ok(lock(&self.state).clone())
    }
    fn apply_native_origins(
        &self,
        expected: &NativeConfiguration,
        origins: &[NativeOrigin],
    ) -> std::result::Result<NativeConfiguration, DisplayError> {
        let mut state = lock(&self.state);
        if &*state != expected {
            return Err(DisplayError::Arrangement(
                oink_display::arrangement::ArrangementError::Changed,
            ));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(DisplayError::Unsupported);
        }
        *state = state.with_origins(origins)?;
        Ok(state.clone())
    }
}
async fn start(dir: &std::path::Path, displays: Arc<Displays>) -> Node {
    let node = Node::start(NodeOptions {
        net: oink_test_support::network_config(dir.into(), "profiles-fixture", 0),
        input: Arc::new(oink_test_support::input::FakeInput::new()),
        displays: displays.displays().unwrap(),
        display_backend: Some(displays),
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    })
    .await
    .unwrap();
    let n = node.clone();
    tokio::task::spawn_blocking(move || n.refresh_native_arrangement())
        .await
        .unwrap()
        .unwrap();
    node
}
async fn call<T: serde::de::DeserializeOwned>(node: &Node, request: Request) -> T {
    let diagnostic = format!("{request:?}");
    let n = node.clone();
    let reply = tokio::task::spawn_blocking(move || dispatch(&n, request))
        .await
        .unwrap()
        .unwrap();
    let mut value = serde_json::to_value(reply).unwrap();
    // Mutation acceptance is asynchronous; test the final authoritative event
    // state rather than assuming the IPC reply completed the operation.
    if let Some(id) = value["operation_id"].as_str().map(str::to_owned) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let n = node.clone();
            let state = tokio::task::spawn_blocking(move || n.profile_snapshot())
                .await
                .unwrap()
                .unwrap();
            if state
                .operation
                .as_ref()
                .is_some_and(|op| op.id == id && op.status != OperationStatus::Running)
            {
                value["state"] = serde_json::to_value(state).unwrap();
                break;
            }
            assert!(std::time::Instant::now() < deadline, "operation did not settle: {id}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("profile reply: {value}; {e}; request={diagnostic}"))
}
async fn query(node: &Node) -> LayoutProfilesState {
    call(node, Request::LayoutProfiles).await
}
async fn enable(node: &Node, device: crate::api::DeviceKey, id: &str) {
    let state = query(node).await;
    let owner = state.devices.iter().find(|d| d.device == device).unwrap();
    let _: LayoutProfilesState = call(
        node,
        Request::SetSyncSystemArrangement(SettingRequest {
            request_id: id.into(),
            device,
            expected_preference_token: owner.preference_token.clone(),
            enabled: true,
        }),
    )
    .await;
}
#[test]
fn failed_native_observations_are_logged_without_retrying_or_writing() {
    crate::test_watchdog::run(module_path!(), "native observation diagnostics", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        node.hold_platform_sensor_clock().await;
        displays.read_fail.store(true, Ordering::SeqCst);
        let reads = displays.reads.load(Ordering::SeqCst);
        let output = tempfile::NamedTempFile::new().unwrap();
        let writer = output.reopen().unwrap();
        let n = node.clone();
        let backend = displays.clone();
        tokio::task::spawn_blocking(move || {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(Mutex::new(writer))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                for site in ["startup", "observer"] {
                    let error = n.observe_native_arrangement(site).unwrap_err();
                    assert_eq!(error.code, ErrorCode::FactsUnavailable);
                    assert!(error.message.contains("Injected native read failure"));
                }
                backend.read_fail.store(false, Ordering::SeqCst);
                lock(&backend.state).displays[0].primary = false;
                let error = n.observe_native_arrangement("observer").unwrap_err();
                assert_eq!(error.code, ErrorCode::FactsUnavailable);
                assert_eq!(error.message, "Main screen unavailable");
            });
        })
        .await
        .unwrap();
        assert_eq!(displays.reads.load(Ordering::SeqCst), reads + 3);
        assert_eq!(displays.calls.load(Ordering::SeqCst), 0);
        node.resume_platform_sensor_clock();
        node.shutdown().await;
        let logs = std::fs::read_to_string(output.path()).unwrap();
        assert_eq!(logs.matches("native arrangement observation failed").count(), 3);
        for site in ["startup", "observer"] {
            assert!(logs.contains(&format!("call_site=\"{site}\"")), "{logs}");
        }
        assert!(logs.contains("Injected native read failure"), "{logs}");
        assert!(logs.contains("operation=\"native_state\" display_paths=2"), "{logs}");
    });
}
#[test]
fn completed_native_observation_keeps_save_tokens_current() {
    crate::test_watchdog::run(module_path!(), "native observation publication", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        let before = query(&node).await;
        // A real mode change must invalidate a previously issued save token.
        lock(&displays.state).displays[0].mode = "changed-mode".into();
        let n = node.clone();
        let observed = tokio::task::spawn_blocking(move || n.refresh_native_arrangement())
            .await
            .unwrap()
            .unwrap();
        let current = query(&node).await;
        assert_eq!(
            current.devices[0].native_token.as_deref(),
            Some(observed.token.as_str())
        );
        assert_ne!(current.token, before.token);
        let refused: serde_json::Value = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "save-before-mode-change".into(),
                expected_token: before.token,
                name: "Outdated".into(),
            }),
        )
        .await;
        assert_eq!(refused["error"]["code"], "stale_state");
        assert_eq!(query(&node).await.catalog_token, before.catalog_token);

        // Re-enumerating identical displays is the startup observer publication
        // that used to expose a native token after refresh had already returned.
        let n = node.clone();
        let info = displays.displays().unwrap();
        tokio::task::spawn_blocking(move || n.link_config().unwrap().initialize_displays(info))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(query(&node).await.token, current.token);
        let saved: LayoutProfilesState = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "save-observed-mode".into(),
                expected_token: current.token,
                name: "Current".into(),
            }),
        )
        .await;
        assert_eq!(saved.profiles.len(), before.profiles.len() + 1);
        assert_eq!(displays.calls.load(Ordering::SeqCst), 0);
        node.shutdown().await;
    });
}
#[test]
fn native_screen_preview_leaves_other_computers_fixed_for_frontend_release() {
    crate::test_watchdog::run(module_path!(), "native preview collision ownership", Some(6), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let da = Displays::new();
        let db = Displays::new();
        let a = start(dirs[0].path(), da.clone()).await;
        let b = start(dirs[1].path(), db.clone()).await;
        let source_default = query(&a).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &source_default).await;
        enable(&a, a.id().into(), "enable-native-edit").await;
        let state = query(&a).await;
        let mut placements = state.snapshot.as_ref().unwrap().placements.clone();
        let own = placements.iter().find(|p| p.device == a.id().into()).unwrap().clone();
        // The peer starts just below the second screen, outside today's desktop.
        placements
            .iter_mut()
            .find(|p| p.device == b.id().into())
            .unwrap()
            .origin = Point::new(own.origin.x + 100.0 * own.scale, own.origin.y + 100.0 * own.scale);
        let _: OperationReply = call(
            &a,
            Request::CommitArrangement(CommitRequest {
                request_id: "place-peer".into(),
                expected_token: state.token,
                placements: placements.clone(),
                native_plan_tokens: Vec::new(),
                disabled: Vec::new(),
                screen_shared: Vec::new(),
            }),
        )
        .await;
        let state = query(&a).await;
        let preview: PreviewReply = call(
            &a,
            Request::PreviewSystemArrangement(PreviewRequest {
                sequence: "1".into(),
                expected_token: state.token,
                device: a.id().into(),
                moved_screen: crate::api::ScreenKey {
                    device: a.id().into(),
                    display: "second".into(),
                },
                desired_canvas_origin: Point::new(own.origin.x + 100.0 * own.scale, own.origin.y + 50.0 * own.scale),
            }),
        )
        .await;
        assert_eq!(
            preview
                .native
                .screens
                .iter()
                .find(|s| s.screen.display == "second")
                .unwrap()
                .origin,
            Point::new(100.0, 50.0)
        );
        assert_eq!(
            preview.placements, placements,
            "canvas collision release belongs to the frontend and must preserve unedited computers"
        );
        assert!(preview.adjustments.is_empty());
        assert_eq!(da.calls.load(Ordering::SeqCst), 0);
        assert_eq!(db.calls.load(Ordering::SeqCst), 0);
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn profile_ipc_native_edit_dedup_load_undo_and_restart() {
    crate::test_watchdog::run(module_path!(), "profile IPC owner", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let display = Displays::new();
        let node = start(dir.path(), display.clone()).await;
        let initial = query(&node).await;
        let default = initial.active_profile_id.clone().unwrap();
        let _: LayoutProfilesState = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "working-copy".into(),
                expected_token: initial.token.clone(),
                name: "Working".into(),
            }),
        )
        .await;
        enable(&node, node.id().into(), "enable").await;
        let state = query(&node).await;
        let current = state.snapshot.as_ref().unwrap();
        let p = &current.placements[0];
        let preview: PreviewReply = call(
            &node,
            Request::PreviewSystemArrangement(PreviewRequest {
                sequence: "1".into(),
                expected_token: state.token.clone(),
                device: node.id().into(),
                moved_screen: crate::api::ScreenKey {
                    device: node.id().into(),
                    display: "second".into(),
                },
                desired_canvas_origin: Point::new(p.origin.x, p.origin.y + 100.0 * p.scale),
            }),
        )
        .await;
        // A normal native drag plan can be committed with the whole computer's
        // compacted placement. Compaction needs no second native preview/write.
        let mut compacted = preview.placements.clone();
        compacted[0].origin.x += 0.25;
        compacted[0].origin.y += 0.125;
        let expected_native = preview.native.screens.clone();
        let mut bits = current.disabled.clone();
        bits.iter_mut().find(|b| b.screen.display == "second").unwrap().disabled = true;
        let request = Request::CommitArrangement(CommitRequest {
            screen_shared: Vec::new(),
            request_id: "edit-once".into(),
            expected_token: state.token,
            placements: compacted.clone(),
            native_plan_tokens: vec![preview.plan_token],
            disabled: bits,
        });
        let edited: OperationReply = call(&node, request.clone()).await;
        assert_eq!(
            edited.state.operation.as_ref().unwrap().status,
            OperationStatus::Applied
        );
        assert_eq!(display.calls.load(Ordering::SeqCst), 1);
        let applied = query(&node).await.snapshot.unwrap();
        assert_eq!(applied.placements, compacted);
        assert_eq!(applied.native[0].screens, expected_native);
        let _: OperationReply = call(&node, request).await;
        assert_eq!(display.calls.load(Ordering::SeqCst), 1);
        let saved: LayoutProfilesState = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "save-as".into(),
                expected_token: edited.state.token,
                name: "Vertical".into(),
            }),
        )
        .await;
        let vertical = saved.active_profile_id.clone().unwrap();
        let loaded: OperationReply = call(
            &node,
            Request::LoadLayoutProfile(Load {
                request_id: "load-default".into(),
                expected_token: saved.token,
                profile_id: default.clone(),
            }),
        )
        .await;
        let op = loaded.state.operation.unwrap();
        assert!(op.history_required);
        assert_eq!(display.calls.load(Ordering::SeqCst), 2);
        let undone: OperationReply = call(
            &node,
            Request::ReplayArrangement(ReplayRequest {
                request_id: "undo".into(),
                expected_token: loaded.state.token,
                from: op.after.unwrap(),
                target: op.before,
                native_restore_devices: op.native_restore_devices,
                direction: Direction::Undo,
                original_kind: HistoryKind::Load,
            }),
        )
        .await;
        assert_eq!(undone.state.active_profile_id, Some(vertical.clone()));
        assert_eq!(display.calls.load(Ordering::SeqCst), 3);
        node.shutdown().await;
        let restarted = start(dir.path(), display.clone()).await;
        let restored = query(&restarted).await;
        assert_eq!(restored.active_profile_id, Some(vertical));
        assert_eq!(restored.profiles.len(), 3);
        assert_eq!(display.calls.load(Ordering::SeqCst), 3);
        restarted.shutdown().await;
    });
}
#[test]
fn profiles_merge_and_sync_over_low_without_applying_native() {
    crate::test_watchdog::run(module_path!(), "profile merge sync", Some(6), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let da = Displays::new();
        let db = Displays::new();
        let a = start(dirs[0].path(), da.clone()).await;
        let b = start(dirs[1].path(), db.clone()).await;
        let sa = query(&a).await;
        let _: LayoutProfilesState = call(
            &a,
            Request::CreateLayoutProfile(Create {
                request_id: "a-save".into(),
                expected_token: sa.token,
                name: "First desk".into(),
            }),
        )
        .await;
        let sb = query(&b).await;
        let _: LayoutProfilesState = call(
            &b,
            Request::CreateLayoutProfile(Create {
                request_id: "b-save".into(),
                expected_token: sb.token,
                name: "Second desk".into(),
            }),
        )
        .await;
        common::pair(&a, &b).await;
        common::until("profile catalogs union and converge", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| c.profiles.len() == 3) && ca == cb
        })
        .await;
        assert_eq!(da.calls.load(Ordering::SeqCst) + db.calls.load(Ordering::SeqCst), 0);
        let sa = query(&a).await;
        let active = sa.active_profile_id.clone().unwrap();
        let _: LayoutProfilesState = call(
            &a,
            Request::RenameLayoutProfile(Rename {
                request_id: "rename".into(),
                expected_catalog_token: sa.catalog_token,
                profile_id: active,
                name: "Merged desk".into(),
            }),
        )
        .await;
        common::until("renamed catalog reaches peer", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca == cb
        })
        .await;
        // Exercise the larger catalog over the real Low transport, then deletion.
        for i in 3..16 {
            let state = query(&a).await;
            let _: LayoutProfilesState = call(
                &a,
                Request::CreateLayoutProfile(Create {
                    request_id: format!("fill-{i}"),
                    expected_token: state.token,
                    name: format!("Desk {i}"),
                }),
            )
            .await;
        }
        common::until("sixteen profiles reach peer", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| c.profiles.len() == 16) && ca == cb
        })
        .await;
        let state = query(&a).await;
        assert!(!state.create_enabled);
        let _: LayoutProfilesState = call(
            &a,
            Request::DeleteLayoutProfile(Delete {
                request_id: "delete-current".into(),
                expected_catalog_token: state.catalog_token,
                profile_id: state.active_profile_id.unwrap(),
            }),
        )
        .await;
        common::until("deleted profile reaches peer", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| c.profiles.len() == 15) && ca == cb
        })
        .await;
        let qa = query(&a).await;
        let qb = query(&b).await;
        assert!(qa.create_enabled);
        assert!(model::same_content(
            qa.snapshot.as_ref().unwrap(),
            qb.snapshot.as_ref().unwrap()
        ));
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn native_owner_fences_mode_changes_and_status_queries_never_repeat_a_write() {
    crate::test_watchdog::run(module_path!(), "native dedup fences", Some(4), async {
        use oink_protocol::v1::RawValue;
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        enable(&node, node.id().into(), "enable-native").await;
        let state = query(&node).await;
        let mut target = state.snapshot.unwrap().native[0].clone();
        target
            .screens
            .iter_mut()
            .find(|s| s.screen.display == "second")
            .unwrap()
            .origin = Point::new(0.0, 100.0);
        let request = node
            .make_arrangement_request("wire-apply", node.id().into(), Some(&target), &[])
            .unwrap();
        let n = node.clone();
        let r = request.clone();
        let first = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), r))
            .await
            .unwrap();
        assert_eq!(first.outcome(), Ok(super::native::APPLIED));
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        let n = node.clone();
        let r = request.clone();
        let repeat = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), r))
            .await
            .unwrap();
        assert_eq!(repeat, first);
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        let mut status = request.clone();
        status
            .edit(|raw| {
                raw.insert(3, RawValue::Bool(true));
            })
            .unwrap();
        let old_status = status.clone();
        let n = node.clone();
        let result = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), status))
            .await
            .unwrap();
        assert_eq!(result, first);
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        let changed = node
            .make_arrangement_request("mode-race", node.id().into(), Some(&target), &[])
            .unwrap();
        lock(&displays.state).displays[0].mode = "new-mode".into();
        let n = node.clone();
        let result = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), changed))
            .await
            .unwrap();
        assert_eq!(result.outcome(), Ok(super::native::FAILED));
        assert_eq!(result.error().unwrap().unwrap().0, "native_changed");
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        let mut never_sent = node
            .make_arrangement_request("not-sent", node.id().into(), None, &[])
            .unwrap();
        never_sent
            .edit(|raw| {
                raw.insert(3, RawValue::Bool(true));
            })
            .unwrap();
        let n = node.clone();
        let result = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), never_sent))
            .await
            .unwrap();
        assert_eq!(result.outcome(), Ok(super::native::UNKNOWN));
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        node.shutdown().await;
        let restarted = start(dir.path(), displays.clone()).await;
        let n = restarted.clone();
        let result = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), old_status))
            .await
            .unwrap();
        assert_eq!(result, first);
        let n = restarted.clone();
        let rejected = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), request))
            .await
            .unwrap();
        assert_eq!(rejected.outcome(), Ok(super::native::FAILED));
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        restarted.shutdown().await;
    });
}

#[test]
fn multi_owner_load_returns_actual_partial_state_without_compensation() {
    crate::test_watchdog::run(module_path!(), "partial Load", Some(6), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let da = Displays::new();
        let db = Displays::new();
        let a = start(dirs[0].path(), da.clone()).await;
        let b = start(dirs[1].path(), db.clone()).await;
        let source_default = query(&a).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &source_default).await;
        enable(&a, a.id().into(), "enable-a").await;
        enable(&a, b.id().into(), "enable-b").await;
        let state = query(&a).await;
        let saved: LayoutProfilesState = call(
            &a,
            Request::CreateLayoutProfile(Create {
                request_id: "both-saved".into(),
                expected_token: state.token,
                name: "Both horizontal".into(),
            }),
        )
        .await;
        for (node, display) in [(&a, &da), (&b, &db)] {
            lock(&display.state).displays[1].bounds = Rect::new(0.0, 100.0, 100.0, 100.0);
            let n = node.clone();
            let displays = display.displays().unwrap();
            tokio::task::spawn_blocking(move || {
                n.link_config().unwrap().initialize_displays(displays).unwrap();
                n.refresh_native_arrangement().unwrap();
            })
            .await
            .unwrap();
        }
        common::until("observed native changes converge", || {
            a.layout().0 == b.layout().0
                && a.device_facts(b.id()).and_then(|f| f.native_arrangement_token().ok())
                    == b.device_facts(b.id()).and_then(|f| f.native_arrangement_token().ok())
                && a.layout()
                    .0
                    .screens
                    .iter()
                    .filter(|s| s.id.display == "second")
                    .all(|s| s.local.y == 100.0)
        })
        .await;
        db.fail.store(true, Ordering::SeqCst);
        let state = query(&a).await;
        let result: OperationReply = call(
            &a,
            Request::LoadLayoutProfile(Load {
                request_id: "partial-load".into(),
                expected_token: state.token,
                profile_id: saved.active_profile_id.unwrap(),
            }),
        )
        .await;
        let op = result.state.operation.unwrap();
        assert_eq!(op.status, OperationStatus::Partial, "{op:#?}");
        assert!(op.history_required);
        assert_eq!(da.calls.load(Ordering::SeqCst), 1);
        assert_eq!(db.calls.load(Ordering::SeqCst), 1);
        assert_eq!(lock(&da.state).displays[1].bounds.y, 0.0);
        assert_eq!(lock(&db.state).displays[1].bounds.y, 100.0);
        assert_eq!(op.owners.iter().filter(|o| o.phase == OwnerPhase::Failed).count(), 1);
        assert_eq!(op.native_restore_devices, vec![a.id().into()]);
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn reset_and_visibility_patches_preserve_history_without_native_writes() {
    crate::test_watchdog::run(module_path!(), "reset and rigid replay", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        let initial = query(&node).await;
        let mut placements = initial.snapshot.as_ref().unwrap().placements.clone();
        placements[0].origin.x = -50.0;
        let mut bit = initial.snapshot.as_ref().unwrap().disabled[0].clone();
        bit.disabled = true;
        let edited: OperationReply = call(
            &node,
            Request::CommitArrangement(CommitRequest {
                screen_shared: Vec::new(),
                request_id: "patch".into(),
                expected_token: initial.token,
                placements,
                disabled: vec![bit.clone()],
                native_plan_tokens: Vec::new(),
            }),
        )
        .await;
        assert_eq!(edited.state.snapshot.as_ref().unwrap().disabled.len(), 2);
        assert!(edited.state.snapshot.as_ref().unwrap().disabled.contains(&bit));
        let reset: OperationReply = call(
            &node,
            Request::ResetArrangement(Mutation {
                request_id: "reset".into(),
                expected_token: edited.state.token,
            }),
        )
        .await;
        let op = reset.state.operation.as_ref().unwrap();
        assert_eq!(op.kind, OperationKind::Reset);
        assert!(op.history_required);
        assert!(op.native_restore_devices.is_empty());
        assert!(op.owners.is_empty());
        assert_eq!(op.after.as_ref().unwrap().disabled, op.before.disabled);
        lock(&displays.state).displays[1].bounds = Rect::new(0.0, 100.0, 100.0, 100.0);
        let n = node.clone();
        let d = displays.clone();
        tokio::task::spawn_blocking(move || {
            n.link_config()
                .unwrap()
                .initialize_displays(d.displays().unwrap())
                .unwrap();
            n.refresh_native_arrangement().unwrap();
        })
        .await
        .unwrap();
        let state = query(&node).await;
        let replayed: OperationReply = call(
            &node,
            Request::ReplayArrangement(ReplayRequest {
                request_id: "undo-reset".into(),
                expected_token: state.token,
                from: op.after.clone().unwrap(),
                target: op.before.clone(),
                native_restore_devices: Vec::new(),
                direction: Direction::Undo,
                original_kind: HistoryKind::Reset,
            }),
        )
        .await;
        assert_eq!(replayed.state.operation.unwrap().status, OperationStatus::Applied);
        assert_eq!(displays.calls.load(Ordering::SeqCst), 0);
        assert_eq!(replayed.state.snapshot.unwrap().placements[0].origin.x, -50.0);
        node.shutdown().await;
    });
}

#[test]
fn requester_restart_resolves_durable_pending_without_a_second_write() {
    crate::test_watchdog::run(module_path!(), "pending operation restart", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        enable(&node, node.id().into(), "enable").await;
        let state = query(&node).await;
        let before = state.snapshot.unwrap();
        let mut target = before.native[0].clone();
        target
            .screens
            .iter_mut()
            .find(|s| s.screen.display == "second")
            .unwrap()
            .origin = Point::new(0.0, 100.0);
        let request = node
            .make_arrangement_request("crash-step", node.id().into(), Some(&target), &[])
            .unwrap();
        let operation = LayoutOperation {
            id: "crash-step".into(),
            kind: OperationKind::Edit,
            status: OperationStatus::Running,
            before: before.clone(),
            after: None,
            native_restore_devices: vec![node.id().into()],
            adjustments: Vec::new(),
            owners: vec![OwnerProgress {
                device: node.id().into(),
                phase: OwnerPhase::Applying,
                error: None,
            }],
            history_required: false,
            manual_save: None,
            error: None,
        };
        let commit = CommitRequest {
            screen_shared: Vec::new(),
            request_id: "crash-step".into(),
            expected_token: state.token,
            placements: before.placements,
            disabled: Vec::new(),
            native_plan_tokens: vec!["expired-plan".into()],
        };
        {
            let mut data = lock(&node.inner.profiles.data);
            data.operation_digests
                .insert("crash-step".into(), model::hash(&Command::Commit(commit.clone())));
            data.operations.push_back(operation);
            data.unknown
                .insert("crash-step".into(), vec![(node.id().into(), request.clone())]);
        }
        node.persist_profile_operations().unwrap();
        let n = node.clone();
        let reply = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), request))
            .await
            .unwrap();
        assert_eq!(reply.outcome(), Ok(super::native::APPLIED));
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        // Simulate losing the result after the owner wrote its durable receipt.
        node.shutdown().await;
        let restarted = start(dir.path(), displays.clone()).await;
        let recovered = query(&restarted).await;
        if recovered.operation.unwrap().status == OperationStatus::OutcomeUnknown {
            assert!(!recovered.create_enabled);
        }
        common::until("pending result resolves through status polling", || {
            lock(&restarted.inner.profiles.data)
                .operations
                .back()
                .is_some_and(|op| op.status == OperationStatus::Applied)
        })
        .await;
        let result: OperationQuery = call(
            &restarted,
            Request::LayoutProfileOperation(OperationRequest {
                request_id: "crash-step".into(),
            }),
        )
        .await;
        assert_eq!(result.operation.status, OperationStatus::Applied);
        assert!(result.operation.history_required);
        let n = restarted.clone();
        tokio::task::spawn_blocking(move || {
            // Recovery publishes its durable result before it drops command_gate.
            // Freeze that window: a duplicate is a receipt read, even while the
            // resolver or another command still owns the mutation gate.
            let _command = lock(&n.inner.profiles.command_gate);
            let reply = dispatch(&n, Request::CommitArrangement(commit.clone())).unwrap();
            let reply = serde_json::to_value(reply).unwrap();
            assert_eq!(reply["operation_id"], "crash-step", "{reply}");
            assert_eq!(reply["state"]["operation"]["status"], "applied");

            let mut changed = commit.clone();
            changed.placements[0].origin.x += 1.0;
            let reply = dispatch(&n, Request::CommitArrangement(changed)).unwrap();
            let reply = serde_json::to_value(reply).unwrap();
            assert_eq!(reply["error"]["code"], "stale_state", "{reply}");

            let mut fresh = commit;
            fresh.request_id = "another-write".into();
            let reply = dispatch(&n, Request::CommitArrangement(fresh)).unwrap();
            let reply = serde_json::to_value(reply).unwrap();
            assert_eq!(reply["error"]["code"], "busy", "{reply}");
        })
        .await
        .unwrap();
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        restarted.shutdown().await;
    });
}

#[test]
fn owner_screen_sharing_is_a_saveable_replayable_arrangement_edit() {
    crate::test_watchdog::run(module_path!(), "owner visibility history", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        let initial = query(&node).await;
        let initial_id = initial.active_profile_id.clone().unwrap();
        let initial: LayoutProfilesState = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "visible-working-copy".into(),
                expected_token: initial.token,
                name: "Working".into(),
            }),
        )
        .await;
        // Exercise the real IPC decoder: older code silently discards this patch.
        let request: CommitRequest = serde_json::from_value(serde_json::json!({
            "request_id": "hide-owner", "expected_token": initial.token,
            "placements": initial.snapshot.as_ref().unwrap().placements,
            "native_plan_tokens": [], "disabled": [],
            "screen_shared": [{"device": node.id().to_hex(), "shared": false}]
        }))
        .unwrap();
        let edited: OperationReply = call(&node, Request::CommitArrangement(request)).await;
        let op = edited.state.operation.clone().unwrap();
        assert!(op.history_required);
        assert!(op.native_restore_devices.is_empty());
        let saved: LayoutProfilesState = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "save-hidden".into(),
                expected_token: edited.state.token,
                name: "Hidden".into(),
            }),
        )
        .await;
        let loaded: OperationReply = call(
            &node,
            Request::LoadLayoutProfile(Load {
                request_id: "load-visible".into(),
                expected_token: saved.token,
                profile_id: initial_id,
            }),
        )
        .await;
        assert!(loaded.state.operation.as_ref().unwrap().history_required);
        let undone: OperationReply = call(
            &node,
            Request::ReplayArrangement(ReplayRequest {
                request_id: "undo-load".into(),
                expected_token: loaded.state.token,
                from: loaded.state.operation.as_ref().unwrap().after.clone().unwrap(),
                target: loaded.state.operation.as_ref().unwrap().before.clone(),
                native_restore_devices: Vec::new(),
                direction: Direction::Undo,
                original_kind: HistoryKind::Load,
            }),
        )
        .await;
        assert!(model::same_content(
            undone.state.snapshot.as_ref().unwrap(),
            op.after.as_ref().unwrap()
        ));
        let redone: OperationReply = call(
            &node,
            Request::ReplayArrangement(ReplayRequest {
                request_id: "redo-load".into(),
                expected_token: undone.state.token,
                from: undone.state.snapshot.unwrap(),
                target: loaded.state.snapshot.unwrap(),
                native_restore_devices: Vec::new(),
                direction: Direction::Redo,
                original_kind: HistoryKind::Load,
            }),
        )
        .await;
        assert!(model::same_content(
            redone.state.snapshot.as_ref().unwrap(),
            initial.snapshot.as_ref().unwrap()
        ));
        assert_eq!(displays.calls.load(Ordering::SeqCst), 0);
        node.shutdown().await;
    });
}

#[test]
fn redo_020_manual_commit_autosaves_without_saving_automatic_geometry() {
    crate::test_watchdog::run(module_path!(), "manual autosave", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let node = start(dir.path(), Displays::new()).await;
        let initial = query(&node).await;
        let saved = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            n.link_config().unwrap().edit_layout(
                Box::new(|layout| {
                    for screen in &mut layout.screens {
                        screen.rect.x += 300.0;
                    }
                }),
                oink_protocol::v1::dto::LayoutSource::Default,
            )
        })
        .await
        .unwrap()
        .unwrap();
        let changed = query(&node).await;
        assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref().unwrap(), &saved);
        let mut bit = changed.snapshot.as_ref().unwrap().disabled[0].clone();
        bit.disabled = !bit.disabled;
        let reply: OperationReply = call(
            &node,
            Request::CommitArrangement(CommitRequest {
                request_id: "hide-manually".into(),
                expected_token: changed.token,
                placements: changed.snapshot.unwrap().placements,
                native_plan_tokens: vec![],
                disabled: vec![bit.clone()],
                screen_shared: vec![],
            }),
        )
        .await;
        assert_eq!(reply.state.operation.unwrap().status, OperationStatus::Applied);
        let actual = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        assert!(
            actual.active().content.disabled.contains(&bit),
            "manual visibility must autosave before success"
        );
        assert_eq!(
            actual.active().content.placements,
            initial.snapshot.unwrap().placements,
            "automatic repair must not leak into saved geometry"
        );
        assert!(actual.active().last_used > saved.active().last_used);
        node.shutdown().await;
    });
}

#[test]
fn redo_020_undo_edit_autosaves_into_current_profile() {
    crate::test_watchdog::run(module_path!(), "undo edit current profile", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let node = start(dir.path(), Displays::new()).await;
        let initial = query(&node).await;
        let mut bit = initial.snapshot.as_ref().unwrap().disabled[0].clone();
        bit.disabled = true;
        let edited: OperationReply = call(
            &node,
            Request::CommitArrangement(CommitRequest {
                request_id: "hide".into(),
                expected_token: initial.token,
                placements: initial.snapshot.unwrap().placements,
                disabled: vec![bit],
                screen_shared: vec![],
                native_plan_tokens: vec![],
            }),
        )
        .await;
        let op = edited.state.operation.unwrap();
        let created: LayoutProfilesState = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "create-current".into(),
                expected_token: edited.state.token,
                name: "Current".into(),
            }),
        )
        .await;
        let selected = created.active_profile_id.clone();
        let old_catalog = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        let undone: OperationReply = call(
            &node,
            Request::ReplayArrangement(ReplayRequest {
                request_id: "undo-hide".into(),
                expected_token: created.token,
                from: op.after.unwrap(),
                target: op.before.clone(),
                native_restore_devices: vec![],
                direction: Direction::Undo,
                original_kind: HistoryKind::Edit,
            }),
        )
        .await;
        assert_eq!(undone.state.active_profile_id, selected);
        assert_eq!(undone.state.operation.unwrap().kind, OperationKind::Replay);
        let c = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        assert_eq!(c.active().content.disabled, op.before.disabled);
        assert!(c.active().last_used > old_catalog.active().last_used);
        assert_eq!(
            c.profile(&op.before.active_profile_id).unwrap(),
            old_catalog.profile(&op.before.active_profile_id).unwrap()
        );
        node.shutdown().await;
    });
}

#[test]
fn redo_020_crash_after_manual_layout_recovers_autosave_once() {
    crate::test_watchdog::run(module_path!(), "manual autosave recovery", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        let state = query(&node).await;
        let before = state.snapshot.unwrap();
        let mut intended = before.clone();
        intended.placements[0].origin.x += 123.0;
        let op = LayoutOperation {
            id: "crash-manual".into(),
            kind: OperationKind::Edit,
            status: OperationStatus::Running,
            before: before.clone(),
            after: None,
            native_restore_devices: vec![],
            adjustments: vec![],
            owners: vec![],
            history_required: false,
            error: None,
            manual_save: Some(super::autosave::ManualSave {
                profile_id: before.active_profile_id,
                intended: intended.clone(),
                applied: false,
            }),
        };
        lock(&node.inner.profiles.data).operations.push_back(op);
        node.persist_profile_operations().unwrap();
        let n = node.clone();
        let p = intended.placements[0].clone();
        tokio::task::spawn_blocking(move || {
            n.link_config().unwrap().edit_layout(
                Box::new(move |l| {
                    l.place_device(p.device.0, p.origin, p.scale, true);
                }),
                oink_protocol::v1::dto::LayoutSource::Manual,
            )
        })
        .await
        .unwrap()
        .unwrap();
        // No terminal receipt/catalog save: emulate a crash at that exact boundary.
        node.shutdown().await;
        let node = start(dir.path(), displays.clone()).await;
        let state = query(&node).await;
        assert!(state.operation.unwrap().manual_save.unwrap().applied);
        let first = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        assert_eq!(first.active().content.placements, intended.placements);
        node.shutdown().await;
        let node = start(dir.path(), displays).await;
        let _ = query(&node).await;
        assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref().unwrap(), &first);
        node.shutdown().await;
    });
}

#[test]
fn legacy_manual_commit_preserves_intervening_automatic_layout_fields() {
    crate::test_watchdog::run(module_path!(), "manual commit automatic race", Some(2), async {
        let dir = tempfile::tempdir().unwrap();
        let node = start(dir.path(), Displays::new()).await;
        let initial = query(&node).await.snapshot.unwrap();
        let device = node.id();
        let original = initial
            .placements
            .iter()
            .find(|p| p.device.0 == device)
            .unwrap()
            .clone();
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            let group = n.link_config().unwrap();
            n.commit_manual_layout(|target| {
                // Deterministically publish automatic geometry after the legacy
                // edit took its snapshot but before its config transaction.
                group
                    .edit_layout(
                        Box::new(move |live| {
                            let (mut origin, scale) = live.device_transform(device).unwrap();
                            origin.y += 0.5;
                            live.place_device(device, origin, scale, false);
                        }),
                        oink_protocol::v1::dto::LayoutSource::Default,
                    )
                    .unwrap();
                let (mut origin, scale) = target.device_transform(device).unwrap();
                origin.x += 0.25;
                target.place_device(device, origin, scale, true);
                Ok(())
            })
            .unwrap();
        })
        .await
        .unwrap();
        let live = query(&node).await.snapshot.unwrap();
        let placement = live.placements.iter().find(|p| p.device.0 == device).unwrap();
        assert_eq!(placement.origin.x, original.origin.x + 0.25);
        assert_eq!(placement.origin.y, original.origin.y + 0.5);
        let saved = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        let placement = saved
            .active()
            .content
            .placements
            .iter()
            .find(|p| p.device.0 == device)
            .unwrap();
        assert_eq!(placement.origin.x, original.origin.x + 0.25);
        assert_eq!(placement.origin.y, original.origin.y);
        node.shutdown().await;
    });
}

async fn directional_default_join(source_members: usize) {
    let count = source_members + 2;
    let dirs = (0..count).map(|_| tempfile::tempdir().unwrap()).collect::<Vec<_>>();
    let displays = (0..count).map(|_| Displays::new()).collect::<Vec<_>>();
    let mut nodes = Vec::new();
    for i in 0..count {
        nodes.push(start(dirs[i].path(), displays[i].clone()).await);
    }
    for node in &nodes {
        query(node).await;
    }
    let source_default = lock(&nodes[0].inner.profiles.data)
        .catalog
        .as_ref()
        .unwrap()
        .active_profile_id
        .clone();
    if source_members == 2 {
        common::pair(&nodes[0], &nodes[1]).await;
        common::until("source group default converges", || {
            let a = lock(&nodes[0].inner.profiles.data).catalog.clone();
            let b = lock(&nodes[1].inner.profiles.data).catalog.clone();
            a.as_ref()
                .is_some_and(|c| c.profiles.len() == 1 && c.active_profile_id == source_default)
                && a == b
        })
        .await;
    }
    let target_default = lock(&nodes[source_members].inner.profiles.data)
        .catalog
        .as_ref()
        .unwrap()
        .active_profile_id
        .clone();
    common::pair(&nodes[source_members], &nodes[source_members + 1]).await;
    common::until("target group inherited default converges", || {
        let a = lock(&nodes[source_members].inner.profiles.data).catalog.clone();
        let b = lock(&nodes[source_members + 1].inner.profiles.data).catalog.clone();
        a.as_ref()
            .is_some_and(|c| c.profiles.len() == 1 && c.active_profile_id == target_default)
            && a == b
    })
    .await;
    let target_group = nodes[source_members + 1].group_id().unwrap();
    // The responder accepts; the initiating source joins its existing group.
    common::pair(&nodes[0], &nodes[source_members + 1]).await;
    common::until("directional default rule converges on all members", || {
        let catalogs = nodes
            .iter()
            .map(|n| lock(&n.inner.profiles.data).catalog.clone())
            .collect::<Vec<_>>();
        catalogs.iter().all(|c| c == &catalogs[0])
            && catalogs[0].as_ref().is_some_and(|c| {
                c.group.0 == target_group
                    && c.profiles.len() == 1
                    && c.active_profile_id == source_default
                    && c.default_profile().is_some_and(|p| p.id == source_default)
                    && c.completed_joins
                        .values()
                        .any(|j| j.dropped_defaults.contains(&target_default))
            })
    })
    .await;
    assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
    for node in nodes {
        node.shutdown().await;
    }
}

#[test]
fn group_group_pairing_drops_acceptors_inherited_default_over_low() {
    crate::test_watchdog::run(
        module_path!(),
        "two groups default import",
        Some(8),
        directional_default_join(2),
    );
}

#[test]
fn single_device_pairing_drops_acceptor_group_default_over_low() {
    crate::test_watchdog::run(
        module_path!(),
        "device joins group default import",
        Some(6),
        directional_default_join(1),
    );
}

async fn offline_acceptor_scenario(recover_projection: bool) {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let displays = [Displays::new(), Displays::new()];
    let a = start(dirs[0].path(), displays[0].clone()).await;
    let b = start(dirs[1].path(), displays[1].clone()).await;
    let source_default = query(&a).await.active_profile_id.unwrap();
    let target_default = query(&b).await.active_profile_id.unwrap();
    b.inner.profiles.defer_joins.store(true, Ordering::SeqCst);
    common::pair(&a, &b).await;
    common::until("acceptor interrupted before catalog commit", || {
        b.inner.profiles.deferred_joins.load(Ordering::SeqCst) > 0
    })
    .await;
    let group = b.group_id().unwrap().into();
    let pending = super::join::list(dirs[0].path(), group).unwrap();
    assert_eq!(pending.len(), 1);
    let id = pending[0].id.clone();
    assert!(
        pending[0]
            .catalog
            .as_ref()
            .unwrap()
            .profiles
            .iter()
            .any(|p| p.id == source_default)
    );
    assert!(
        !super::store::load(dirs[1].path(), group)
            .unwrap()
            .unwrap()
            .completed_joins
            .contains_key(&id)
    );
    b.shutdown().await;
    assert!(a.pending_profile_joins(b.id()).unwrap().iter().any(|j| j.id == id));
    a.shutdown().await;
    if recover_projection {
        // Freeze the durable membership decision before the profile handoff
        // and group projection acknowledgements. Startup must prepare the
        // exact list while no live configuration handle exists yet.
        use oink_protocol::v1::{RawValue as V, cbor, schema::WireValue};
        for (dir, node) in dirs.iter().zip([&a, &b]) {
            let path = dir.path().join(oink_network::pair_journal::FILE);
            let journal = oink_network::pair_journal::Journal::load(&path, node.id()).unwrap();
            let recovery = journal
                .recoveries()
                .unwrap()
                .into_iter()
                .find(|r| matches!(r.outcome, oink_network::pair_journal::Decision::Committed(_)))
                .unwrap();
            let oink_network::pair_journal::Decision::Committed(bundle) = recovery.outcome else {
                unreachable!()
            };
            let intent = bundle.certificate().unwrap().intent().unwrap();
            let projected = oink_network::group_merge::verify_bundle(
                &bundle,
                oink_network::group_merge::anchor(&intent.source().unwrap()).unwrap(),
            )
            .unwrap();
            let mut raw = cbor::decode_map(&std::fs::read(&path).unwrap(), 64 * 1024 * 1024).unwrap();
            raw.insert(
                9,
                V::Map(
                    [
                        (1, intent.pair_id().unwrap().write()),
                        (2, recovery.decision.write()),
                        (3, projected.wire().write()),
                    ]
                    .into(),
                ),
            );
            oink_foundation::storage::atomic_write(&path, &cbor::encode_map(&raw, 64 * 1024 * 1024).unwrap()).unwrap();
            std::fs::remove_file(dir.path().join("layout-profile-joins").join(format!("{id}.json"))).unwrap();
        }
    }
    let b = start(dirs[1].path(), displays[1].clone()).await;
    let a = start(dirs[0].path(), displays[0].clone()).await;
    common::query(&a, &b).await;
    common::active(&[&a, &b]).await;
    common::until("retained source list completes after acceptor restart", || {
        let ca = lock(&a.inner.profiles.data).catalog.clone();
        let cb = lock(&b.inner.profiles.data).catalog.clone();
        ca.as_ref().is_some_and(|c| {
            c.completed_joins.contains_key(&id) && c.default_profile().is_some_and(|p| p.id == source_default)
        }) && ca == cb
    })
    .await;
    let result = super::store::load(dirs[1].path(), group).unwrap().unwrap();
    assert!(!result.profiles.iter().any(|p| p.id == target_default));
    assert!(a.pending_profile_joins(b.id()).unwrap().is_empty());
    assert!(
        super::join::read(dirs[0].path(), &id)
            .unwrap()
            .unwrap()
            .catalog
            .is_none()
    );
    assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn acceptor_offline_before_merge_resends_durable_list_after_both_restart() {
    crate::test_watchdog::run(
        module_path!(),
        "offline acceptor profile handoff",
        Some(6),
        offline_acceptor_scenario(false),
    );
}
#[test]
fn committed_membership_recovers_profile_handoff_before_configuration_start() {
    crate::test_watchdog::run(
        module_path!(),
        "recover profile projection boundary",
        Some(6),
        offline_acceptor_scenario(true),
    );
}

#[test]
fn rapid_second_pairing_converges_three_real_nodes_without_import_proofs() {
    crate::test_watchdog::run(module_path!(), "rapid second profile pairing", Some(6), async {
        let dirs = [
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        ];
        let displays = [Displays::new(), Displays::new(), Displays::new()];
        let a = start(dirs[0].path(), displays[0].clone()).await;
        let b = start(dirs[1].path(), displays[1].clone()).await;
        let c = start(dirs[2].path(), displays[2].clone()).await;
        let source_default = query(&a).await.active_profile_id.unwrap();
        query(&b).await;
        query(&c).await;
        common::pair(&a, &b).await;
        // The new join contract assumes the initiator has its converged source
        // list. Pair again immediately when it has the acceptor's first result;
        // do not wait for every member's catalog/facts/UI readiness.
        common::until("initiator has the first acceptor result", || {
            lock(&a.inner.profiles.data).catalog.as_ref().is_some_and(|c| {
                c.group.0 == b.group_id().unwrap()
                    && !c.completed_joins.is_empty()
                    && c.default_profile().is_some_and(|p| p.id == source_default)
            })
        })
        .await;
        common::pair(&a, &c).await;
        common::active(&[&a, &b, &c]).await;
        common::until("rapid second result relays to every member", || {
            let catalogs = [&a, &b, &c].map(|n| lock(&n.inner.profiles.data).catalog.clone());
            catalogs[0].as_ref().is_some_and(|catalog| {
                catalog.group.0 == c.group_id().unwrap()
                    && catalog.default_profile().is_some_and(|p| p.id == source_default)
            }) && catalogs.iter().all(|catalog| catalog == &catalogs[0])
                && [&a, &b, &c]
                    .iter()
                    .all(|n| n.profile_snapshot().is_ok_and(|s| s.readiness == Readiness::Ready))
        })
        .await;
        for node in [&a, &b, &c] {
            assert_eq!(query(node).await.readiness, Readiness::Ready);
        }
        assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    });
}

#[test]
fn manual_layout_fixture_waits_for_target_catalog_after_screen_facts() {
    crate::test_watchdog::run(module_path!(), "manual fixture catalog readiness", Some(6), async {
        use crate::api::AgentApi;

        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let a = start(dirs[0].path(), Displays::new()).await;
        let b = start(dirs[1].path(), Displays::new()).await;
        // Hold both publication paths until after facts and input admission.
        // This forces the ordering that used to flake in diagnostics setup.
        a.inner.profiles.defer_catalogs.store(true, Ordering::SeqCst);
        b.inner.profiles.defer_joins.store(true, Ordering::SeqCst);
        common::pair(&a, &b).await;
        let nodes = [&a, &b];
        common::until("screen facts before target catalog", || {
            nodes.iter().all(|n| n.layout().0.screens.len() == 4)
                && a.profile_snapshot().is_ok_and(|s| s.readiness == Readiness::Syncing)
        })
        .await;
        // The command fence is intentional: a real caller must wait for ready
        // state rather than auto-saving target visibility into a source list.
        let node = a.clone();
        let peer = b.id().into();
        let error = tokio::task::spawn_blocking(move || node.set_screen_disabled(peer, true))
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(&error, crate::api::ApiError::InvalidState { message }
                if message == "Waiting for the target group catalog"),
            "{error:?}"
        );

        let mut ready = std::pin::pin!(common::editable_layouts(&nodes, 4));
        let observed =
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(ready.as_mut(), cx))).await;
        assert!(
            observed.is_pending(),
            "screen facts alone admitted a manual layout fixture"
        );

        a.inner.profiles.defer_catalogs.store(false, Ordering::SeqCst);
        b.inner.profiles.defer_joins.store(false, Ordering::SeqCst);
        ready.await;
        let node = a.clone();
        tokio::task::spawn_blocking(move || node.set_screen_disabled(peer, true))
            .await
            .unwrap()
            .unwrap();
        common::until("visibility edit after catalog publication", || {
            nodes.iter().all(|n| {
                n.link_config()
                    .unwrap()
                    .applied()
                    .owners
                    .get(&peer.0)
                    .is_some_and(|owner| !owner.screen_shared)
            })
        })
        .await;
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn awaiting_target_catalog_never_mutates_retained_source_profiles() {
    crate::test_watchdog::run(module_path!(), "awaiting catalog mutation fence", Some(6), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let displays = [Displays::new(), Displays::new()];
        let a = start(dirs[0].path(), displays[0].clone()).await;
        let b = start(dirs[1].path(), displays[1].clone()).await;
        let original = lock(&a.inner.profiles.data).catalog.clone().unwrap();
        a.inner.profiles.defer_catalogs.store(true, Ordering::SeqCst);
        b.inner.profiles.defer_joins.store(true, Ordering::SeqCst);
        common::pair(&a, &b).await;
        common::until("owner facts visible during pending catalog", || {
            a.profile_snapshot().is_ok()
        })
        .await;
        let current = query(&a).await;
        assert_eq!(current.readiness, Readiness::Syncing);
        assert!(
            !common::profile_layout_ready(&a),
            "retained source snapshot is not writable target readiness"
        );
        let n = a.clone();
        let reply = tokio::task::spawn_blocking(move || {
            n.layout_profile_command(Command::Rename(Rename {
                request_id: "rename-during-handoff".into(),
                expected_catalog_token: current.catalog_token,
                profile_id: current.active_profile_id.unwrap(),
                name: "Wrong group".into(),
            }))
        })
        .await
        .unwrap();
        assert_eq!(reply["error"]["code"], "busy", "{reply}");
        let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = executed.clone();
        let n = a.clone();
        let result = tokio::task::spawn_blocking(move || {
            n.commit_manual_visibility(n.id().into(), None, true, || {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            })
        })
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(!executed.load(Ordering::SeqCst));
        assert_eq!(
            super::store::load(dirs[0].path(), original.group).unwrap().unwrap(),
            original
        );
        assert!(
            super::store::load(dirs[0].path(), b.group_id().unwrap().into())
                .unwrap()
                .is_none()
        );
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn pairing_durable_catalog_does_not_require_live_owner_facts() {
    crate::test_watchdog::run(
        module_path!(),
        "profile handoff without live facts",
        Some(4),
        pairing_without_live_facts(false),
    );
}

#[test]
fn source_granted_pairing_does_not_require_live_owner_facts() {
    crate::test_watchdog::run(
        module_path!(),
        "source permit without live facts",
        Some(4),
        pairing_without_live_facts(true),
    );
}

async fn pairing_without_live_facts(source_permits: bool) {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let a = start(dirs[0].path(), Displays::new()).await;
    let b = start(dirs[1].path(), Displays::new()).await;
    // Only the smaller device ID grants the permit. Exercise both roles
    // explicitly: the pairing deadline must not depend on facts in either.
    let (a, b) = if (a.id() < b.id()) == source_permits {
        (a, b)
    } else {
        (b, a)
    };
    // Both saved catalogs already exist. Facts/UI readiness is independent
    // of the saved list and must not block the membership commit protocol.
    let original = lock(&a.inner.profiles.data).catalog.clone().unwrap();
    for node in [&a, &b] {
        assert!(
            super::store::load(&node.inner.data_dir, node.group_id().unwrap().into())
                .unwrap()
                .is_some()
        );
        node.inner
            .profiles
            .unavailable_observation
            .store(true, Ordering::SeqCst);
        assert_eq!(node.profile_snapshot().unwrap_err().code, ErrorCode::FactsUnavailable);
    }
    common::pair(&a, &b).await;
    common::until("saved catalog transferred without live observation", || {
        lock(&b.inner.profiles.data).catalog.as_ref().is_some_and(|c| {
            !c.completed_joins.is_empty()
                && c.default_profile().map(|p| &p.id) == original.default_profile().map(|p| &p.id)
        })
    })
    .await;
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn catalog_publication_cannot_be_overwritten_by_snapshot_initialization() {
    crate::test_watchdog::run(
        module_path!(),
        "catalog publication versus snapshot init",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let node = start(dir.path(), Displays::new()).await;
            let old = lock(&node.inner.profiles.data).catalog.clone().unwrap();
            let next = old
                .rename(node.id().into(), &old.active_profile_id, "Arrived catalog")
                .unwrap();
            let loaded = Arc::new(std::sync::Barrier::new(2));
            let resume = Arc::new(std::sync::Barrier::new(2));
            *lock(&node.inner.profiles.pause_snapshot_init) = Some((loaded.clone(), resume.clone()));
            lock(&node.inner.profiles.data).catalog = None;
            let reader = node.clone();
            let reading = std::thread::spawn(move || reader.profile_snapshot());
            // Snapshot initialization now holds profiles.data and has read the old
            // durable list, but has not yet written its recovered operation receipts.
            loaded.wait();
            let publishing = Arc::new(std::sync::Barrier::new(2));
            *lock(&node.inner.profiles.catalog_publish_ready) = Some(publishing.clone());
            let writer = node.clone();
            let candidate = next.clone();
            let writing = std::thread::spawn(move || writer.persist_catalog(candidate));
            publishing.wait();
            // In the broken ordering, the new catalog is already on disk while its
            // publisher waits for profiles.data; resuming recovery overwrites it.
            resume.wait();
            reading.join().unwrap().unwrap();
            writing.join().unwrap().unwrap();
            assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref(), Some(&next));
            assert_eq!(
                super::store::load(dir.path(), next.group).unwrap().as_ref(),
                Some(&next)
            );
            node.shutdown().await;
        },
    );
}

#[test]
fn hotplug_fixture_waits_for_handoff_even_when_provisional_catalogs_match() {
    crate::test_watchdog::run(module_path!(), "hotplug fixture handoff fence", Some(6), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let a = start(dirs[0].path(), Displays::new()).await;
        let b = start(dirs[1].path(), Displays::new()).await;
        let source_default = query(&a).await.active_profile_id.unwrap();
        b.inner.profiles.defer_joins.store(true, Ordering::SeqCst);
        common::pair(&a, &b).await;
        common::until("provisional catalogs match before handoff", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| {
                c.group.0 == b.group_id().unwrap() && c.profiles.len() == 1 && c.completed_joins.is_empty()
            }) && ca == cb
                && [&a, &b].iter().all(|n| common::profile_layout_ready(n))
        })
        .await;
        let nodes = [&a, &b];
        let mut handoff = std::pin::pin!(common::profile_handoff(&nodes, &source_default));
        let observed =
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(handoff.as_mut(), cx))).await;
        assert!(
            observed.is_pending(),
            "equal provisional catalogs are not a completed profile handoff"
        );
        b.inner.profiles.defer_joins.store(false, Ordering::SeqCst);
        handoff.await;
        for node in [&a, &b] {
            let catalog = lock(&node.inner.profiles.data).catalog.clone().unwrap();
            assert!(!catalog.completed_joins.is_empty());
            assert_eq!(catalog.active_profile_id, source_default);
        }
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn physical_topology_ignores_online_state_and_profile_restores_plug_unplug_once() {
    crate::test_watchdog::run(module_path!(), "physical topology restore", Some(6), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let da = Displays::new();
        let db = Displays::new();
        let a = start(dirs[0].path(), da.clone()).await;
        let b = start(dirs[1].path(), db.clone()).await;
        let source_default = query(&a).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &source_default).await;
        let n = a.clone();
        let peer = b.id();
        tokio::task::spawn_blocking(move || {
            n.commit_manual_layout(|layout| {
                layout.place_device(n.id(), Point::default(), 0.01, true);
                layout.place_device(peer, Point::new(2.0, 0.0), 0.01, true);
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();
        let state = query(&a).await;
        let _: LayoutProfilesState = call(
            &a,
            Request::CreateLayoutProfile(Create {
                request_id: "topology-save".into(),
                expected_token: state.token,
                name: "Physical desktop".into(),
            }),
        )
        .await;
        let saved = lock(&a.inner.profiles.data).catalog.clone().unwrap();
        assert!(saved.active().saved_topology.is_some());
        common::until("saved topology propagated", || {
            lock(&b.inner.profiles.data).catalog.as_ref() == Some(&saved)
        })
        .await;
        // Keep the second replica on the saved geometry while the writer
        // processes the excursion. Equal content alone must not count as the
        // restore echo: this replica still has the pre-hotplug register.
        b.link_config().unwrap().hold_incoming_config_states(true);
        let mut members = (*a.link_config().unwrap().members()).clone();
        let physical = super::topology::from_facts(&members);
        for member in &mut members.members {
            member.online = !member.online;
        }
        assert_eq!(super::topology::from_facts(&members), physical);

        let extra = NativeDisplay {
            key: "extra".into(),
            bounds: Rect::new(200., 0., 100., 100.),
            primary: false,
            mirror_of: None,
            mode: "extra-mode".into(),
        };
        lock(&da.state).displays.push(extra);
        da.changed();
        common::until("hotplug moves peer but not profile", || {
            let observed = a.observe_profiles().unwrap();
            observed
                .snapshot
                .native
                .iter()
                .any(|n| n.device.0 == a.id() && n.screens.len() == 3)
                && observed.snapshot.placements != saved.active().content.placements
        })
        .await;
        let n = a.clone();
        tokio::task::spawn_blocking(move || n.refresh_layout_profiles())
            .await
            .unwrap();
        assert_eq!(lock(&a.inner.profiles.data).catalog.as_ref(), Some(&saved));
        lock(&da.state).displays.pop();
        da.changed();
        common::until("saved placements restored on both replicas", || {
            [&a, &b].iter().all(|n| {
                n.observe_profiles()
                    .is_ok_and(|o| model::same_content(&o.snapshot, &saved.active().content))
            })
        })
        .await;
        assert_ne!(
            a.link_config().unwrap().applied().version,
            b.link_config().unwrap().applied().version,
            "matching saved geometry can precede the restore register"
        );
        b.link_config().unwrap().hold_incoming_config_states(false);
        common::until("restored content and register converged", || {
            let version = a.link_config().unwrap().applied().version.clone();
            [&a, &b].iter().all(|node| {
                node.link_config().unwrap().applied().version == version
                    && node.observe_profiles().is_ok_and(|o| {
                        o.layout_version == version && model::same_content(&o.snapshot, &saved.active().content)
                    })
            })
        })
        .await;
        let versions = [
            a.link_config().unwrap().applied().version.clone(),
            b.link_config().unwrap().applied().version.clone(),
        ];
        for _ in 0..4 {
            for node in [&a, &b] {
                let n = node.clone();
                tokio::task::spawn_blocking(move || n.refresh_layout_profiles())
                    .await
                    .unwrap();
                assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref(), Some(&saved));
            }
        }
        assert_eq!(a.link_config().unwrap().applied().version, versions[0]);
        assert_eq!(b.link_config().unwrap().applied().version, versions[1]);
        assert_eq!(da.calls.load(Ordering::SeqCst), 0);
        assert_eq!(db.calls.load(Ordering::SeqCst), 0);
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[test]
fn legacy_gap_travel_edit_autosaves_active_profile_and_physical_topology() {
    use crate::api::AgentApi;
    crate::test_watchdog::run(module_path!(), "legacy topology autosave", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let node = start(dir.path(), Displays::new()).await;
        let initial = query(&node).await;
        let _: LayoutProfilesState = call(
            &node,
            Request::CreateLayoutProfile(Create {
                request_id: "legacy-topology".into(),
                expected_token: initial.token,
                name: "Desktop".into(),
            }),
        )
        .await;
        let before = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        let gap_travel = !before.active().content.topology.gap_travel;
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            n.set_input_settings(serde_json::from_value(serde_json::json!({"gap_travel": gap_travel})).unwrap())
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(node.layout().0.settings.gap_travel, gap_travel);
        let saved = lock(&node.inner.profiles.data).catalog.clone().unwrap();
        assert_ne!(
            saved.active().content_revision,
            before.active().content_revision,
            "the legacy manual topology edit must save a new profile revision"
        );
        assert_eq!(saved.active().content.topology.gap_travel, gap_travel);
        assert_eq!(
            saved.active().saved_topology,
            node.observe_profiles().unwrap().physical_topology
        );
        assert_eq!(
            super::store::load(dir.path(), saved.group).unwrap().as_ref(),
            Some(&saved)
        );
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            n.set_input_settings(serde_json::from_value(serde_json::json!({"gap_travel": gap_travel})).unwrap())
                .unwrap();
            n.set_input_settings(serde_json::from_value(serde_json::json!({"cursor_trail": true})).unwrap())
                .unwrap();
        })
        .await
        .unwrap();
        assert_eq!(
            lock(&node.inner.profiles.data).catalog.as_ref(),
            Some(&saved),
            "an unchanged topology choice or unrelated input preference must not save again"
        );
        node.shutdown().await;
    });
}

#[test]
fn native_tokens_refresh_on_display_events_without_idle_polling_and_retry_failures() {
    crate::test_watchdog::run(module_path!(), "event-driven native tokens", Some(12), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        common::until("initial platform observation", || {
            node.inner.platform_ready.load(Ordering::Acquire)
        })
        .await;
        let token = || {
            node.link_config()
                .unwrap()
                .local_facts()
                .native_arrangement_token()
                .unwrap()
        };
        let initial = token();
        let reads = displays.reads.load(Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(4200)).await;
        assert_eq!(
            displays.reads.load(Ordering::SeqCst),
            reads,
            "idle nodes must not enumerate native configuration every two seconds"
        );

        // Mode identity can change while DisplayInfo bounds/pixels stay equal.
        lock(&displays.state).displays[0].mode.push_str("-changed");
        displays.changed();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while token() == initial {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("display callback must refresh the native token promptly");
        let next = token();
        let reads = displays.reads.load(Ordering::SeqCst);
        displays.read_fail.store(true, Ordering::SeqCst);
        lock(&displays.state).displays[0].mode.push_str("-recovered");
        displays.changed();
        common::until("failed native enumeration observed", || {
            displays.reads.load(Ordering::SeqCst) > reads
        })
        .await;
        displays.read_fail.store(false, Ordering::SeqCst);
        // Some platforms send no second notification after a transient error.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while token() == next {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("failed event read must retry without another callback");
        node.shutdown().await;
    });
}

#[test]
fn remote_hotplug_returns_to_the_manually_saved_profile_without_saving_repairs() {
    remote_hotplug_profile_return(false);
}

#[test]
fn remote_unplug_with_catalog_ahead_of_manual_layout_preserves_the_manual_placement() {
    remote_hotplug_profile_return(true);
}

fn remote_hotplug_profile_return(delay_manual_layout: bool) {
    use crate::api::AgentApi;
    use oink_protocol::v1::schema::WireDto;
    crate::test_watchdog::run(module_path!(), "remote hotplug profile return", Some(6), async {
        let dirs = [
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        ];
        let da = Displays::new();
        let db = Displays::new();
        lock(&da.state).displays.pop();
        lock(&db.state).displays.pop();
        let a = start(dirs[0].path(), da.clone()).await;
        let b = start(dirs[1].path(), db.clone()).await;
        // Force the old-register unplug to win the concurrent LWW tie. This
        // turns the release-gate timing failure into a deterministic ordering.
        let (a, b, da, db) = if delay_manual_layout && a.id() > b.id() {
            (b, a, db, da)
        } else {
            (a, b, da, db)
        };
        let source_default = query(&a).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &source_default).await;
        let c = start(dirs[2].path(), Displays::new()).await;
        common::pair(&a, &c).await;
        common::active(&[&a, &b, &c]).await;
        common::profile_handoff(&[&a, &b, &c], &source_default).await;
        let leaving = c.clone();
        tokio::task::spawn_blocking(move || leaving.leave_group())
            .await
            .unwrap()
            .unwrap();
        common::until("departed computer leaves the active roster", || {
            [&a, &b]
                .iter()
                .all(|n| n.link_config().unwrap().members().members.len() == 2)
        })
        .await;
        common::profile_handoff(&[&a, &b], &source_default).await;
        let peer_observation_settled = || {
            let token = model::hash(&*lock(&db.state));
            let version = a.link_config().unwrap().applied().version.clone();
            [&a, &b].iter().all(|node| {
                node.link_config().unwrap().applied().version == version
                    && node.observe_profiles().is_ok_and(|o| {
                        o.devices
                            .iter()
                            .any(|d| d.device.0 == b.id() && d.native_token.as_ref() == Some(&token))
                            && o.physical_topology
                                .as_ref()
                                .is_some_and(|t| super::topology::matches_native(t, &o.snapshot))
                    })
            })
        };
        common::until("initial native observations published", peer_observation_settled).await;
        let state = query(&a).await;
        let mut placements = state.snapshot.as_ref().unwrap().placements.clone();
        for p in &mut placements {
            p.scale = 0.01;
            p.origin = if p.device.0 == a.id() {
                Point::new(1.0, -1.0)
            } else {
                Point::default()
            };
        }
        // Use the same IPC manual edit as the canvas; no Create/reset to re-arm
        // the tracker after the last user edit.
        let edited: OperationReply = call(
            &a,
            Request::CommitArrangement(CommitRequest {
                request_id: "manual-before-remote-hotplug".into(),
                expected_token: state.token,
                placements,
                native_plan_tokens: vec![],
                disabled: vec![],
                screen_shared: vec![],
            }),
        )
        .await;
        assert_eq!(edited.state.operation.unwrap().status, OperationStatus::Applied);
        let expected = edited.state.snapshot.unwrap();
        let saved = lock(&a.inner.profiles.data).catalog.clone().unwrap();
        assert_eq!(saved.active().content_revision.writer.0, a.id());
        assert!(saved.active().saved_topology.is_some());
        // The real installation retained placements for several departed members.
        // Membership removal hides them from the applied layout, not the register.
        let stored = crate::config_module::group_store::load(
            &a.inner.data_dir.join(crate::config_module::group_store::STATE_FILE),
            a.id(),
        )
        .unwrap()
        .unwrap();
        let value =
            oink_protocol::v1::dto::LayoutValue::from_raw(stored.layout_register().unwrap().value().unwrap()).unwrap();
        let retained = crate::config_module::layout_wire::from_wire(&value).unwrap();
        assert!(retained.screens.iter().any(|s| s.id.device == c.id()));
        assert!(!a.layout().0.screens.iter().any(|s| s.id.device == c.id()));
        common::until("manual save propagated", || {
            lock(&b.inner.profiles.data).catalog.as_ref() == Some(&saved)
        })
        .await;
        let extra = NativeDisplay {
            key: "extra".into(),
            bounds: Rect::new(100.0, -100.0, 100.0, 100.0),
            primary: false,
            mirror_of: None,
            mode: "extra-mode".into(),
        };
        lock(&db.state).displays.push(extra.clone());
        db.changed();
        common::until("remote hotplug pushes the overlapping computer", || {
            a.observe_profiles().is_ok_and(|o| {
                o.snapshot
                    .native
                    .iter()
                    .any(|n| n.device.0 == b.id() && n.screens.len() == 2)
                    && o.snapshot.placements != expected.placements
            })
        })
        .await;
        // The kind-2 layout can arrive before its physical Facts. The logical
        // restore clock must observe a coherent departure before we unplug;
        // sleeping for the installed incident's duration cannot establish that.
        common::until(
            "remote hotplug native facts and layout converged",
            peer_observation_settled,
        )
        .await;
        let writer = a.clone();
        tokio::task::spawn_blocking(move || writer.refresh_layout_profiles())
            .await
            .unwrap();
        common::until("writer recorded the remote topology departure", || {
            // Probe a clone: returning to the saved topology must consume the
            // departure, without consuming the live tracker's pending return.
            lock(&a.inner.profiles.data)
                .restore_tracker
                .clone()
                .observe(saved.active(), saved.active().saved_topology.as_ref())
        })
        .await;
        for node in [&a, &b] {
            assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref(), Some(&saved));
            assert_eq!(
                node.link_config().unwrap().applied().layout_source,
                oink_protocol::v1::dto::LayoutSource::Default
            );
        }
        lock(&db.state).displays.pop();
        db.changed();
        common::until("remote unplug restores saved geometry on both replicas", || {
            // Matching content is insufficient when a replica still has an
            // earlier register. Finish this excursion before another manual edit.
            peer_observation_settled()
                && [&a, &b].iter().all(|n| {
                    n.observe_profiles()
                        .is_ok_and(|o| model::same_content(&o.snapshot, &expected))
                })
        })
        .await;
        for node in [&a, &b] {
            assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref(), Some(&saved));
        }

        // Save a new manual layout while plugged in. Unplugging now has no
        // matching saved topology: removing the only contact must keep the gap,
        // not attach the floating computer to a corner or compact either unit.
        // Layout publication and native-token/facts publication are separate;
        // finish the first excursion before starting another manual command.
        common::until("unplugged native observations published", peer_observation_settled).await;
        lock(&db.state).displays.push(extra);
        db.changed();
        common::until("second remote plug repaired", || {
            peer_observation_settled()
                && a.observe_profiles().is_ok_and(|o| {
                    o.snapshot
                        .native
                        .iter()
                        .any(|n| n.device.0 == b.id() && n.screens.len() == 2)
                        && o.snapshot.placements != expected.placements
                })
        })
        .await;
        let plugged = query(&a).await;
        let mut placements = plugged.snapshot.as_ref().unwrap().placements.clone();
        placements.iter_mut().find(|p| p.device.0 == a.id()).unwrap().origin = Point::new(1.0, -2.0);
        if delay_manual_layout {
            b.link_config().unwrap().hold_incoming_config_states(true);
        }
        let manual: OperationReply = call(
            &a,
            Request::CommitArrangement(CommitRequest {
                request_id: "manual-while-plugged".into(),
                expected_token: plugged.token,
                placements,
                native_plan_tokens: vec![],
                disabled: vec![],
                screen_shared: vec![],
            }),
        )
        .await;
        assert_eq!(manual.state.operation.unwrap().status, OperationStatus::Applied);
        let unmatched = manual.state.snapshot.unwrap();
        let saved_plugged = lock(&a.inner.profiles.data).catalog.clone().unwrap();
        assert_ne!(saved_plugged.active().content_revision, saved.active().content_revision);
        common::until("plugged manual save propagated", || {
            lock(&b.inner.profiles.data).catalog.as_ref() == Some(&saved_plugged)
        })
        .await;
        if delay_manual_layout {
            assert_ne!(
                b.observe_profiles().unwrap().snapshot.placements,
                unmatched.placements,
                "the saved catalog must arrive while its manual C0 layout is still held"
            );
        }
        lock(&db.state).displays.pop();
        db.changed();
        if delay_manual_layout {
            common::until(
                "unplug native observation reaches the owner before C0 is released",
                || {
                    b.link_config()
                        .unwrap()
                        .members()
                        .members
                        .iter()
                        .find(|m| m.is_self)
                        .and_then(|m| m.facts.as_ref())
                        .is_some_and(|f| f.displays().is_ok_and(|d| d.0.len() == 1))
                },
            )
            .await;
            b.link_config().unwrap().hold_incoming_config_states(false);
        }
        common::until("unmatched unplug observed on both replicas", || {
            [&a, &b].iter().all(|n| {
                n.observe_profiles().is_ok_and(|o| {
                    o.snapshot
                        .native
                        .iter()
                        .any(|n| n.device.0 == b.id() && n.screens.len() == 1)
                })
            })
        })
        .await;
        common::until(
            "unmatched unplug native facts and layout converged",
            peer_observation_settled,
        )
        .await;
        // Drive the ordinary logical observation directly, including repeated
        // ticks, instead of guessing when a background clock was scheduled.
        for _ in 0..4 {
            for node in [&a, &b] {
                let n = node.clone();
                tokio::task::spawn_blocking(move || n.refresh_layout_profiles())
                    .await
                    .unwrap();
                assert_eq!(
                    node.observe_profiles().unwrap().snapshot.placements,
                    unmatched.placements,
                    "unmatched unplug must preserve every remaining computer's placement"
                );
                assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref(), Some(&saved_plugged));
                assert_eq!(
                    node.link_config().unwrap().applied().layout_source,
                    oink_protocol::v1::dto::LayoutSource::Default
                );
            }
        }
        assert_eq!(da.calls.load(Ordering::SeqCst), 0);
        assert_eq!(db.calls.load(Ordering::SeqCst), 0);
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    });
}

#[path = "join_order_tests.rs"]
mod join_order_tests;
