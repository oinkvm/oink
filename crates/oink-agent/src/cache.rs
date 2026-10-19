//! Storage owner publishes on change; request/clear use one bounded Low operation.
use crate::{
    api::{self, ApiEvent, CacheDevice, DeviceKey},
    node::{Node, lock},
};
use oink_clipboard::cache::{CacheCategory, CacheClear, CacheSize, CacheUsage};
mod wire;
use oink_foundation::DeviceId;
use oink_foundation::session::Boot;
use oink_network::link::{LinkId, ModuleEvent, ModulePort};
use oink_protocol::{
    frame::{StreamCode, StreamKind},
    v1::{
        self, Message, RawValue as V, dto,
        schema::{WireDto, WireValue},
    },
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use wire::{boot, clear_projection, project, wire_clear, wire_usage};

type CachedReplies = std::collections::VecDeque<(Boot, [u8; 16], bool, dto::CacheResult)>;

pub(crate) struct State {
    native: bool,
    scan: tokio::sync::Mutex<()>,
    local: Mutex<(u64, CacheUsage)>,
    remote: Mutex<HashMap<DeviceId, (Boot, u64, dto::CacheUsageValue)>>,
    replies: tokio::sync::Mutex<CachedReplies>,
    changed: Arc<tokio::sync::Notify>,
}
impl State {
    pub(crate) fn new(native: bool) -> Self {
        Self {
            native,
            scan: Default::default(),
            local: Default::default(),
            remote: Default::default(),
            replies: Default::default(),
            changed: Default::default(),
        }
    }
}
fn unavailable() -> api::ApiError {
    api::ApiError::Unavailable {
        message: "设备缓存不可用，请在设备在线时重试".into(),
    }
}
fn snapshot(node: &Node) -> Vec<CacheDevice> {
    let mut list = vec![CacheDevice {
        device: node.id().into(),
        usage: lock(&node.inner.cache.local).1.clone(),
    }];
    list.extend(lock(&node.inner.cache.remote).iter().filter_map(|(id, (_, _, usage))| {
        project(usage).ok().map(|usage| CacheDevice {
            device: (*id).into(),
            usage,
        })
    }));
    list.sort_by_key(|v| v.device);
    list
}
fn emit(node: &Node) {
    node.emit(ApiEvent::CacheUsage(snapshot(node)));
}
fn set_remote(node: &Node, peer: DeviceId, boot: Boot, revision: u64, wire: dto::CacheUsageValue) {
    if project(&wire).is_err() {
        return;
    }
    let mut remote = lock(&node.inner.cache.remote);
    if remote
        .get(&peer)
        .is_some_and(|(old_boot, old, _)| *old_boot == boot && *old >= revision)
    {
        return;
    }
    remote.insert(peer, (boot, revision, wire));
    drop(remote);
    emit(node);
}
async fn local(
    node: &Node,
    clear: bool,
    authority: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> api::ApiResult<(CacheUsage, Option<CacheClear>)> {
    let _scan = node.inner.cache.scan.lock().await;
    let data = node.cache_dir();
    let native = node.inner.cache.native;
    let clipboard = node.clone();
    let start = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        if authority.is_some_and(|valid| !valid()) {
            return None;
        }
        let current = if clear {
            clipboard.cache_clipboard_paths()
        } else {
            Ok(Vec::new())
        };
        let can_clear = clear && current.is_ok();
        let _current: Vec<_> = current
            .unwrap_or_default()
            .into_iter()
            .map(oink_clipboard::cache::SourceLease::current)
            .collect();
        let mut usage = CacheUsage::default();
        if clear && !can_clear {
            usage.incomplete = true;
        }
        let mut removed = 0;
        #[cfg(target_os = "macos")]
        let _provider_pins;
        #[cfg(target_os = "macos")]
        if native {
            let (categories, freed, incomplete, pins) = oink_clipboard::file_provider::cache_usage(can_clear);
            _provider_pins = pins;
            usage.categories = categories;
            removed += freed;
            usage.incomplete |= incomplete;
        }
        let _ = native;
        for kind in oink_clipboard::store::StoreKind::ALL {
            let (size, freed, incomplete) = oink_clipboard::store::usage(&data, kind, can_clear);
            usage.categories.push(size);
            removed += freed;
            usage.incomplete |= incomplete;
        }
        // Native deliveries live exclusively in the content store. These
        // categories have no independent storage producer in this build.
        for category in [
            CacheCategory::ClipboardFiles,
            CacheCategory::DragFiles,
            CacheCategory::DeliveredFiles,
        ] {
            usage.categories.push(CacheSize {
                category,
                bytes: 0,
                kept_bytes: 0,
            });
        }
        if !native {
            for category in [
                CacheCategory::ProviderCatalog,
                CacheCategory::ProviderMaterialized,
                CacheCategory::ProviderTemporary,
            ] {
                usage.categories.push(CacheSize {
                    category,
                    bytes: 0,
                    kept_bytes: 0,
                });
            }
        }
        if clear && !can_clear {
            for c in &mut usage.categories {
                c.kept_bytes = c.bytes;
            }
        }
        usage.total_bytes = usage.categories.iter().map(|c| c.bytes).sum();
        Some((usage, removed))
    })
    .await
    .map_err(|_| unavailable())?;
    let (usage, removed) = result.ok_or_else(unavailable)?;
    let mut cached = lock(&node.inner.cache.local);
    if cached.1 != usage {
        cached.0 += 1;
        cached.1 = usage.clone();
        drop(cached);
        emit(node);
        node.inner.cache.changed.notify_one();
    }
    let result = clear.then(|| CacheClear {
        removed_bytes: removed,
        kept_bytes: usage.categories.iter().map(|c| c.kept_bytes).sum(),
        usage: usage.clone(),
    });
    if clear {
        tracing::info!(
            removed_bytes = removed,
            kept_bytes = usage.total_bytes,
            duration_ms = start.elapsed().as_millis() as u64,
            "cache clear completed"
        );
    }
    Ok((usage, result))
}
fn options() -> oink_foundation::transfer::LowOptions {
    oink_foundation::transfer::LowOptions {
        send_max: 32 * 1024,
        receive_max: 32 * 1024,
        operation_max: 64 * 1024,
    }
}
async fn request(node: &Node, peer: DeviceId, clear: bool) -> api::ApiResult<(CacheUsage, Option<CacheClear>)> {
    let links = node.links().map_err(|_| unavailable())?;
    let entry = links
        .links()
        .by_peer(peer)
        .filter(|e| e.state == oink_network::link::LinkState::Ready && e.agreement.contains("oink.cache/1"))
        .cloned()
        .ok_or_else(unavailable)?;
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(|_| unavailable())?;
    let request = dto::CacheRequest::from_raw(
        [
            (1, V::Bool(clear)),
            (2, V::Bytes(id.to_vec())),
            (3, boot(entry.peer_boot).write()),
        ]
        .into(),
    )
    .map_err(|_| unavailable())?;
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut stream = links
            .open_low(entry.link, StreamKind::Cache, options())
            .await
            .map_err(|_| unavailable())?;
        stream
            .send(&Message::CacheRequest(request))
            .await
            .map_err(|_| unavailable())?;
        stream.finish();
        let Some(Message::CacheResult(result)) = stream.recv().await.map_err(|_| unavailable())? else {
            return Err(unavailable());
        };
        if result.request_id().map_err(|_| unavailable())?.0 != id
            || stream.recv().await.map_err(|_| unavailable())?.is_some()
            || !stream.effect_validity()()
        {
            return Err(unavailable());
        }
        let usage = project(&result.usage().map_err(|_| unavailable())?).map_err(|_| unavailable())?;
        let result = result
            .clear()
            .map_err(|_| unavailable())?
            .map(|v| clear_projection(&v))
            .transpose()
            .map_err(|_| unavailable())?;
        if result.is_some() != clear {
            return Err(unavailable());
        }
        Ok((usage, result))
    })
    .await
    .map_err(|_| unavailable())?
}
pub(crate) async fn query(
    node: Node,
    device: Option<DeviceKey>,
    clear: bool,
) -> api::ApiResult<(Vec<CacheDevice>, Option<CacheClear>)> {
    let id = device.map_or(node.id(), |d| d.0);
    let (usage, result) = if id == node.id() {
        local(&node, clear, None).await?
    } else {
        request(&node, id, clear).await?
    };
    if clear || device.is_some() {
        return Ok((
            vec![CacheDevice {
                device: id.into(),
                usage,
            }],
            result,
        ));
    }
    // On demand refresh each ready device. A failed peer remains absent/unchanged;
    // local accounting and other members still succeed.
    let links = node.links().map_err(|_| unavailable())?;
    let mut jobs = tokio::task::JoinSet::new();
    for link in links
        .links()
        .links
        .iter()
        .filter(|l| l.state == oink_network::link::LinkState::Ready)
    {
        let n = node.clone();
        let peer = link.peer;
        jobs.spawn(async move {
            request(&n, peer, false).await.map(|(usage, _)| CacheDevice {
                device: peer.into(),
                usage,
            })
        });
    }
    let mut list = vec![CacheDevice {
        device: id.into(),
        usage,
    }];
    while let Some(result) = jobs.join_next().await {
        if let Ok(Ok(device)) = result {
            list.push(device);
        }
    }
    Ok((list, result))
}
async fn publish(node: Node, _peer: DeviceId, link: LinkId) {
    let (revision, usage) = lock(&node.inner.cache.local).clone();
    let Ok(usage) = wire_usage(&usage) else {
        return;
    };
    let Ok(value) = dto::CacheUsage::from_raw([(1, V::Uint(revision)), (2, usage.write())].into()) else {
        return;
    };
    if let Ok(links) = node.links()
        && let Ok(mut stream) = links.open_low(link, StreamKind::Cache, options()).await
        && stream.send(&Message::CacheUsage(value)).await.is_ok()
    {
        stream.finish();
        let _ = stream.recv().await;
    }
}
pub(crate) fn start(node: Node, mut port: ModulePort) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut changes = oink_clipboard::cache::subscribe();
        let mut stop = node.stop_receiver();
        let changed = node.inner.cache.changed.clone();
        let mut peers = HashMap::new();
        let mut jobs = tokio::task::JoinSet::new();
        let _ = local(&node, false, None).await;
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                _ = changed.notified() => for (&peer, &link) in &peers { jobs.spawn(publish(node.clone(), peer, link)); },
                event = changes.recv() => {
                    let relevant = match event {
                        Ok(root) => root.starts_with(node.cache_dir()) || {
                            #[cfg(target_os = "macos")] { node.inner.cache.native && oink_clipboard::file_provider::is_cache_root(&root) }
                            #[cfg(not(target_os = "macos"))] { false }
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                        Err(_) => break,
                    };
                    if relevant {
                        while changes.try_recv().is_ok() {}
                        let _ = local(&node, false,None).await;
                    }
                },
                _ = jobs.join_next(), if !jobs.is_empty() => {},
                event = port.next() => match event {
                    Some(ModuleEvent::LinkReady{peer, link}) => {
                        peers.insert(peer, link);
                        lock(&node.inner.cache.remote).remove(&peer);
                        jobs.spawn(publish(node.clone(), peer, link));
                    },
                    Some(ModuleEvent::LinkLost{peer, link,..}) => if peers.get(&peer) == Some(&link) {
                        peers.remove(&peer); lock(&node.inner.cache.remote).remove(&peer); emit(&node);
                    },
                    Some(ModuleEvent::Inbound(stream)) => stream.reject(StreamCode::Unsupported),
                    Some(ModuleEvent::InboundLow(mut operation)) => {
                        let peer = operation.stream.peer();
                        if peers.get(&peer) != Some(&operation.stream.link()) { operation.stream.cancel(); continue; }
                        let Ok(links)=node.links()else{operation.stream.cancel();continue;};
                        let Some(entry)=links.links().by_peer(peer).filter(|e|e.link==operation.stream.link()).cloned()else{operation.stream.cancel();continue;};
                        match operation.first.clone(){
                            Message::CacheUsage(value)=>{
                                if let(Ok(revision),Ok(usage))=(value.revision(),value.usage()){set_remote(&node,peer,entry.peer_boot,revision,usage);}
                                operation.stream.finish();jobs.spawn(async move{let _=operation.stream.recv().await;});
                            }
                            Message::CacheRequest(request)=>{
                                let n=node.clone();
                                jobs.spawn(async move{
                                    let Ok(id)=request.request_id()else{return;};let Ok(clear)=request.clear()else{return;};
                                    if request.target()!=Ok(boot(links.boot())){operation.stream.cancel();return;}
                                    let valid=operation.stream.effect_validity();
                                    let mut replies=n.inner.cache.replies.lock().await;
                                    let result=if let Some((_,_,old_clear,result))=replies.iter().find(|(source,old,_,_)|*source==entry.peer_boot&&*old==id.0){
                                        if *old_clear!=clear{operation.stream.cancel();return;}result.clone()
                                    }else{
                                        if !valid(){operation.stream.cancel();return;}
                                        let Ok((usage,result))=local(&n,clear,Some(valid.clone())).await else{operation.stream.cancel();return;};
                                        let Ok(usage)=wire_usage(&usage)else{return;};
                                        let Ok(clear)=result.as_ref().map(wire_clear).transpose()else{return;};
                                        let mut raw=v1::RawMap::from([(1,usage.write()),(3,id.write())]);if let Some(clear)=clear{raw.insert(2,clear.write());}
                                        let Ok(result)=dto::CacheResult::from_raw(raw)else{return;};
                                        if replies.len()>=128{replies.pop_front();}replies.push_back((entry.peer_boot,id.0,request.clear().unwrap(),result.clone()));result
                                    };
                                    drop(replies);
                                    if valid(){let _=operation.stream.send(&Message::CacheResult(result)).await;operation.stream.finish();let _=operation.stream.recv().await;}
                                });
                            }
                            _=>operation.stream.cancel(),
                        }
                    },
                    Some(_) => {}, None => break,
                },
            }
        }
    })
}
