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
    #[cfg(windows)]
    visibility: std::sync::atomic::AtomicBool,
    disabled: Mutex<Vec<String>>,
    #[cfg(windows)]
    returned: Mutex<Vec<String>>,
    state: Mutex<NativeConfiguration>,
    calls: AtomicUsize,
    reads: AtomicUsize,
    read_fail: std::sync::atomic::AtomicBool,
    watchers: Mutex<Vec<ChangeHandler>>,
    fail: std::sync::atomic::AtomicBool,
    virtual_display: std::sync::atomic::AtomicBool,
}
impl Displays {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            #[cfg(windows)]
            visibility: std::sync::atomic::AtomicBool::new(false),
            disabled: Mutex::new(Vec::new()),
            #[cfg(windows)]
            returned: Mutex::new(Vec::new()),
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
            virtual_display: std::sync::atomic::AtomicBool::new(false),
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
                is_virtual: self.virtual_display.load(Ordering::SeqCst),
            })
            .collect())
    }
    fn active_displays(&self) -> std::result::Result<Vec<DisplayInfo>, DisplayError> {
        let disabled = lock(&self.disabled);
        Ok(self
            .displays()?
            .into_iter()
            .filter(|d| !disabled.contains(&d.key))
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
    #[cfg(windows)]
    fn apply_native_layout(
        &self,
        expected: &NativeConfiguration,
        origins: &[NativeOrigin],
        bits: &[(String, bool)],
    ) -> std::result::Result<NativeConfiguration, DisplayError> {
        if !self.visibility.load(Ordering::SeqCst) {
            return if origins.is_empty() || expected.with_origins(origins)? == *expected {
                Ok(expected.clone())
            } else {
                self.apply_native_origins(expected, origins)
            };
        }
        let mut state = lock(&self.state);
        assert_eq!(&*state, expected);
        let mut hidden = lock(&self.disabled);
        let active = state
            .sources()
            .filter(|d| !hidden.contains(&d.key))
            .map(|d| d.key.clone())
            .collect::<Vec<_>>();
        let enabled = oink_display::arrangement::enabled_targets(&active, bits);
        let next_hidden = state
            .sources()
            .filter(|d| !enabled.contains(&d.key))
            .map(|d| d.key.clone())
            .collect::<Vec<_>>();
        let next = if origins.is_empty() {
            state.clone()
        } else {
            state.with_origins(origins)?
        };
        if *state != next || *hidden != next_hidden {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        *state = next;
        *hidden = next_hidden;
        Ok(state.clone())
    }
    #[cfg(windows)]
    fn release_disabled_displays(&self) -> std::result::Result<(), DisplayError> {
        lock(&self.disabled).clear();
        Ok(())
    }
    #[cfg(windows)]
    fn reconnected_displays(&self) -> std::result::Result<Vec<String>, DisplayError> {
        Ok(lock(&self.returned).clone())
    }
    #[cfg(windows)]
    fn acknowledge_reconnected(&self, keys: &[String]) -> std::result::Result<(), DisplayError> {
        lock(&self.returned).retain(|key| !keys.contains(key));
        lock(&self.disabled).retain(|key| !keys.contains(key));
        Ok(())
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
    if !dir.join(crate::config::SETTINGS_FILE).exists() {
        crate::config::AgentConfig::load_from_with_device_name(dir, "profiles-fixture".into())
            .unwrap()
            .update(|s| s.sync_system_arrangement = false)
            .unwrap();
    }
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
    diagnose(&node);
    let n = node.clone();
    tokio::task::spawn_blocking(move || n.refresh_native_arrangement())
        .await
        .unwrap()
        .unwrap();
    node
}
fn diagnose(node: &Node) {
    let weak = Arc::downgrade(&node.inner);
    common::watchdog::on_timeout(format!("layout fixture: {:?}", node.id()), move || {
        let Some(inner) = weak.upgrade() else {
            return "node stopped".into();
        };
        let node = Node { inner };
        let config = node.link_config().unwrap();
        let applied = config.applied();
        let profiles = match node.inner.profiles.data.try_lock() {
            Ok(data) => format!(
                "pending={:?}; catalog={:?}; last_readiness={:?}; operation={:?}",
                data.pending_layout,
                data.catalog.as_ref().map(|c| (
                    c.group,
                    &c.version,
                    &c.active_profile_id,
                    c.profiles.len(),
                    c.completed_joins.keys().collect::<Vec<_>>()
                )),
                data.last.as_ref().map(|s| s.readiness),
                data.operations.back().map(|op| (&op.id, op.status))
            ),
            Err(error) => format!("profile data unavailable: {error}"),
        };
        format!(
            "device={:?}; group={:?}; layout_version={:?}; screens={}; owners={:?}; members={:?}; {profiles}",
            node.id(),
            applied.group,
            applied.version,
            applied.layout.screens.len(),
            applied.owners.keys(),
            config
                .members()
                .members
                .iter()
                .map(|m| (m.device, m.online, m.facts.is_some()))
                .collect::<Vec<_>>()
        )
    });
}

async fn call<T: serde::de::DeserializeOwned>(node: &Node, request: Request) -> T {
    let diagnostic = format!("{request:?}");
    let changes = matches!(
        &request,
        Request::LoadLayoutProfile(_)
            | Request::CommitArrangement(_)
            | Request::ResetArrangement(_)
            | Request::ReplayArrangement(_)
    )
    .then(|| common::StateChanges::new(&[node], &diagnostic));
    let n = node.clone();
    let reply = tokio::task::spawn_blocking(move || dispatch(&n, request))
        .await
        .unwrap()
        .unwrap();
    let mut value = serde_json::to_value(reply).unwrap();
    // Mutation acceptance is asynchronous; test the final authoritative event
    // state rather than assuming the IPC reply completed the operation.
    if let Some(id) = value["operation_id"].as_str().map(str::to_owned) {
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
            changes.as_ref().expect("mutation subscription").notified().await;
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
        assert_eq!(logs.matches("call_site=\"").count(), 3, "{logs}");
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
        let expected_default = query(&b).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &expected_default).await;
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
fn native_screen_preview_translates_whole_device_when_offset_is_disallowed() {
    crate::test_watchdog::run(module_path!(), "native preview rigid fallback", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        enable(&node, node.id().into(), "enable-native-edit").await;
        let state = query(&node).await;
        let current = state.snapshot.as_ref().unwrap();
        let placement = current
            .placements
            .iter()
            .find(|p| p.device == node.id().into())
            .unwrap();
        let desired = Point::new(placement.origin.x - 50.0 * placement.scale, placement.origin.y);
        let preview: PreviewReply = call(
            &node,
            Request::PreviewSystemArrangement(PreviewRequest {
                sequence: "rigid-1".into(),
                expected_token: state.token,
                device: node.id().into(),
                moved_screen: crate::api::ScreenKey {
                    device: node.id().into(),
                    display: "second".into(),
                },
                desired_canvas_origin: desired,
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
            Point::new(100.0, 0.0),
            "the rejected internal offset must leave native sibling geometry unchanged"
        );
        let moved = preview
            .placements
            .iter()
            .find(|p| p.device == node.id().into())
            .unwrap();
        assert_eq!(
            moved.origin,
            Point::new(placement.origin.x - 150.0 * placement.scale, placement.origin.y)
        );
        assert!(preview.adjustments.is_empty());
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
        let changed = common::StateChanges::new(&[&node], "hotkey native load");
        let n = node.clone();
        let target = default.clone();
        tokio::task::spawn_blocking(move || n.on_hotkey(&format!("switch_layout:{target}")))
            .await
            .unwrap();
        loop {
            let state = query(&node).await;
            if state.active_profile_id == Some(default.clone())
                && state
                    .operation
                    .as_ref()
                    .is_some_and(|o| o.status != OperationStatus::Running)
            {
                break;
            }
            changed.notified().await;
        }
        assert_eq!(display.calls.load(Ordering::SeqCst), 4);
        let before = query(&node).await;
        let n = node.clone();
        let target = default.clone();
        tokio::task::spawn_blocking(move || n.on_hotkey(&format!("switch_layout:{target}")))
            .await
            .unwrap();
        assert_eq!(query(&node).await.operation, before.operation);
        assert_eq!(display.calls.load(Ordering::SeqCst), 4);
        node.shutdown().await;
        let restarted = start(dir.path(), display.clone()).await;
        let restored = query(&restarted).await;
        assert_eq!(restored.active_profile_id, Some(default));
        assert_eq!(restored.profiles.len(), 3);
        assert_eq!(display.calls.load(Ordering::SeqCst), 4);
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
        common::converge(&[&a, &b], "profile catalogs union and converge", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| c.profiles.len() == 2) && ca == cb
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
        common::converge(&[&a, &b], "renamed catalog reaches peer", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca == cb
        })
        .await;
        // Exercise the larger catalog over the real Low transport, then deletion.
        for i in 2..16 {
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
        common::converge(&[&a, &b], "sixteen profiles reach peer", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| c.profiles.len() == 16) && ca == cb
        })
        .await;
        let state = query(&a).await;
        assert!(!state.create_enabled);
        let removed = state.active_profile_id.clone().unwrap();
        let binding = crate::api::Hotkey {
            action: crate::api::HotkeyAction::SwitchLayout,
            device: None,
            profile: Some(removed.clone()),
            keys: vec!["Ctrl".into(), "Shift".into(), "F9".into()],
            enabled: true,
        };
        // This local choice must never travel with the shared catalog.
        b.update_settings(|s| s.hotkeys = Some(vec![binding.clone()])).unwrap();
        assert!(!a.effective_hotkeys().iter().any(|h| h.profile == Some(removed.clone())));
        let mut local_binding = binding.clone();
        local_binding.keys[2] = "F10".into();
        a.update_settings(|s| s.hotkeys = Some(vec![local_binding.clone()]))
            .unwrap();
        let _: LayoutProfilesState = call(
            &a,
            Request::DeleteLayoutProfile(Delete {
                request_id: "delete-current".into(),
                expected_catalog_token: state.catalog_token,
                profile_id: state.active_profile_id.unwrap(),
            }),
        )
        .await;
        common::converge(&[&a, &b], "deleted profile reaches peer", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| c.profiles.len() == 15) && ca == cb
        })
        .await;
        assert!(!a.settings().hotkeys.unwrap().contains(&local_binding));
        assert!(!b.settings().hotkeys.unwrap().contains(&binding));
        assert!(
            !crate::config::AgentConfig::load_from(dirs[1].path())
                .unwrap()
                .settings
                .hotkeys
                .unwrap()
                .contains(&binding)
        );
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
        use oink_protocol::v2::RawValue;
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
        let expected_default = query(&b).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &expected_default).await;
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
        common::converge(&[&a, &b], "observed native changes converge", || {
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
        common::converge(&[&restarted], "pending result resolves through status polling", || {
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
                oink_protocol::v2::dto::LayoutSource::Default,
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
                oink_protocol::v2::dto::LayoutSource::Manual,
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
                        oink_protocol::v2::dto::LayoutSource::Default,
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

#[test]
fn multi_group_profile_overflow_completes_handoff_without_pending_joins() {
    crate::test_watchdog::run(module_path!(), "profile overflow handoff", Some(10), async {
        let dirs = (0..4).map(|_| tempfile::tempdir().unwrap()).collect::<Vec<_>>();
        let displays = (0..4).map(|_| Displays::new()).collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for i in 0..4 {
            nodes.push(start(dirs[i].path(), displays[i].clone()).await);
            query(&nodes[i]).await;
        }
        for first in [0, 2] {
            common::pair(&nodes[first + 1], &nodes[first]).await;
            common::converge(
                &[&nodes[first], &nodes[first + 1]],
                "original group profile handoff completes",
                || {
                    let a = lock(&nodes[first].inner.profiles.data).catalog.clone();
                    let b = lock(&nodes[first + 1].inner.profiles.data).catalog.clone();
                    a.as_ref().is_some_and(|c| c.completed_joins.len() == 1)
                        && a == b
                        && [&nodes[first], &nodes[first + 1]]
                            .iter()
                            .all(|n| common::profile_layout_ready(n))
                },
            )
            .await;
            let profiles = query(&nodes[first]).await.profiles.len();
            for i in profiles..16 {
                let state = query(&nodes[first]).await;
                let _: LayoutProfilesState = call(
                    &nodes[first],
                    Request::CreateLayoutProfile(Create {
                        request_id: format!("overflow-{first}-{i}"),
                        expected_token: state.token,
                        name: format!("Desk {first}-{i}"),
                    }),
                )
                .await;
            }
            common::converge(
                &[&nodes[first], &nodes[first + 1]],
                "full original catalog converges",
                || {
                    let a = lock(&nodes[first].inner.profiles.data).catalog.clone();
                    let b = lock(&nodes[first + 1].inner.profiles.data).catalog.clone();
                    a.as_ref().is_some_and(|c| c.profiles.len() == 16) && a == b
                },
            )
            .await;
        }
        let source_default = lock(&nodes[0].inner.profiles.data)
            .catalog
            .as_ref()
            .unwrap()
            .default_profile()
            .unwrap()
            .id
            .clone();
        let target_active = query(&nodes[2]).await.active_profile_id.unwrap();
        common::pair(&nodes[0], &nodes[2]).await;
        common::converge(
            &nodes.iter().collect::<Vec<_>>(),
            "overflow completes instead of remaining pending",
            || {
                let catalogs = nodes
                    .iter()
                    .map(|n| lock(&n.inner.profiles.data).catalog.clone())
                    .collect::<Vec<_>>();
                catalogs[0].as_ref().is_some_and(|c| {
                    c.profiles.len() == 16
                        && c.active_profile_id == target_active
                        && c.default_profile().is_some_and(|p| p.id == source_default)
                        && c.completed_joins.len() == 2
                }) && catalogs.iter().all(|c| c == &catalogs[0])
                    && nodes[0].pending_profile_joins(nodes[2].id()).unwrap().is_empty()
                    && nodes.iter().all(common::profile_layout_ready)
            },
        )
        .await;
        for node in &nodes {
            let state = query(node).await;
            assert_ne!(state.readiness, Readiness::Syncing);
            let catalog = lock(&node.inner.profiles.data).catalog.clone().unwrap();
            assert!(catalog.profiles.iter().all(|p| p.content.placements.len() == 4));
            assert_eq!(
                super::store::load(&node.inner.data_dir, catalog.group)
                    .unwrap()
                    .as_ref(),
                Some(&catalog)
            );
        }
        assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
        for node in nodes {
            node.shutdown().await;
        }
    });
}

#[test]
fn empty_singleton_joins_three_device_group_without_losing_saved_default_over_low() {
    crate::test_watchdog::run(module_path!(), "empty singleton profile import", Some(8), async {
        let dirs = (0..4).map(|_| tempfile::tempdir().unwrap()).collect::<Vec<_>>();
        let displays = (0..4).map(|_| Displays::new()).collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for i in 0..4 {
            nodes.push(start(dirs[i].path(), displays[i].clone()).await);
            query(&nodes[i]).await;
        }
        // Model a singleton whose saved default was created without screen facts,
        // even though its live desktop has since become available.
        let node = nodes[3].clone();
        tokio::task::spawn_blocking(move || {
            let _gate = node.inner.profiles.gate.lock().unwrap();
            let empty = ArrangementSnapshot {
                placements: vec![],
                disabled: vec![],
                native: vec![],
                screen_shared: vec![],
                topology: Default::default(),
                active_profile_id: String::new(),
            };
            let catalog = model::Catalog::initial(
                node.group_id().unwrap().into(),
                node.id().into(),
                &empty,
                model::DeviceState {
                    online_devices: Default::default(),
                    connected_screens: Default::default(),
                },
                1,
            )
            .unwrap();
            node.persist_catalog(catalog).unwrap();
        })
        .await
        .unwrap();
        let initial_default = query(&nodes[1]).await.active_profile_id.unwrap();
        common::pair(&nodes[0], &nodes[1]).await;
        common::profile_handoff(&[&nodes[0], &nodes[1]], &initial_default).await;
        common::pair(&nodes[0], &nodes[2]).await;
        common::profile_handoff(&[&nodes[0], &nodes[1], &nodes[2]], &initial_default).await;
        common::editable_layouts(&[&nodes[0], &nodes[1], &nodes[2]], 6).await;
        let target_group = nodes[2].group_id().unwrap();
        let live = query(&nodes[2]).await.snapshot.unwrap();
        let node = nodes[2].clone();
        let saved = tokio::task::spawn_blocking(move || {
            let _gate = node.inner.profiles.gate.lock().unwrap();
            let catalog = lock(&node.inner.profiles.data).catalog.clone().unwrap();
            let next = catalog
                .save(node.id().into(), &live, catalog.active().match_state.clone(), None, 497)
                .unwrap();
            let saved = next.active().clone();
            node.persist_catalog(next).unwrap();
            saved
        })
        .await
        .unwrap();
        common::converge(
            &nodes[..3].iter().collect::<Vec<_>>(),
            "saved three-device default converges",
            || {
                nodes[..3].iter().all(|n| {
                    lock(&n.inner.profiles.data)
                        .catalog
                        .as_ref()
                        .is_some_and(|c| c.profile(&saved.id) == Ok(&saved))
                })
            },
        )
        .await;
        let singleton_default = query(&nodes[3]).await.active_profile_id.unwrap();
        let before = query(&nodes[2]).await.snapshot.unwrap();
        assert!(
            lock(&nodes[3].inner.profiles.data)
                .catalog
                .as_ref()
                .unwrap()
                .active()
                .content
                .placements
                .is_empty()
        );
        common::pair(&nodes[3], &nodes[2]).await;
        common::profile_handoff(&nodes.iter().collect::<Vec<_>>(), &saved.id).await;
        let result = lock(&nodes[2].inner.profiles.data).catalog.clone().unwrap();
        assert_eq!(result.version.writer.0, nodes[2].id());
        assert!(result.profile(&singleton_default).is_err());
        let preserved = result.profile(&saved.id).unwrap();
        for old in &saved.content.placements {
            assert!(preserved.content.placements.contains(old));
        }
        for old in &saved.content.disabled {
            assert!(preserved.content.disabled.contains(old));
        }
        assert!(preserved.content.placements.iter().any(|p| p.device.0 == nodes[3].id()));
        assert!(
            preserved
                .content
                .disabled
                .iter()
                .filter(|d| d.screen.device.0 == nodes[3].id())
                .all(|d| !d.disabled)
        );
        assert!(preserved.content_revision > saved.content_revision);
        assert_eq!(preserved.last_used, saved.last_used);
        assert_eq!(preserved.default_group, Some(result.group));
        for node in &nodes {
            assert_eq!(node.group_id(), Some(target_group));
            assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref(), Some(&result));
        }
        let after = query(&nodes[2]).await.snapshot.unwrap();
        for placement in before.placements {
            assert!(after.placements.contains(&placement));
        }
        assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
        for node in nodes {
            node.shutdown().await;
        }
    });
}

#[test]
fn fresh_virtual_screen_is_visible_after_singleton_pairing_in_both_directions() {
    crate::test_watchdog::run(module_path!(), "fresh virtual screen pairing", Some(4), async {
        for reverse in [false, true] {
            let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
            let displays = [Displays::new(), Displays::new()];
            displays[0].virtual_display.store(true, Ordering::SeqCst);
            let a = start(dirs[0].path(), displays[0].clone()).await;
            let b = start(dirs[1].path(), displays[1].clone()).await;
            let (source, target) = if reverse { (&b, &a) } else { (&a, &b) };
            let default = query(target).await.active_profile_id.unwrap();
            common::pair(source, target).await;
            common::profile_handoff(&[&a, &b], &default).await;
            for node in [&a, &b] {
                let layout: crate::api::Layout = call(node, Request::Layout).await;
                assert!(layout.geometry.screens.iter().any(|s| s.key.device.0 == a.id()));
                assert!(
                    layout.geometry.screens.iter().all(|s| !s.hidden && !s.device_disabled),
                    "a freshly enumerated screen must not require manual Show after pairing"
                );
            }
            a.shutdown().await;
            b.shutdown().await;
        }
    });
}

#[test]
fn new_virtual_screen_hotplug_is_visible_and_explicit_hidden_choice_survives_refresh() {
    crate::test_watchdog::run(module_path!(), "virtual screen hotplug visibility", Some(2), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        let node = start(dir.path(), displays.clone()).await;
        query(&node).await;
        displays.virtual_display.store(true, Ordering::SeqCst);
        lock(&displays.state).displays.push(NativeDisplay {
            key: "third".into(),
            bounds: Rect::new(200.0, 0.0, 100.0, 100.0),
            primary: false,
            mirror_of: None,
            mode: "mode-c".into(),
        });
        displays.changed();
        common::converge(&[&node], "hotplugged virtual screen", || {
            node.layout().0.screens.len() == 3
        })
        .await;
        let current: crate::api::Layout = call(&node, Request::Layout).await;
        assert!(
            !current
                .geometry
                .screens
                .iter()
                .find(|s| s.key.display == "third")
                .unwrap()
                .hidden,
            "a new virtual screen must not be auto-hidden"
        );
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: crate::api::ScreenKey {
                    device: node.id().into(),
                    display: "third".into(),
                },
                hidden: true,
            },
        )
        .await;
        displays.virtual_display.store(false, Ordering::SeqCst);
        displays.changed();
        let current: crate::api::Layout = call(&node, Request::Layout).await;
        assert!(
            current
                .geometry
                .screens
                .iter()
                .find(|s| s.key.display == "third")
                .unwrap()
                .hidden
        );
        node.shutdown().await;
    });
}

/// Membership and the saved catalog can converge before every owner has been
/// admitted and published its visibility. Ready also permits cached offline
/// owners, so it alone cannot fence the online assertions below. Subscribe
/// before observing, coalesce real state changes, and keep the exact snapshots
/// that met all conditions. The scenario watchdog bounds a missing event or
/// permanent non-convergence independently of machine scheduling speed.
async fn visibility_layouts(nodes: &[Node], screen_count: usize, what: &str) -> Vec<crate::api::Layout> {
    let refs = nodes.iter().collect::<Vec<_>>();
    let changes = common::StateChanges::new(&refs, what);
    loop {
        let nodes = nodes.to_vec();
        let observed = tokio::task::spawn_blocking(move || {
            nodes
                .iter()
                .map(|node| (node.profile_snapshot(), crate::agent_api::layout_api(node).unwrap()))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        if observed.iter().all(|(profile, layout)| {
            profile
                .as_ref()
                .is_ok_and(|state| state.readiness == Readiness::Ready && state.snapshot.is_some())
                && layout.geometry.screens.len() == screen_count
                && layout.geometry.screens.iter().all(|screen| screen.online)
        }) {
            break observed.into_iter().map(|(_, layout)| layout).collect();
        }
        // notify_one retains a permit when publication races the observation.
        changes.notified().await;
    }
}

async fn pairing_visibility_matrix(source_count: usize, target_count: usize) {
    for selection in 0..4 {
        let count = source_count + target_count;
        let dirs = (0..count).map(|_| tempfile::tempdir().unwrap()).collect::<Vec<_>>();
        let displays = (0..count).map(|_| Displays::new()).collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for i in 0..count {
            nodes.push(start(dirs[i].path(), displays[i].clone()).await);
            query(&nodes[i]).await;
        }
        for range in [0..source_count, source_count..count] {
            let first = &nodes[range.start];
            let default = query(&nodes[range.start + usize::from(range.len() > 1)])
                .await
                .active_profile_id
                .unwrap();
            for i in range.clone().skip(1) {
                common::pair(first, &nodes[i]).await;
                common::profile_handoff(&nodes[range.start..=i].iter().collect::<Vec<_>>(), &default).await;
            }
            common::converge(
                &nodes[range.clone()].iter().collect::<Vec<_>>(),
                &format!(
                    "visibility {source_count}+{target_count} selection {selection} original group {}",
                    range.start
                ),
                || {
                    nodes[range.clone()]
                        .iter()
                        .all(|n| n.layout().0.screens.len() == range.len() * 2 && common::profile_layout_ready(n))
                },
            )
            .await;
            let source = range.start == 0;
            let custom = selection & if source { 1 } else { 2 } != 0;
            if custom {
                let state = query(first).await;
                let _: LayoutProfilesState = call(
                    first,
                    Request::CreateLayoutProfile(Create {
                        request_id: format!("custom-{selection}-{source}"),
                        expected_token: state.token,
                        name: if source { "Source desk" } else { "Target desk" }.into(),
                    }),
                )
                .await;
                // Only this explicitly hidden screen may stay hidden after pairing.
                let _: serde_json::Value = call(
                    first,
                    Request::SetScreenHidden {
                        screen: crate::api::ScreenKey {
                            device: first.id().into(),
                            display: "second".into(),
                        },
                        hidden: true,
                    },
                )
                .await;
            }
        }
        let before_source: crate::api::Layout = call(&nodes[0], Request::Layout).await;
        let before_target: crate::api::Layout = call(&nodes[source_count], Request::Layout).await;
        let completed_joins = lock(&nodes[source_count].inner.profiles.data)
            .catalog
            .as_ref()
            .unwrap()
            .completed_joins
            .len();
        let target_group = nodes[source_count].group_id().unwrap();
        common::pair(&nodes[0], &nodes[source_count]).await;
        common::converge(
            &nodes.iter().collect::<Vec<_>>(),
            "visibility matrix catalog handoff",
            || {
                let catalogs = nodes
                    .iter()
                    .map(|n| lock(&n.inner.profiles.data).catalog.clone())
                    .collect::<Vec<_>>();
                catalogs.iter().all(|c| c == &catalogs[0])
                    && catalogs[0]
                        .as_ref()
                        .is_some_and(|c| c.group.0 == target_group && c.completed_joins.len() == completed_joins + 1)
            },
        )
        .await;
        let after_layouts = visibility_layouts(
            &nodes,
            count * 2,
            &format!("visibility {source_count}+{target_count} selection {selection} merged layout"),
        )
        .await;
        // The completed handoff may perform a later global edit while adding
        // the second singleton. Use the final register writer to identify the
        // authoritative visibility snapshot for this convergence assertion.
        let writer = nodes[0]
            .link_config()
            .unwrap()
            .applied()
            .version
            .writer()
            .unwrap()
            .ordering_key()
            .unwrap();
        let source_key = oink_network::wire_identity::device(nodes[0].id())
            .ordering_key()
            .unwrap();
        let winner = if writer == source_key {
            &before_source
        } else {
            &before_target
        };
        for after in after_layouts {
            for before in before_source
                .geometry
                .screens
                .iter()
                .chain(&before_target.geometry.screens)
            {
                let screen = after.geometry.screens.iter().find(|s| s.key == before.key).unwrap();
                let expected_hidden = winner
                    .geometry
                    .screens
                    .iter()
                    .find(|candidate| candidate.key == before.key)
                    .map(|candidate| candidate.hidden)
                    .unwrap_or(before.hidden);
                assert_eq!(
                    screen.hidden, expected_hidden,
                    "pairing did not apply the global visibility winner: selection={selection} source_count={source_count} target_count={target_count} screen={:?} actual={} expected={}",
                    before.key, screen.hidden, expected_hidden,
                );
                assert_eq!(screen.device_disabled, before.device_disabled);
                assert!(screen.online);
            }
            let visible = after.geometry.screens.iter().filter(|s| !s.hidden).collect::<Vec<_>>();
            for (i, a) in visible.iter().enumerate() {
                for b in visible.iter().skip(i + 1).filter(|b| a.key.device != b.key.device) {
                    let (a, b) = (a.rect(), b.rect());
                    assert!(
                        a.right() <= b.x || b.right() <= a.x || a.bottom() <= b.y || b.bottom() <= a.y,
                        "pairing failed to arrange both groups"
                    );
                }
            }
        }
        assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
        for node in nodes {
            node.shutdown().await;
        }
    }
}

fn pairing_visibility_gate() -> &'static Mutex<()> {
    static GATE: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

#[test]
fn pairing_visibility_singleton_singleton_defaults_and_custom_profiles() {
    // Queue before the scenario watchdog: another matrix owns its own budget.
    let _gate = pairing_visibility_gate().lock().unwrap();
    crate::test_watchdog::run(
        module_path!(),
        "singleton visibility matrix",
        Some(4),
        pairing_visibility_matrix(1, 1),
    );
}

#[test]
fn pairing_visibility_group_singleton_defaults_and_custom_profiles() {
    // Queue before the scenario watchdog: another matrix owns its own budget.
    let _gate = pairing_visibility_gate().lock().unwrap();
    crate::test_watchdog::run(
        module_path!(),
        "group singleton visibility matrix",
        Some(8),
        pairing_visibility_matrix(1, 3),
    );
}

#[test]
fn pairing_visibility_group_group_defaults_and_custom_profiles() {
    // Queue before the scenario watchdog: another matrix owns its own budget.
    let _gate = pairing_visibility_gate().lock().unwrap();
    crate::test_watchdog::run(
        module_path!(),
        "group group visibility matrix",
        Some(8),
        pairing_visibility_matrix(2, 2),
    );
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
    let source_default = lock(&nodes[usize::from(source_members == 2)].inner.profiles.data)
        .catalog
        .as_ref()
        .unwrap()
        .active_profile_id
        .clone();
    if source_members == 2 {
        common::pair(&nodes[0], &nodes[1]).await;
        common::converge(&[&nodes[0], &nodes[1]], "source group default converges", || {
            let a = lock(&nodes[0].inner.profiles.data).catalog.clone();
            let b = lock(&nodes[1].inner.profiles.data).catalog.clone();
            a.as_ref()
                .is_some_and(|c| c.profiles.len() == 1 && c.active_profile_id == source_default)
                && a == b
        })
        .await;
    }
    let target_default = lock(&nodes[source_members + 1].inner.profiles.data)
        .catalog
        .as_ref()
        .unwrap()
        .active_profile_id
        .clone();
    common::pair(&nodes[source_members], &nodes[source_members + 1]).await;
    common::converge(
        &[&nodes[source_members], &nodes[source_members + 1]],
        "target group inherited default converges",
        || {
            let a = lock(&nodes[source_members].inner.profiles.data).catalog.clone();
            let b = lock(&nodes[source_members + 1].inner.profiles.data).catalog.clone();
            a.as_ref()
                .is_some_and(|c| c.profiles.len() == 1 && c.active_profile_id == target_default)
                && a == b
        },
    )
    .await;
    let target_group = nodes[source_members + 1].group_id().unwrap();
    let expected_default = if source_members == 1 {
        target_default.clone()
    } else {
        source_default.clone()
    };
    let expected_profiles = if source_members == 1 { 1 } else { 2 };
    // The responder accepts; the initiating source joins its existing group.
    common::pair(&nodes[0], &nodes[source_members + 1]).await;
    common::converge(
        &nodes.iter().collect::<Vec<_>>(),
        "directional default rule converges on all members",
        || {
            let catalogs = nodes
                .iter()
                .map(|n| lock(&n.inner.profiles.data).catalog.clone())
                .collect::<Vec<_>>();
            catalogs.iter().all(|c| c == &catalogs[0])
                && catalogs[0].as_ref().is_some_and(|c| {
                    c.group.0 == target_group
                        && c.profiles.len() == expected_profiles
                        && c.active_profile_id == expected_default
                        && c.default_profile().is_some_and(|p| p.id == expected_default)
                        && (source_members == 1
                            || c.completed_joins
                                .values()
                                .any(|j| j.dropped_defaults.contains(&target_default)))
                })
        },
    )
    .await;
    assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
    for node in nodes {
        node.shutdown().await;
    }
}

#[test]
fn group_group_pairing_preserves_acceptors_inherited_default_over_low() {
    crate::test_watchdog::run(
        module_path!(),
        "two groups default import",
        Some(8),
        directional_default_join(2),
    );
}

#[test]
fn single_device_pairing_preserves_acceptor_group_default_over_low() {
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
        use oink_protocol::v2::{RawValue as V, cbor, schema::WireValue};
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
    common::converge(
        &[&a, &b],
        "retained source list completes after acceptor restart",
        || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| {
                c.completed_joins.contains_key(&id) && c.default_profile().is_some_and(|p| p.id == target_default)
            }) && ca == cb
        },
    )
    .await;
    let result = super::store::load(dirs[1].path(), group).unwrap().unwrap();
    assert!(!result.profiles.iter().any(|p| p.id == source_default));
    assert_eq!(result.active_profile_id, target_default);
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
        let expected_default = query(&b).await.active_profile_id.unwrap();
        query(&a).await;
        query(&c).await;
        common::pair(&a, &b).await;
        // The new join contract assumes the initiator has its converged source
        // list. Pair again immediately when it has the acceptor's first result;
        // do not wait for every member's catalog/facts/UI readiness.
        common::converge(&[&a], "initiator has the first acceptor result", || {
            lock(&a.inner.profiles.data).catalog.as_ref().is_some_and(|c| {
                c.group.0 == b.group_id().unwrap()
                    && !c.completed_joins.is_empty()
                    && c.default_profile().is_some_and(|p| p.id == expected_default)
            })
        })
        .await;
        common::pair(&a, &c).await;
        common::active(&[&a, &b, &c]).await;
        common::converge(&[&a, &b, &c], "rapid second result relays to every member", || {
            let catalogs = [&a, &b, &c].map(|n| lock(&n.inner.profiles.data).catalog.clone());
            catalogs[0].as_ref().is_some_and(|catalog| {
                catalog.group.0 == c.group_id().unwrap()
                    && catalog.default_profile().is_some_and(|p| p.id == expected_default)
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
        common::converge(&nodes, "screen facts before target catalog", || {
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
        common::converge(&nodes, "visibility edit after catalog publication", || {
            nodes.iter().all(|n| {
                n.link_config()
                    .unwrap()
                    .applied()
                    .layout
                    .screens
                    .iter()
                    .filter(|screen| screen.id.device == peer.0)
                    .all(|screen| screen.device_hidden)
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
        common::converge(&[&a, &b], "owner facts visible during pending catalog", || {
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
    let original = lock(&b.inner.profiles.data).catalog.clone().unwrap();
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
    // Complete the join only after pairing's state events. Without live facts a
    // completed join publishes no profile event, so poll the catalog instead.
    b.inner.profiles.defer_joins.store(true, Ordering::SeqCst);
    common::pair(&a, &b).await;
    common::until("acceptor interrupted before catalog commit", || {
        b.inner.profiles.deferred_joins.load(Ordering::SeqCst) > 0
    })
    .await;
    b.inner.profiles.defer_joins.store(false, Ordering::SeqCst);
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
        let expected_default = query(&b).await.active_profile_id.unwrap();
        b.inner.profiles.defer_joins.store(true, Ordering::SeqCst);
        common::pair(&a, &b).await;
        common::converge(&[&a, &b], "provisional catalogs match before handoff", || {
            let ca = lock(&a.inner.profiles.data).catalog.clone();
            let cb = lock(&b.inner.profiles.data).catalog.clone();
            ca.as_ref().is_some_and(|c| {
                c.group.0 == b.group_id().unwrap() && c.profiles.len() == 1 && c.completed_joins.is_empty()
            }) && ca == cb
                && [&a, &b].iter().all(|n| common::profile_layout_ready(n))
        })
        .await;
        let nodes = [&a, &b];
        let mut handoff = std::pin::pin!(common::profile_handoff(&nodes, &expected_default));
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
            assert_eq!(catalog.active_profile_id, expected_default);
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
        let expected_default = query(&b).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &expected_default).await;
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
        common::converge(&[&a, &b], "saved topology propagated", || {
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
        common::converge(&[&a, &b], "hotplug moves peer but not profile", || {
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
        common::converge(&[&a, &b], "saved placements restored on both replicas", || {
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
        common::converge(&[&a, &b], "restored content and register converged", || {
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
#[ignore = "slow: display scheduler clock injection required"]
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
        common::converge(&[&node], "display callback refreshes native token", || {
            token() != initial
        })
        .await;
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
        common::converge(&[&node], "failed event read retries without another callback", || {
            token() != next
        })
        .await;
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
    use oink_protocol::v2::schema::WireDto;
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
        let expected_default = query(&b).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &expected_default).await;
        let c = start(dirs[2].path(), Displays::new()).await;
        common::pair(&a, &c).await;
        common::active(&[&a, &b, &c]).await;
        common::profile_handoff(&[&a, &b, &c], &expected_default).await;
        let leaving = c.clone();
        tokio::task::spawn_blocking(move || leaving.leave_group())
            .await
            .unwrap()
            .unwrap();
        common::converge(&[&a, &b, &c], "departed computer leaves the active roster", || {
            [&a, &b]
                .iter()
                .all(|n| n.link_config().unwrap().members().members.len() == 2)
        })
        .await;
        common::profile_handoff(&[&a, &b], &expected_default).await;
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
        common::converge(
            &[&a, &b],
            "initial native observations published",
            peer_observation_settled,
        )
        .await;
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
            oink_protocol::v2::dto::LayoutValue::from_raw(stored.layout_register().unwrap().value().unwrap()).unwrap();
        let retained = crate::config_module::layout_wire::from_wire(&value).unwrap();
        assert!(retained.screens.iter().any(|s| s.id.device == c.id()));
        assert!(!a.layout().0.screens.iter().any(|s| s.id.device == c.id()));
        common::converge(&[&a, &b], "manual save propagated", || {
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
        common::converge(&[&a, &b], "remote hotplug pushes the overlapping computer", || {
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
        common::converge(
            &[&a, &b],
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
                oink_protocol::v2::dto::LayoutSource::Default
            );
        }
        lock(&db.state).displays.pop();
        db.changed();
        common::converge(
            &[&a, &b],
            "remote unplug restores saved geometry on both replicas",
            || {
                // Matching content is insufficient when a replica still has an
                // earlier register. Finish this excursion before another manual edit.
                peer_observation_settled()
                    && [&a, &b].iter().all(|n| {
                        n.observe_profiles()
                            .is_ok_and(|o| model::same_content(&o.snapshot, &expected))
                    })
            },
        )
        .await;
        for node in [&a, &b] {
            assert_eq!(lock(&node.inner.profiles.data).catalog.as_ref(), Some(&saved));
        }

        // Save a new manual layout while plugged in. Unplugging now has no
        // matching saved topology: removing the only contact must keep the gap,
        // not attach the floating computer to a corner or compact either unit.
        // Layout publication and native-token/facts publication are separate;
        // finish the first excursion before starting another manual command.
        common::converge(
            &[&a, &b],
            "unplugged native observations published",
            peer_observation_settled,
        )
        .await;
        lock(&db.state).displays.push(extra);
        db.changed();
        common::converge(&[&a, &b], "second remote plug repaired", || {
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
        common::converge(&[&a, &b], "plugged manual save propagated", || {
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
            common::converge(
                &[&a, &b],
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
        common::converge(&[&a, &b], "unmatched unplug observed on both replicas", || {
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
        common::converge(
            &[&a, &b],
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
                    oink_protocol::v2::dto::LayoutSource::Default
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

#[test]
fn offline_device_hide_is_shared_layout_and_survives_reconnect() {
    crate::test_watchdog::run(module_path!(), "offline whole device visibility", Some(4), async {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let displays = [Displays::new(), Displays::new()];
        #[cfg(windows)]
        for (dir, display) in dirs.iter().zip(&displays) {
            display.visibility.store(true, Ordering::SeqCst);
            crate::config::AgentConfig::load_from_with_device_name(dir.path(), "profiles-fixture".into())
                .unwrap()
                .update(|settings| {
                    settings.sync_system_arrangement = true;
                    settings.disable_hidden_displays = true;
                })
                .unwrap();
        }
        let a = start(dirs[0].path(), displays[0].clone()).await;
        let b = start(dirs[1].path(), displays[1].clone()).await;
        let default = query(&b).await.active_profile_id.unwrap();
        common::pair(&a, &b).await;
        common::profile_handoff(&[&a, &b], &default).await;
        let peer = b.id();
        enable(&a, peer.into(), "enable-peer").await;
        b.shutdown().await;
        common::until("peer offline", || {
            a.link_config()
                .unwrap()
                .members()
                .members
                .iter()
                .any(|m| m.device == peer && !m.online)
        })
        .await;
        let state = query(&a).await;
        assert!(
            !state
                .devices
                .iter()
                .find(|d| d.device.0 == peer)
                .unwrap()
                .can_edit_screens
        );
        let before = state.snapshot.clone().unwrap();
        let edited: OperationReply = call(
            &a,
            Request::CommitArrangement(CommitRequest {
                request_id: "offline-hide".into(),
                expected_token: state.token,
                placements: before.placements.clone(),
                disabled: vec![],
                native_plan_tokens: vec![],
                screen_shared: vec![DeviceSharing {
                    device: peer.into(),
                    shared: false,
                }],
            }),
        )
        .await;
        assert_eq!(
            edited.state.operation.as_ref().unwrap().status,
            OperationStatus::Applied
        );
        assert!(!model::screen_shared(
            edited.state.snapshot.as_ref().unwrap(),
            peer.into()
        ));
        assert!(edited.state.operation.as_ref().unwrap().history_required);
        assert_eq!(displays[1].calls.load(Ordering::SeqCst), 0);
        let op = edited.state.operation.unwrap();
        let undone: OperationReply = call(
            &a,
            Request::ReplayArrangement(ReplayRequest {
                request_id: "undo-offline-hide".into(),
                expected_token: edited.state.token,
                from: op.after.clone().unwrap(),
                target: op.before.clone(),
                native_restore_devices: vec![],
                direction: Direction::Undo,
                original_kind: HistoryKind::Edit,
            }),
        )
        .await;
        assert!(model::screen_shared(
            undone.state.snapshot.as_ref().unwrap(),
            peer.into()
        ));
        let _: OperationReply = call(
            &a,
            Request::ReplayArrangement(ReplayRequest {
                request_id: "redo-offline-hide".into(),
                expected_token: undone.state.token,
                from: op.before,
                target: op.after.unwrap(),
                native_restore_devices: vec![],
                direction: Direction::Redo,
                original_kind: HistoryKind::Edit,
            }),
        )
        .await;
        a.shutdown().await;
        let a = start(dirs[0].path(), displays[0].clone()).await;
        assert!(!model::screen_shared(
            query(&a).await.snapshot.as_ref().unwrap(),
            peer.into()
        ));
        let b = start(dirs[1].path(), displays[1].clone()).await;
        // Restarted fixtures bind fresh ephemeral ports. Discover the current
        // address explicitly, as the existing restart scenarios do.
        common::query(&a, &b).await;
        common::active(&[&a, &b]).await;
        common::until("reconnected hidden layout", || {
            let layout = crate::api::AgentApi::layout(&b).unwrap();
            layout
                .geometry
                .screens
                .iter()
                .filter(|s| s.key.device.0 == peer)
                .count()
                == 2
                && layout
                    .geometry
                    .screens
                    .iter()
                    .filter(|s| s.key.device.0 == peer)
                    .all(|s| s.hidden)
        })
        .await;
        let calls_before_show = displays[1].calls.load(Ordering::SeqCst);
        // Reconnect publications may advance the token between query and
        // admission. Retry only a definitive pre-acceptance stale refusal.
        let n = a.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                let state = n.profile_snapshot().unwrap();
                let reply = serde_json::to_value(
                    dispatch(
                        &n,
                        Request::CommitArrangement(CommitRequest {
                            request_id: "show-peer".into(),
                            expected_token: state.token,
                            placements: state.snapshot.unwrap().placements,
                            disabled: vec![],
                            native_plan_tokens: vec![],
                            screen_shared: vec![DeviceSharing {
                                device: peer.into(),
                                shared: true,
                            }],
                        }),
                    )
                    .unwrap(),
                )
                .unwrap();
                if reply["type"] == "profile_error" && reply["error"]["code"] == "stale_state" {
                    continue;
                }
                assert!(reply.get("operation_id").is_some(), "{reply}");
                break;
            }
        })
        .await
        .unwrap();
        common::until("peer shown", || {
            crate::api::AgentApi::layout(&b)
                .unwrap()
                .geometry
                .screens
                .iter()
                .filter(|s| s.key.device.0 == peer)
                .all(|s| !s.hidden)
        })
        .await;
        #[cfg(windows)]
        common::until("owner applies the global unhide", || {
            displays[1].calls.load(Ordering::SeqCst) > calls_before_show
        })
        .await;
        #[cfg(not(windows))]
        assert_eq!(displays[1].calls.load(Ordering::SeqCst), calls_before_show);
        a.shutdown().await;
        b.shutdown().await;
    });
}

#[cfg(windows)]
#[test]
fn windows_visibility_startup_and_external_unhide_are_one_shot() {
    crate::test_watchdog::run(module_path!(), "Windows target visibility", Some(12), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        displays.visibility.store(true, Ordering::SeqCst);
        let node = start(dir.path(), displays.clone()).await;
        enable(&node, node.id().into(), "enable-windows-targets").await;
        enable_hidden_policy(&node, "enable-hidden-policy").await;
        assert_eq!(displays.calls.load(Ordering::SeqCst), 0, "setting toggle wrote OS");
        let screen = crate::api::ScreenKey {
            device: node.id().into(),
            display: "second".into(),
        };
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        assert_eq!(*lock(&displays.disabled), vec!["second"]);
        assert_eq!(displays.calls.load(Ordering::SeqCst), 1);
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: false,
            },
        )
        .await;
        assert!(lock(&displays.disabled).is_empty());
        assert_eq!(displays.calls.load(Ordering::SeqCst), 2);
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        lock(&displays.returned).push("second".into());
        let n = node.clone();
        tokio::task::spawn_blocking(move || n.refresh_native_arrangement())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !query(&node)
                .await
                .snapshot
                .unwrap()
                .disabled
                .iter()
                .find(|b| b.screen == screen)
                .unwrap()
                .disabled
        );
        assert_eq!(
            displays.calls.load(Ordering::SeqCst),
            3,
            "observation fought external enable"
        );
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        let calls = displays.calls.load(Ordering::SeqCst);
        node.shutdown().await;
        assert!(
            lock(&displays.disabled).is_empty(),
            "exit did not release Oink-disabled screens"
        );
        assert_eq!(displays.calls.load(Ordering::SeqCst), calls);
        // Simulate system position drift across a crash/restart; cached hidden
        // geometry stays present in the backend, like the Windows local file.
        lock(&displays.state).displays[1].bounds = Rect::new(0.0, 100.0, 100.0, 100.0);
        let restarted = start(dir.path(), displays.clone()).await;
        assert_eq!(
            lock(&displays.state).displays[1].bounds,
            Rect::new(100.0, 0.0, 100.0, 100.0)
        );
        assert_eq!(*lock(&displays.disabled), vec!["second"]);
        assert_eq!(
            displays.calls.load(Ordering::SeqCst),
            calls + 2,
            "startup did not apply global visibility and native geometry"
        );
        restarted.shutdown().await;
        assert_eq!(displays.calls.load(Ordering::SeqCst), calls + 2);
    });
}

#[cfg(windows)]
#[test]
fn windows_external_unhide_wins_a_command_arriving_before_its_display_event() {
    crate::test_watchdog::run(module_path!(), "Windows external unhide command race", Some(8), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        displays.visibility.store(true, Ordering::SeqCst);
        let node = start(dir.path(), displays.clone()).await;
        enable(&node, node.id().into(), "race-enable-sync").await;
        enable_hidden_policy(&node, "race-enable-policy").await;
        let screen = crate::api::ScreenKey {
            device: node.id().into(),
            display: "second".into(),
        };
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        node.hold_platform_sensor_clock().await;
        let n = node.clone();
        let request = tokio::task::spawn_blocking(move || {
            n.make_arrangement_request(
                "before-external-unhide",
                n.id().into(),
                None,
                &[Disabled { screen, disabled: true }],
            )
        })
        .await
        .unwrap()
        .unwrap();
        let calls = displays.calls.load(Ordering::SeqCst);
        lock(&displays.returned).push("second".into());
        let n = node.clone();
        let reply = tokio::task::spawn_blocking(move || n.owner_arrangement(n.id().into(), request))
            .await
            .unwrap();
        assert_eq!(reply.outcome(), Ok(super::native::FAILED));
        assert_eq!(reply.error().unwrap().unwrap().0, "stale_state");
        assert!(lock(&displays.disabled).is_empty());
        assert_eq!(
            displays.calls.load(Ordering::SeqCst),
            calls,
            "stale command fought the OS"
        );
        node.resume_platform_sensor_clock();
        node.shutdown().await;
    });
}

#[cfg(windows)]
async fn enable_hidden_policy(node: &Node, id: &str) {
    let state = query(node).await;
    let owner = state.devices.iter().find(|d| d.device.0 == node.id()).unwrap();
    let _: LayoutProfilesState = call(
        node,
        Request::SetDisableHiddenDisplays(SettingRequest {
            request_id: id.into(),
            device: owner.device,
            expected_preference_token: owner.preference_token.clone(),
            enabled: true,
        }),
    )
    .await;
}

#[cfg(windows)]
async fn set_hidden_policy(node: &Node, enabled: bool, id: &str) {
    let state = query(node).await;
    let owner = state.devices.iter().find(|d| d.device.0 == node.id()).unwrap();
    let _: LayoutProfilesState = call(
        node,
        Request::SetDisableHiddenDisplays(SettingRequest {
            request_id: id.into(),
            device: owner.device,
            expected_preference_token: owner.preference_token.clone(),
            enabled,
        }),
    )
    .await;
}

#[cfg(windows)]
#[test]
fn windows_hidden_policy_defaults_off_and_releases_on_both_switches_and_exit() {
    crate::test_watchdog::run(module_path!(), "Windows release lifecycle", Some(12), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        displays.visibility.store(true, Ordering::SeqCst);
        let node = start(dir.path(), displays.clone()).await;
        enable(&node, node.id().into(), "release-parent-on").await;
        assert!(!query(&node).await.devices[0].disable_hidden_displays);
        let screen = crate::api::ScreenKey {
            device: node.id().into(),
            display: "second".into(),
        };
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        assert!(
            lock(&displays.disabled).is_empty(),
            "default-off policy changed OS visibility"
        );
        set_hidden_policy(&node, true, "release-policy-on").await;
        assert_eq!(
            *lock(&displays.disabled),
            vec!["second"],
            "enabling the policy applies the hidden layout"
        );
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: false,
            },
        )
        .await;
        assert!(lock(&displays.disabled).is_empty());
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        assert_eq!(*lock(&displays.disabled), vec!["second"]);
        let geometry = lock(&displays.state).clone();
        set_hidden_policy(&node, false, "release-policy-off").await;
        assert!(lock(&displays.disabled).is_empty());
        assert!(
            query(&node)
                .await
                .snapshot
                .unwrap()
                .disabled
                .iter()
                .find(|b| b.screen == screen)
                .unwrap()
                .disabled
        );
        assert_eq!(*lock(&displays.state), geometry, "policy-off restored positions");
        set_hidden_policy(&node, true, "release-policy-back-on").await;
        assert_eq!(*lock(&displays.disabled), vec!["second"]);
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: false,
            },
        )
        .await;
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        let state = query(&node).await;
        let owner = &state.devices[0];
        let _: LayoutProfilesState = call(
            &node,
            Request::SetSyncSystemArrangement(SettingRequest {
                request_id: "release-parent-off".into(),
                device: owner.device,
                expected_preference_token: owner.preference_token.clone(),
                enabled: false,
            }),
        )
        .await;
        assert!(lock(&displays.disabled).is_empty());
        assert!(
            query(&node)
                .await
                .snapshot
                .unwrap()
                .disabled
                .iter()
                .find(|b| b.screen == screen)
                .unwrap()
                .disabled
        );
        assert_eq!(*lock(&displays.state), geometry, "parent-off restored positions");
        enable(&node, node.id().into(), "release-parent-back-on").await;
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: false,
            },
        )
        .await;
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: screen.clone(),
                hidden: true,
            },
        )
        .await;
        assert_eq!(*lock(&displays.disabled), vec!["second"]);
        node.shutdown().await;
        assert!(lock(&displays.disabled).is_empty());
        assert_eq!(*lock(&displays.state), geometry);
        let restarted = start(dir.path(), displays.clone()).await;
        assert_eq!(
            *lock(&displays.disabled),
            vec!["second"],
            "startup missed saved hidden state"
        );
        assert!(
            query(&restarted)
                .await
                .snapshot
                .unwrap()
                .disabled
                .iter()
                .find(|b| b.screen == screen)
                .unwrap()
                .disabled
        );
        restarted.shutdown().await;
        assert!(lock(&displays.disabled).is_empty());
    });
}

#[cfg(windows)]
#[test]
fn windows_hidden_policy_switch_applies_current_layout_once() {
    crate::test_watchdog::run(module_path!(), "Windows hidden policy switch", Some(4), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        displays.visibility.store(true, Ordering::SeqCst);
        let node = start(dir.path(), displays.clone()).await;
        enable(&node, node.id().into(), "switch-parent-on").await;
        set_hidden_policy(&node, true, "switch-on-nothing-hidden").await;
        assert!(lock(&displays.disabled).is_empty(), "no screen is hidden in the layout");
        set_hidden_policy(&node, false, "switch-off-nothing-hidden").await;
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: crate::api::ScreenKey {
                    device: node.id().into(),
                    display: "second".into(),
                },
                hidden: true,
            },
        )
        .await;
        assert!(lock(&displays.disabled).is_empty(), "policy off left the OS alone");
        let geometry = lock(&displays.state).clone();
        let calls = displays.calls.load(Ordering::SeqCst);
        set_hidden_policy(&node, true, "switch-on").await;
        assert_eq!(
            *lock(&displays.disabled),
            vec!["second"],
            "switching on did not apply the hidden screen"
        );
        assert_eq!(displays.calls.load(Ordering::SeqCst), calls + 1, "one OS transaction");
        set_hidden_policy(&node, false, "switch-off").await;
        assert!(
            lock(&displays.disabled).is_empty(),
            "switching off kept the screen disabled"
        );
        assert_eq!(*lock(&displays.state), geometry);
        set_hidden_policy(&node, true, "switch-on-again").await;
        assert_eq!(*lock(&displays.disabled), vec!["second"]);
        node.shutdown().await;
        assert!(lock(&displays.disabled).is_empty());
    });
}

#[test]
fn retained_disabled_catalog_never_replaces_active_input_geometry() {
    crate::test_watchdog::run(module_path!(), "active desktop versus catalog", Some(4), async {
        use oink_foundation::input::{CapturedEvent, Disposition, InputEvent, SourceKind};
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        lock(&displays.disabled).push("second".into());
        // Match the reported startup policy; either switch being off correctly
        // releases Oink-disabled targets on Windows before input starts.
        crate::config::AgentConfig::load_from_with_device_name(dir.path(), "active-desktop-fixture".into())
            .unwrap()
            .update(|s| {
                s.sync_system_arrangement = true;
                s.disable_hidden_displays = true;
            })
            .unwrap();
        let input = Arc::new(oink_test_support::input::FakeInput::new());
        let node = Node::start(NodeOptions {
            net: oink_test_support::network_config(dir.path().into(), "active-desktop-fixture", 0),
            input: input.clone(),
            displays: displays.displays().unwrap(),
            display_backend: Some(displays.clone()),
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        })
        .await
        .unwrap();
        let _: serde_json::Value = call(
            &node,
            Request::SetScreenHidden {
                screen: crate::api::ScreenKey {
                    device: node.id().into(),
                    display: "second".into(),
                },
                hidden: true,
            },
        )
        .await;
        assert_eq!(node.current_active_displays().unwrap().len(), 1);
        common::until("input captures", || input.capture_sessions() > 0).await;
        for _ in 0..3 {
            // Repeated catalog publications must never reintroduce the retained
            // target into the router's active desktop list through InputStreams.
            node.link_config()
                .unwrap()
                .initialize_displays(displays.displays().unwrap())
                .unwrap();
            node.refresh_active_input_displays().unwrap();
            let result = input.feed(&CapturedEvent {
                physical: true,
                event: InputEvent::PointerDelta { dx: -1., dy: 0. },
                source: SourceKind::Mouse,
                time_us: 100,
                cursor: Some(Point::new(50., 50.)),
            });
            assert_eq!(result, Disposition::Pass);
        }
        assert_eq!(node.layout().0.screens.len(), 2, "retained target remains editable");
        node.shutdown().await;
    });
}

#[test]
fn retained_android_screen_is_in_arrangement_api_and_always_rigid() {
    crate::test_watchdog::run(module_path!(), "retained Android arrangement", Some(2), async {
        let dir = tempfile::tempdir().unwrap();
        let displays = Displays::new();
        {
            let mut native = lock(&displays.state);
            native.displays.truncate(1);
            native.displays[0].key = oink_foundation::display::ANDROID_INPUT_DISPLAY.into();
            native.displays[0].bounds = Rect::new(0., 0., 400., 800.);
        }
        let mut net = oink_test_support::network_config(dir.path().into(), "phone-arrangement-fixture", 0);
        net.os = "android".into();
        // This is the production API projection over a retained phone record,
        // with fake OS backends; it does not certify an Android helper/device.
        let node = Node::start(NodeOptions {
            net,
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
        let state = query(&node).await;
        let phone = crate::api::DeviceKey(node.id());
        let snapshot = state.snapshot.unwrap();
        assert!(snapshot.placements.iter().any(|p| p.device == phone));
        assert!(snapshot.native.iter().any(|n| {
            n.device == phone
                && n.screens
                    .iter()
                    .any(|s| s.screen.display == oink_foundation::display::ANDROID_INPUT_DISPLAY)
        }));
        let device = state.devices.iter().find(|d| d.device == phone).unwrap();
        assert!(!device.sync_system_arrangement);
        assert!(!device.can_edit_screens);
        node.shutdown().await;
    });
}
