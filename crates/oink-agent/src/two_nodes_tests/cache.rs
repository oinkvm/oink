//! Real cache request and owner publication over C0-authenticated nodes.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_remote_clear_and_owner_publication_work_while_paused() {
    use crate::api::{ApiEvent, DeviceKey, LiveViews, Request};
    use std::sync::Mutex;
    let a = start("cache-a", 0).await;
    let b = start("cache-b", 0).await;
    use oink_clipboard::store::{Batch, BatchId, StoreKind};
    let batch = Batch::create(&b.node.cache_dir(), StoreKind::CompletedDelivery, BatchId([81; 16])).unwrap();
    batch.snapshot(1, &mut std::io::Cursor::new(vec![8; 16001])).unwrap();
    batch.complete().unwrap();
    let stale = batch.path().to_owned();
    drop(batch);
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
