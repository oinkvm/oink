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
