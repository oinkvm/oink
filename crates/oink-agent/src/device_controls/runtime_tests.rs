//! Real owner transactions and durable storage with fake platform devices.
use super::*;
use crate::snapshot_tests::{Displays, start_with_clip};

async fn execute(node: &Node, request: Message) -> dto::LocalEditResult {
    let node = node.clone();
    let response = tokio::task::spawn_blocking(move || {
        node.execute_owner_request(
            Requester {
                device: node.id().into(),
                nonce: node.inner.boot.nonce,
            },
            node.link_config().unwrap().applied().group,
            request,
            false,
        )
    })
    .await
    .unwrap()
    .unwrap();
    let Message::ConfigLocalEditResult(response) = response else {
        panic!("wrong response family")
    };
    response
}

#[test]
fn owner_receipts_fence_replays_stale_revisions_and_failed_persistence() {
    crate::test_watchdog::run(
        module_path!(),
        "owner_receipts_fence_replays_stale_revisions_and_failed_persistence",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let node = start_with_clip(
                &oink_test_support::network_config(dir.path().join("owner"), "owner", 0),
                &Displays::new("owner"),
                None,
            )
            .await;
            let revision = lock(&node.inner.config)
                .owner()
                .revision(dto::LocalOption::ClipboardEnabled)
                .unwrap();
            let mut binding = TokenBinding {
                owner: OwnerVersion {
                    group: node.link_config().unwrap().applied().group.into(),
                    device: node.id().into(),
                    boot_nonce: node.inner.boot.nonce,
                    revision,
                },
                operation: TokenOperation::Option {
                    edit: CardOptionEdit::ClipboardEnabled(false),
                },
            };
            let id = fresh_request_id();
            let request = requests::action_request(&binding, &id).unwrap();
            let receipt = execute(&node, request.clone()).await;
            assert_eq!(receipt.outcome().unwrap(), dto::ControlOutcome::Applied);
            assert!(receipt.persisted().unwrap());
            assert!(receipt.fact_sequence().unwrap().is_some());
            let applied_revision = receipt.current().unwrap().unwrap().revision().unwrap();
            assert_eq!(applied_revision, revision + 1);
            assert!(!node.settings().clipboard_enabled);
            assert!(
                !crate::config::AgentConfig::load_from(node.data_dir())
                    .unwrap()
                    .settings
                    .clipboard_enabled
            );
            assert_eq!(execute(&node, request).await, receipt);
            assert_eq!(
                lock(&node.inner.config)
                    .owner()
                    .revision(dto::LocalOption::ClipboardEnabled),
                Some(applied_revision)
            );

            let stale = execute(&node, requests::action_request(&binding, &fresh_request_id()).unwrap()).await;
            assert_eq!(stale.reason().unwrap(), Failure::StaleRevision);
            binding.operation = TokenOperation::Option {
                edit: CardOptionEdit::ClipboardEnabled(true),
            };
            let conflict = execute(&node, requests::action_request(&binding, &id).unwrap()).await;
            assert_eq!(conflict.reason().unwrap(), Failure::StateConflict);

            binding.owner.revision = applied_revision;
            binding.owner.boot_nonce[0] ^= 1;
            let old_boot = execute(&node, requests::action_request(&binding, &fresh_request_id()).unwrap()).await;
            assert_eq!(old_boot.reason().unwrap(), Failure::StaleBoot);
            binding.owner.boot_nonce = node.inner.boot.nonce;

            let path = lock(&node.inner.config).settings_path();
            let saved = std::fs::read(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            let failed = execute(&node, requests::action_request(&binding, &fresh_request_id()).unwrap()).await;
            assert_eq!(failed.outcome().unwrap(), dto::ControlOutcome::Failed);
            assert_eq!(failed.reason().unwrap(), Failure::PersistenceFailed);
            assert!(!failed.persisted().unwrap());
            assert!(!node.settings().clipboard_enabled);
            assert_eq!(
                lock(&node.inner.config)
                    .owner()
                    .revision(dto::LocalOption::ClipboardEnabled),
                Some(applied_revision)
            );
            let observed = node
                .link_config()
                .unwrap()
                .local_facts()
                .preferences()
                .unwrap()
                .unwrap()
                .0
                .into_iter()
                .find(|item| item.option() == Ok(dto::LocalOption::ClipboardEnabled))
                .unwrap();
            assert_eq!(observed.revision().unwrap(), applied_revision);
            assert_eq!(observed.value().unwrap(), false.write());
            std::fs::remove_dir(&path).unwrap();
            std::fs::write(&path, saved).unwrap();
            node.shutdown().await;
        },
    );
}

