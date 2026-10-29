//! Real cache request and owner publication over C0-authenticated nodes.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_remote_clear_and_owner_publication_work_while_paused() {
    use crate::api::{ApiEvent, DeviceKey, LiveViews, Request};
    use std::sync::Mutex;
    let a = start("cache-a", 0).await;
    let b = start("cache-b", 0).await;
    // Node::start starts the clipboard actor, whose asynchronous recovery
    // removes unpinned stale batches. Its first resource observation is only
    // published after recovery; seed the disposable test batch after that.
    common::until("cache fixture clipboard store recovery", || {
        b.node
            .link_config()
            .is_ok_and(|config| clipboard_store_recovered(&config))
    })
    .await;
    use oink_clipboard::store::{Batch, BatchId, StoreKind};
    let stale = b
        .node
        .cache_dir()
        .join(StoreKind::SourceSnapshot.directory())
        .join("51".repeat(16));
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join(".oink-content-v1"), br#"{"version":1,"kind":"SourceSnapshot","state":"Complete","external_writers":false,"snapshot_bytes":16001,"completed_at":1}"#).unwrap();
    std::fs::write(stale.join("r-1"), vec![8; 16001]).unwrap();
    let expected = oink_clipboard::cache::tree_bytes(&stale).unwrap();
    common::pair(&a.node, &b.node).await;
    api(&a.node, |n| {
        n.set_local_paused(crate::device_controls::runtime::fresh_request_id(), true)
    })
    .await
    .unwrap();
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = changes.clone();
    let subscription = a.node.subscribe_views(
        Arc::new(move |event| {
            if let ApiEvent::CacheUsage(usage) = event {
                sink.lock().unwrap().push(usage.clone());
            }
        }),
        LiveViews::default(),
    );
    let device = DeviceKey(b.node.id());
    let list = api(&a.node, move |n| n.cache_usage(Some(device))).await.unwrap();
    assert_eq!(list[0].usage.total_bytes, expected);
    let reply = api(&a.node, move |n| {
        crate::api::dispatch(n, Request::ClearCache { device: Some(device) })
    })
    .await
    .unwrap();
    let crate::api::Reply::CacheClear(result) = reply else {
        panic!("clear reply");
    };
    assert_eq!(result.device, device);
    assert_eq!(result.result.removed_bytes, expected);
    assert_eq!(result.result.kept_bytes, 0);
    assert!(!stale.exists());
    wait_for("owner cache publication", 5., || {
        changes
            .lock()
            .unwrap()
            .iter()
            .any(|list| list.iter().any(|v| v.device == device && v.usage.total_bytes == 0))
    })
    .await;
    // Storage writes publish directly without a view subscription gate or a poll.
    let active = Batch::create(&b.node.cache_dir(), StoreKind::PromiseExport, BatchId([82; 16])).unwrap();
    active.snapshot(1, &mut std::io::Cursor::new(vec![3; 7000])).unwrap();
    wait_for("changed usage reaches hidden remote UI", 5., || {
        changes
            .lock()
            .unwrap()
            .iter()
            .any(|list| list.iter().any(|v| v.device == device && v.usage.total_bytes > 0))
    })
    .await;
    drop(active);
    a.node.unsubscribe(subscription);
    a.node.shutdown().await;
    b.node.shutdown().await;
}

fn clipboard_store_recovered(config: &crate::config_module::ConfigModule) -> bool {
    // Resource publication updates local_facts independently of the membership
    // projection. Reading the self member here can wait forever on a stale copy.
    config
        .local_facts()
        .resources()
        .ok()
        .flatten()
        .and_then(|resources| resources.clipboard_mode().ok().flatten())
        .is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_recovery_observation_does_not_require_a_membership_publication() {
    use crate::config_module::runtime_tests::{same_group, start, stop};
    use oink_protocol::v1::dto;
    let nodes = start(same_group(1)).await;
    let group = &nodes[0].group;
    assert!(!clipboard_store_recovered(group));
    let membership = group.members();
    group
        .publish_local_clipboard(Some(dto::SharingMode::Shared), None)
        .unwrap();
    assert!(
        group
            .local_facts()
            .resources()
            .unwrap()
            .unwrap()
            .clipboard_mode()
            .unwrap()
            .is_some()
    );
    assert_eq!(
        group.members().as_ref(),
        membership.as_ref(),
        "resource observations do not republish membership"
    );
    assert!(
        clipboard_store_recovered(group),
        "published recovery must be visible without unrelated membership work"
    );
    stop(&nodes).await;
}
