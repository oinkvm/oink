use super::*;
use crate::{api::AgentApi, node::NodeOptions, snapshot_tests::common};
use oink_input::InputBackend;
use std::{sync::Arc, time::Duration};

#[test]
fn emergency_cleanup_keeps_pause_settling_until_resume_is_available() {
    crate::test_watchdog::run(
        module_path!(),
        "emergency_cleanup_keeps_pause_settling_until_resume_is_available",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let node = Node::start_with_fake_platform(NodeOptions {
                net: oink_test_support::network_config(dir.path().to_path_buf(), "pause-owner", 0),
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
            disable_both(&node).await;
            let (completed, mut results) = tokio::sync::mpsc::unbounded_channel();
            let (pause_changed, mut pauses) = tokio::sync::mpsc::unbounded_channel();
            let subscription = node.subscribe(Arc::new(move |event| {
                if let ApiEvent::DeviceControlCompleted(result) = event {
                    let _ = completed.send(result.clone());
                }
                if let ApiEvent::Pause(pause) = event {
                    let _ = pause_changed.send(pause.clone());
                }
            }));

            // Hold an ordinary owner operation in front of durable emergency
            // cleanup. Native safety still applies without acquiring this gate.
            let (entered, ready) = tokio::sync::oneshot::channel();
            let (release, held) = std::sync::mpsc::sync_channel(1);
            let owner = node.clone();
            let gate = tokio::task::spawn_blocking(move || {
                let _gate = owner.owner_gate();
                entered.send(()).unwrap();
                held.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            ready.await.unwrap();
            node.pause_all("00000000-0000-4000-8000-000000000001".into()).unwrap();
            common::until("native emergency pause", || node.input_status().unwrap().paused).await;
            let during_cleanup = node.pause_status();
            assert!(during_cleanup.paused && during_cleanup.emergency);

            let reply = node
                .set_local_paused("00000000-0000-4000-8000-000000000002".into(), false)
                .unwrap();
            assert!(matches!(reply, DeviceControlReply::Accepted(_)), "{reply:?}");
            let busy = completion_for(&mut results, "00000000-0000-4000-8000-000000000002").await;
            assert_eq!(busy.result, DeviceControlResult::refused(DeviceControlFailure::Busy));
            eprintln!("native pause applied: {during_cleanup:?}; early resume: {busy:?}");

            release.send(()).unwrap();
            gate.await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let pause = pauses.recv().await.unwrap();
                    if pause.paused && !pause.settling {
                        break;
                    }
                }
            })
            .await
            .expect("emergency cleanup must publish the settled pause to subscribers");
            assert!(!node.pause_status().settling);
            assert_disables_cleared(&node);
            let reply = node
                .set_local_paused("00000000-0000-4000-8000-000000000003".into(), false)
                .unwrap();
            assert!(matches!(reply, DeviceControlReply::Accepted(_)), "{reply:?}");
            let resumed = completion_for(&mut results, "00000000-0000-4000-8000-000000000003").await;
            assert_eq!(resumed.result, DeviceControlResult::Applied);
            let after = node.pause_status();
            assert!(!after.paused && !after.emergency && !after.settling);
            assert_eq!(after.author, None);
            node.unsubscribe(subscription);
            node.shutdown().await;

            assert!(
                during_cleanup.settling,
                "resume is Busy until durable emergency cleanup finishes: {during_cleanup:?}"
            );
        },
    );
}

#[test]
fn input_backlog_publishes_existing_local_pause_facts_cards_and_resume() {
    crate::test_watchdog::run(
        module_path!(),
        "input_backlog_publishes_existing_local_pause_facts_cards_and_resume",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(oink_test_support::input::FakeInput::new());
            let node = Node::start_with_fake_platform(NodeOptions {
                net: oink_test_support::network_config(dir.path().to_path_buf(), "backlog-owner", 0),
                input: fake.clone(),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            })
            .await
            .unwrap();
            common::until("initial injection session", || fake.injection_active()).await;
            disable_both(&node).await;
            node.input_thread().unwrap().report_input_backlog();
            common::until("local backlog pause", || {
                let pause = node.pause_status();
                pause.paused && pause.reason == PauseReason::InputBacklog && !pause.settling
            })
            .await;
            assert!(!fake.injection_active());
            assert_eq!(fake.capture_active(), Some(false));
            common::until("backlog owner facts", || {
                node.link_config()
                    .unwrap()
                    .local_facts()
                    .pause()
                    .unwrap()
                    .unwrap()
                    .reason()
                    .unwrap()
                    == dto::PauseReason::InputBacklog
            })
            .await;
            common::until("backlog device card", || {
                node.device_cards().unwrap().cards.iter().any(|card| {
                    card.device == DeviceKey(node.id())
                        && card
                            .pause
                            .as_ref()
                            .is_some_and(|pause| pause.reason == PauseReason::InputBacklog)
                })
            })
            .await;
            let facts = node.link_config().unwrap().local_facts();
            assert_eq!(
                facts.pause().unwrap().unwrap().reason().unwrap(),
                dto::PauseReason::InputBacklog
            );
            let cards = node.device_cards().unwrap();
            assert!(cards.emergency_delivery.is_none());
            let card = cards
                .cards
                .iter()
                .find(|card| card.device == DeviceKey(node.id()))
                .unwrap();
            let pause = card.pause.as_ref().unwrap();
            assert!(pause.paused);
            assert_eq!(pause.reason, PauseReason::InputBacklog);
            node.set_local_paused("00000000-0000-4000-8000-000000000004".into(), false)
                .unwrap();
            common::until("fresh resumed injection", || {
                !node.pause_status().paused && fake.injection_active()
            })
            .await;
            assert_eq!(node.pause_status().reason, PauseReason::None);
            assert_disables_cleared(&node);
            node.shutdown().await;
        },
    );
}

