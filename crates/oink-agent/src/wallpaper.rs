//! Wallpaper changes publish a complete JPEG thumbnail set through Low; per-peer
//! delivery retries until the receiver acknowledges storing it, unless paused.
//! Images remain in memory; neither logs nor diagnostics include their contents.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::streams::{StreamModule, wallpaper::WallpaperStreams};
use base64::Engine;
use oink_display::DisplayBackend;
use oink_display::wallpaper::DisplayWallpaper;
use oink_foundation::DeviceId;
use oink_network::link::{ModuleEvent, ModulePort};
pub(crate) mod model;
use model::ScreenWallpaper;

use crate::api::{self, ApiEvent, DeviceKey, ScreenKey};
use crate::node::{Node, lock};

/// 一块屏幕的缩略图。
#[derive(Clone, PartialEq, Eq)]
struct Entry {
    display: String,
    width: u16,
    height: u16,
    jpeg: Vec<u8>,
    /// 内容哈希（十六进制，16 个字符）：去重，也是界面的缓存键。
    rev: String,
}

impl Entry {
    fn new(display: String, width: u16, height: u16, jpeg: Vec<u8>) -> Self {
        let rev = blake3::hash(&jpeg).to_hex()[..16].to_string();
        Self {
            display,
            width,
            height,
            jpeg,
            rev,
        }
    }

    fn same_content(a: &[Entry], b: &[Entry]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.display == y.display && x.rev == y.rev)
    }
}

/// 本机与各设备的缩略图（纯数据，便于测试）。
#[derive(Default)]
struct Store {
    /// `None`：还没读到过（刚启动、平台未实现）。
    local: Option<Vec<Entry>>,
    remote: HashMap<DeviceId, Vec<Entry>>,
}

impl Store {
    /// 记下本机刚读到的缩略图。返回是否有变化。
    fn set_local(&mut self, read: Vec<DisplayWallpaper>) -> bool {
        let mut next: Vec<Entry> = read
            .into_iter()
            .map(|w| Entry::new(w.key, w.thumbnail.width, w.thumbnail.height, w.thumbnail.jpeg))
            .collect();
        next.sort_by(|a, b| a.display.cmp(&b.display));
        if self.local.as_deref().is_some_and(|cur| Entry::same_content(cur, &next)) {
            return false;
        }
        self.local = Some(next);
        true
    }

    /// 记下 `peer` 发来的缩略图（整体替换）。无效的消息丢弃。返回是否有变化。
    fn set_remote_screens(&mut self, peer: DeviceId, screens: Vec<ScreenWallpaper>) -> bool {
        let mut next: Vec<Entry> = screens
            .into_iter()
            .map(|s| Entry::new(s.display, s.width, s.height, s.jpeg))
            .collect();
        next.sort_by(|a, b| a.display.cmp(&b.display));
        next.dedup_by(|a, b| a.display == b.display);
        if self
            .remote
            .get(&peer)
            .is_some_and(|current| Entry::same_content(current, &next))
        {
            return false;
        }
        self.remote.insert(peer, next);
        true
    }

    /// 发给其他设备的消息；本机还没读到过时没有。
    fn local_screens(&self) -> Option<Vec<ScreenWallpaper>> {
        let screens = self
            .local
            .as_ref()?
            .iter()
            .map(|e| ScreenWallpaper {
                display: e.display.clone(),
                width: e.width,
                height: e.height,
                jpeg: e.jpeg.clone(),
            })
            .collect();
        Some(screens)
    }

    /// 给界面的列表：本机的与各设备的。
    fn list(&self, me: DeviceId) -> Vec<api::ScreenWallpaper> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let local = self.local.iter().flatten().map(|e| (me, e));
        let remote = self
            .remote
            .iter()
            .filter(|(d, _)| **d != me)
            .flat_map(|(d, list)| list.iter().map(move |e| (*d, e)));
        let mut out: Vec<api::ScreenWallpaper> = local
            .chain(remote)
            .map(|(device, e)| api::ScreenWallpaper {
                key: ScreenKey {
                    device: DeviceKey(device),
                    display: e.display.clone(),
                },
                rev: e.rev.clone(),
                image: format!("data:image/jpeg;base64,{}", b64.encode(&e.jpeg)),
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }
}

/// 服务进程里的壁纸状态。
pub(crate) struct State {
    backend: Option<Arc<dyn DisplayBackend>>,
    store: Mutex<Store>,
    /// 正在读；期间又有请求时读完再读一次（`again`）。
    reading: AtomicBool,
    again: AtomicBool,
    changed: Arc<tokio::sync::Notify>,
}

impl State {
    pub(crate) fn new(backend: Option<Arc<dyn DisplayBackend>>) -> Self {
        Self {
            backend,
            store: Mutex::new(Store::default()),
            reading: AtomicBool::new(false),
            again: AtomicBool::new(false),
            changed: Default::default(),
        }
    }
}

/// 启动：注册显示器变化、桌面空间切换的回调，并读第一次。返回的任务随服务结束。
pub(crate) fn start(node: &Node) -> Option<tokio::task::JoinHandle<()>> {
    let backend = node.inner.wallpaper.backend.clone()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let notify = || -> oink_display::ChangeHandler {
        let tx = tx.clone();
        Arc::new(move || {
            let _ = tx.send(());
        })
    };
    if let Err(e) = backend.watch(notify()).and_then(|()| backend.watch_wallpaper(notify())) {
        tracing::debug!(%e, "wallpaper: change notifications unavailable");
    }
    let _ = tx.send(());
    let weak = Arc::downgrade(&node.inner);
    Some(tokio::spawn(watch_changes(rx, move || {
        if let Some(inner) = weak.upgrade() {
            refresh(&Node { inner });
        }
    })))
}

async fn watch_changes(mut rx: tokio::sync::mpsc::UnboundedReceiver<()>, refresh: impl Fn()) {
    while rx.recv().await.is_some() {
        // A display/Space callback means "may have changed", not that the OS's
        // wallpaper URL/image is ready. It may not send another callback once it
        // is ready. Retain this source's settling window before reading; the
        // resulting authoritative image still publishes immediately on change.
        let settle = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(settle);
        loop {
            tokio::select! {
                biased;
                notification = rx.recv() => match notification {
                    Some(()) => settle.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(500)),
                    None => return,
                },
                _ = &mut settle => break,
            }
        }
        refresh();
    }
}

