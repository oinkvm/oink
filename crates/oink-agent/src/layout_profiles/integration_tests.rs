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
#[derive(Debug)]
struct Displays {
    state: Mutex<NativeConfiguration>,
    calls: AtomicUsize,
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
            fail: std::sync::atomic::AtomicBool::new(false),
        })
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
    fn watch(&self, _: ChangeHandler) -> std::result::Result<(), DisplayError> {
        Ok(())
    }
    fn native_configuration(&self) -> std::result::Result<NativeConfiguration, DisplayError> {
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
        let mut bits = current.disabled.clone();
        bits.iter_mut().find(|b| b.screen.display == "second").unwrap().disabled = true;
        let request = Request::CommitArrangement(CommitRequest {
            screen_shared: Vec::new(),
            request_id: "edit-once".into(),
            expected_token: state.token,
            placements: preview.placements,
            native_plan_tokens: vec![preview.plan_token],
            disabled: bits,
        });
        let edited: OperationReply = call(&node, request.clone()).await;
        assert_eq!(
            edited.state.operation.as_ref().unwrap().status,
            OperationStatus::Applied
        );
        assert_eq!(display.calls.load(Ordering::SeqCst), 1);
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
            ca.as_ref().is_some_and(|c| c.profiles.len() == 4) && ca == cb
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
        for i in 4..16 {
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
        common::pair(&a, &b).await;
        common::until("catalog import converged", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| c.profiles.len() == 2) && ca == cb
        })
        .await;
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
        let _: OperationReply = call(&restarted, Request::CommitArrangement(commit)).await;
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        restarted.shutdown().await;
    });
}

async fn merged_group_with_missing_source_catalog(pre_feature: bool) {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let displays = [Displays::new(), Displays::new()];
    let a = start(dirs[0].path(), displays[0].clone()).await;
    let b = start(dirs[1].path(), displays[1].clone()).await;
    query(&a).await;
    query(&b).await;
    common::pair(&a, &b).await;
    common::until("catalog merge completed", || {
        let ca = lock(&a.inner.profiles.data).catalog.clone();
        let cb = lock(&b.inner.profiles.data).catalog.clone();
        ca.as_ref()
            .is_some_and(|c| c.profiles.len() == 2 && !c.imported_merges.is_empty())
            && ca == cb
    })
    .await;
    let group = a.group_id().unwrap().into();
    a.shutdown().await;
    b.shutdown().await;
    if pre_feature {
        // Model an upgrade from a release that wrote the certified group journal
        // but never had layout-profile storage on either side.
        for dir in &dirs {
            std::fs::remove_dir_all(dir.path().join("layout-profiles-v2")).unwrap();
        }
    } else {
        // Keep the pre-merge profile-store origin, but lose the source archive
        // and its import receipt. This is a real certified, post-feature merge.
        let mut catalog = super::store::load(dirs[0].path(), group).unwrap().unwrap();
        catalog.imported_merges.clear();
        for entry in std::fs::read_dir(dirs[0].path().join("layout-profiles-v2")).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() != "activation.json" {
                std::fs::remove_file(path).unwrap();
            }
        }
        super::store::save(dirs[0].path(), &catalog).unwrap();
    }
    let restarted = start(dirs[0].path(), displays[0].clone()).await;
    let n = restarted.clone();
    tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match n.reconcile_profile_merges() {
                Err(e) if e.code == ErrorCode::Busy && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                result => break result,
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    let state = query(&restarted).await;
    assert_eq!(
        state.readiness,
        if pre_feature {
            Readiness::Ready
        } else {
            Readiness::Syncing
        }
    );
    assert_eq!(state.create_enabled, pre_feature);
    if pre_feature {
        let saved: LayoutProfilesState = call(
            &restarted,
            Request::CreateLayoutProfile(Create {
                request_id: "save-after-upgrade".into(),
                expected_token: state.token,
                name: "Upgraded desk".into(),
            }),
        )
        .await;
        assert_eq!(saved.profiles.len(), 2);
        assert_eq!(saved.readiness, Readiness::Ready);
    }
    assert_eq!(displays[0].calls.load(Ordering::SeqCst), 0);
    restarted.shutdown().await;
}

#[test]
fn pre_feature_merge_history_does_not_wait_for_nonexistent_catalogs() {
    crate::test_watchdog::run(
        module_path!(),
        "pre-feature merge history",
        Some(6),
        merged_group_with_missing_source_catalog(true),
    );
}
#[test]
fn post_feature_merge_missing_source_catalog_stays_syncing() {
    crate::test_watchdog::run(
        module_path!(),
        "pending post-feature catalog",
        Some(6),
        merged_group_with_missing_source_catalog(false),
    );
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