async fn disable_both(node: &Node) {
    let n = node.clone();
    tokio::task::spawn_blocking(move || {
        n.edit_device_option(n.id().into(), dto::LocalOption::MouseMode, 2u32.write())
            .unwrap();
        n.edit_device_option(n.id().into(), dto::LocalOption::KeyboardDisabled, true.write())
            .unwrap();
    })
    .await
    .unwrap();
    assert!(node.input_status().unwrap().keyboard_disabled);
    assert_eq!(
        node.settings().routing.mouse_mode,
        oink_foundation::input::InputMode::Disabled
    );
}

fn assert_disables_cleared(node: &Node) {
    use oink_foundation::input::InputMode;
    let disk = crate::config::AgentConfig::load_from(node.data_dir()).unwrap();
    assert!(!disk.settings.keyboard_disabled);
    assert_eq!(disk.settings.routing.mouse_mode, InputMode::Shared);
    assert!(!node.input_status().unwrap().keyboard_disabled);
    assert_eq!(node.input_status().unwrap().mouse_mode, InputMode::Shared);
    let preferences = node
        .link_config()
        .unwrap()
        .local_facts()
        .preferences()
        .unwrap()
        .unwrap()
        .0;
    for (option, value) in [
        (dto::LocalOption::KeyboardDisabled, false.write()),
        (dto::LocalOption::MouseMode, 0u32.write()),
    ] {
        assert_eq!(
            preferences
                .iter()
                .find(|p| p.option() == Ok(option))
                .unwrap()
                .value()
                .unwrap(),
            value
        );
    }
}

#[test]
fn ordinary_pause_durably_clears_both_disables_and_resume_keeps_enabled() {
    crate::test_watchdog::run(
        module_path!(),
        "ordinary_pause_durably_clears_both_disables_and_resume_keeps_enabled",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let fake = Arc::new(oink_test_support::input::FakeInput::new());
            let node = Node::start_with_fake_platform(NodeOptions {
                net: oink_test_support::network_config(dir.path().to_path_buf(), "pause-owner", 0),
                input: fake.clone(),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            })
            .await
            .unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let sub = node.subscribe(Arc::new(move |event| {
                if let ApiEvent::DeviceControlCompleted(result) = event {
                    let _ = tx.send(result.clone());
                }
            }));
            for (keyboard_mode, keyboard_icon, value) in [
                (SharingMode::Shared, SharingIcon::Shared, 0u32),
                (SharingMode::Local, SharingIcon::Local, 1u32),
            ] {
                let owner = node.clone();
                tokio::task::spawn_blocking(move || {
                    owner
                        .edit_device_option(owner.id().into(), dto::LocalOption::KeyboardMode, value.write())
                        .unwrap();
                })
                .await
                .unwrap();
                disable_both(&node).await;
                for paused in [true, false] {
                    let id = crate::device_controls::runtime::fresh_request_id();
                    node.set_local_paused(id.clone(), paused).unwrap();
                    let result = completion_for(&mut rx, &id).await;
                    assert_eq!(result.request_id, id);
                    assert_eq!(result.result, DeviceControlResult::Applied);
                    assert_eq!(node.pause_status().paused, paused);
                    assert_disables_cleared(&node);
                    common::until("capture follows owner pause", || {
                        node.input_backend().capture_active() == Some(!paused)
                    })
                    .await;
                    // Wait past the withdrawal callback, which follows native
                    // detachment. The old fake omitted this callback altogether.
                    let turn = fake.loop_turns();
                    (node.input_thread().unwrap().wake())();
                    common::until("capture observation published", || fake.loop_turns() > turn).await;
                    node.publish_local_options();
                    node.refresh_device_cards();
                    let cards = node.device_cards().unwrap();
                    let status = cards
                        .cards
                        .iter()
                        .find(|card| card.is_self)
                        .unwrap()
                        .status
                        .as_ref()
                        .unwrap();
                    assert_eq!(status.keyboard.disabled, Some(false));
                    assert_eq!(status.keyboard.resource.configured_mode, Some(keyboard_mode));
                    assert_eq!(status.keyboard.resource.icon, keyboard_icon);
                    assert_eq!(status.mouse.icon, SharingIcon::Shared);
                    assert_eq!(
                        status.keyboard.resource.condition,
                        if paused {
                            CardCondition::LocalPaused
                        } else {
                            CardCondition::Ready
                        }
                    );
                }
            }
            disable_both(&node).await;
            let path = lock(&node.inner.config).settings_path();
            let saved = std::fs::read(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            let id = crate::device_controls::runtime::fresh_request_id();
            node.set_local_paused(id.clone(), true).unwrap();
            let failed = completion_for(&mut rx, &id).await;
            assert_eq!(
                failed.result,
                DeviceControlResult::Failed {
                    reason: DeviceControlFailure::PersistenceFailed,
                    unknown_code: None,
                }
            );
            assert!(node.pause_status().paused && node.pause_status().settling);
            assert!(!node.input_status().unwrap().keyboard_disabled);
            assert_eq!(
                node.input_status().unwrap().mouse_mode,
                oink_foundation::input::InputMode::Shared
            );
            // Leave the failure in place through shutdown; no retry is hidden by the fixture.
            node.unsubscribe(sub);
            node.shutdown().await;
            std::fs::remove_dir(&path).unwrap();
            std::fs::write(&path, saved).unwrap();
        },
    );
}

async fn completion_for(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<DeviceControlCompleted>,
    id: &str,
) -> DeviceControlCompleted {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = receiver.recv().await.unwrap();
            if result.request_id == id {
                return result;
            }
        }
    })
    .await
    .expect("matching control receipt")
}