#[test]
fn process_shutdown_flushes_pending_forwarding_preferences_before_returning() {
    crate::test_watchdog::run(
        module_path!(),
        "process_shutdown_flushes_pending_forwarding_preferences_before_returning",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let node = start_with_clip(
                &oink_test_support::network_config(dir.path().join("owner"), "owner", 0),
                &Displays::new("owner"),
                None,
            )
            .await;
            let option = dto::LocalOption::ForwardingControls;
            let revision = lock(&node.inner.config).owner().revision(option).unwrap();
            lock(&node.inner.config)
                .edit_forwarding_deferred(
                    revision,
                    oink_protocol::v1::RawValue::Map([(1, 67u16.write()), (2, false.write())].into_iter().collect()),
                )
                .unwrap();
            assert!(!lock(&node.inner.config).forwarding_persisted());
            node.shutdown().await;
            let durable = crate::config::AgentConfig::load_from(node.data_dir()).unwrap();
            assert_eq!(durable.settings.audio.forwarding_volume_percent, 67);
            assert_eq!(durable.owner().revision(option), Some(revision + 1));
        },
    );
}

#[test]
fn update_preparation_drains_admitted_preferences_after_closing_new_writes() {
    const CHILD: &str = "OINK_DEFERRED_UPDATE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // The watchdog bounds runtime teardown but does not isolate process-global
        // maintenance admission. A real backend update must run in its own process.
        let _watchdog = crate::test_watchdog::Scenario::new(module_path!(), "isolated update preparation");
        let rendezvous = tempfile::tempdir().unwrap();
        let unrelated_dir = tempfile::tempdir().unwrap();
        let mut unrelated = crate::config::AgentConfig::load_from(unrelated_dir.path()).unwrap();
        let test = format!(
            "{}::update_preparation_drains_admitted_preferences_after_closing_new_writes",
            module_path!()
        );
        let test = test.split_once("::").unwrap().1;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, rendezvous.path())
            .spawn()
            .unwrap();
        while !rendezvous.path().join("closed").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before closing its gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Deterministically overlap another fixture's persistence with the closed
        // child gate; the same overlap failed with WouldBlock in the shared process.
        let saved = unrelated.save();
        std::fs::write(rendezvous.path().join("continue"), []).unwrap();
        assert!(child.wait().unwrap().success());
        saved.expect("independent test store remains writable while child prepares update");
        return;
    }
    crate::test_watchdog::run(
        module_path!(),
        "update_preparation_drains_admitted_preferences_after_closing_new_writes",
        Some(4),
        async {
            let dir = tempfile::tempdir().unwrap();
            let node = start_with_clip(
                &oink_test_support::network_config(dir.path().join("owner"), "owner", 0),
                &Displays::new("owner"),
                None,
            )
            .await;
            let revision = lock(&node.inner.config)
                .owner()
                .revision(dto::LocalOption::ForwardingControls)
                .unwrap();
            lock(&node.inner.config)
                .edit_forwarding_deferred(
                    revision,
                    oink_protocol::v1::RawValue::Map([(1, 73u16.write()), (2, false.write())].into_iter().collect()),
                )
                .unwrap();
            let preparing = node.clone();
            let token = tokio::task::spawn_blocking(move || preparing.prepare_for_update())
                .await
                .unwrap()
                .unwrap();
            assert!(oink_foundation::maintenance::process().closed());
            let rendezvous = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
            std::fs::write(rendezvous.join("closed"), []).unwrap();
            crate::test_watchdog::until("parent persisted its independent store", || {
                rendezvous.join("continue").exists()
            })
            .await;
            assert!(lock(&node.inner.config).forwarding_persisted());
            assert_eq!(
                crate::config::AgentConfig::load_from(node.data_dir())
                    .unwrap()
                    .settings
                    .audio
                    .forwarding_volume_percent,
                73
            );
            assert!(
                lock(&node.inner.config)
                    .edit_forwarding_deferred(
                        revision + 1,
                        oink_protocol::v1::RawValue::Map([(1, 74u16.write())].into_iter().collect())
                    )
                    .is_err()
            );
            drop(token);
            // An unavailable destination must block quit/update preparation,
            // retain the accepted memory value, and reopen admission on abort.
            lock(&node.inner.config)
                .edit_forwarding_deferred(
                    revision + 1,
                    oink_protocol::v1::RawValue::Map([(1, 74u16.write())].into_iter().collect()),
                )
                .unwrap();
            let path = lock(&node.inner.config).settings_path();
            let backup = path.with_extension("durable");
            std::fs::rename(&path, &backup).unwrap();
            std::fs::create_dir(&path).unwrap();
            let preparing = node.clone();
            assert!(
                tokio::task::spawn_blocking(move || preparing.prepare_for_update())
                    .await
                    .unwrap()
                    .is_err()
            );
            assert!(!oink_foundation::maintenance::process().closed());
            assert!(!lock(&node.inner.config).forwarding_persisted());
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&backup, &path).unwrap();
            node.shutdown().await;
        },
    );
}