/// 重新读本机的壁纸（不等结果）。有变化时通知界面并发给在线设备。
pub(crate) fn refresh(node: &Node) {
    let state = &node.inner.wallpaper;
    let Some(backend) = state.backend.clone() else {
        return;
    };
    if state.reading.swap(true, Ordering::AcqRel) {
        state.again.store(true, Ordering::Release);
        return;
    }
    let node = node.clone();
    node.inner.rt.clone().spawn(async move {
        loop {
            let b = backend.clone();
            let read = tokio::task::spawn_blocking(move || b.wallpapers()).await.ok().flatten();
            let state = &node.inner.wallpaper;
            let changed = read.is_some_and(|list| lock(&state.store).set_local(list));
            if changed {
                node.emit(ApiEvent::Wallpapers);
                state.changed.notify_one();
            }
            state.reading.store(false, Ordering::Release);
            if !state.again.swap(false, Ordering::AcqRel) || state.reading.swap(true, Ordering::AcqRel) {
                return;
            }
        }
    });
}

/// 界面取全部缩略图。
pub(crate) fn list(node: &Node) -> Vec<api::ScreenWallpaper> {
    lock(&node.inner.wallpaper.store).list(node.inner.me)
}

pub(crate) fn start_link_runtime(node: Node, mut port: ModulePort) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let links = node.links()?;
    let group = node.link_config()?;
    let mut changes = group.subscribe();
    let mut stop = node.stop_receiver();
    let mut streams = WallpaperStreams::new(links, group.clone());
    let changed = node.inner.wallpaper.changed.clone();
    let watcher = start(&node);
    if let Some(screens) = lock(&node.inner.wallpaper.store).local_screens() {
        streams.set_wallpapers(screens);
    }
    Ok(tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                _ = changed.notified() => {
                    let current = lock(&node.inner.wallpaper.store).local_screens();
                    if let Some(screens) = current { streams.set_wallpapers(screens); }
                },
                event = port.next() => match event {
                    Some(ModuleEvent::LinkReady { link, peer }) => streams.link_ready(link, peer),
                    Some(ModuleEvent::LinkLost { link, peer, reason }) => streams.link_lost(link, peer, reason),
                    Some(ModuleEvent::Inbound(stream)) => streams.inbound(stream),
                    Some(ModuleEvent::InboundLow(op)) => {
                        let peer = op.stream.peer();
                        streams.inbound_low(*op);
                        if let Some(screens) = streams.received(peer)
                            && lock(&node.inner.wallpaper.store).set_remote_screens(peer, screens.to_vec()) {
                            node.emit(ApiEvent::Wallpapers);
                        }
                    },
                    Some(ModuleEvent::StreamClosed { stream, reason }) => streams.stream_closed(stream, reason),
                    Some(ModuleEvent::PathChanged { .. }) | Some(ModuleEvent::ProtocolViolation { .. }) => {}, None => break,
                },
                change = changes.next() => match change {
                    Some(crate::config_module::ConfigChange::State { .. }) => streams.pause_changed(group.applied().paused),
                    Some(crate::config_module::ConfigChange::LocalDisplays) => {
                        refresh(&node);
                        streams.displays_changed();
                    },
                    Some(_) => {}, None => break,
                },
            }
        }
        if let Some(watcher) = watcher {
            watcher.abort();
        }
    }))
}

#[cfg(all(test, debug_assertions))]
mod notification_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn wallpaper_source_settles_when_the_final_image_has_no_second_notification() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let image = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads = Arc::new(Mutex::new(Vec::new()));
        let (current, received) = (image.clone(), reads.clone());
        let task = tokio::spawn(watch_changes(rx, move || {
            received.lock().unwrap().push(current.load(Ordering::Relaxed));
        }));
        // One early notification; the final image becomes readable later, without
        // another notification. An immediate read would retain the old image.
        tx.send(()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(250)).await;
        image.store(1, Ordering::Relaxed);
        tokio::time::advance(Duration::from_millis(249)).await;
        tokio::task::yield_now().await;
        assert!(reads.lock().unwrap().is_empty());
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(*reads.lock().unwrap(), vec![1]);

        // A new burst resets only the wallpaper source deadline.
        tx.send(()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(400)).await;
        image.store(2, Ordering::Relaxed);
        tx.send(()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(499)).await;
        tokio::task::yield_now().await;
        assert_eq!(*reads.lock().unwrap(), vec![1]);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(*reads.lock().unwrap(), vec![1, 2]);
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(*reads.lock().unwrap(), vec![1, 2], "idle has no polling");
        task.abort();
    }
}
