//! A live clipboard/drag publication upgrades metadata authority once, then
//! keeps native received-file credentials independent of that offer's lifetime.
use super::*;
use crate::content::namespace::NativeFiles;
use std::sync::{Mutex, Weak};

pub struct Publication {
    offer: OfferId,
    roots: Vec<FileNode>,
    link: Arc<dyn BulkLink>,
    runtime: tokio::runtime::Handle,
    demand: Mutex<Demand>,
    serial: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}
#[derive(Default)]
struct State {
    linked: Option<Linked>,
    native: Vec<Weak<dyn NativeFiles>>,
    cancelled: bool,
}
impl std::fmt::Debug for Publication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilePublication")
            .field("roots", &self.roots.len())
            .finish()
    }
}
impl Publication {
    #[cfg(debug_assertions)]
    pub(crate) fn watchdog_snapshot_for_test(&self) -> String {
        let state = self.state.try_lock().map(|state| {
            (
                state.cancelled,
                state.linked.as_ref().map(|linked| {
                    let descriptor = linked.descriptor();
                    (
                        descriptor.provisional,
                        descriptor.roots.len(),
                        descriptor.fulfilled.len(),
                    )
                }),
                state.native.iter().map(Weak::strong_count).collect::<Vec<_>>(),
            )
        });
        format!(
            "publication state={state:?} metadata_request_pending={} demand={:?}",
            self.serial.try_lock().is_err(),
            self.demand.try_lock()
        )
    }

    pub fn new(
        offer: OfferId,
        roots: Vec<FileNode>,
        link: Arc<dyn BulkLink>,
        demand: Demand,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            offer,
            roots,
            link,
            runtime,
            demand: Mutex::new(demand),
            serial: tokio::sync::Mutex::new(()),
            state: Mutex::default(),
            changed: tokio::sync::Notify::new(),
        }
    }
    pub fn set_demand(&self, demand: Demand) {
        *self.demand.lock().unwrap() = demand;
    }
    pub async fn prepare(&self) -> io::Result<Linked> {
        let _serial = self.serial.lock().await;
        let demand = *self.demand.lock().unwrap();
        {
            let state = self.state.lock().unwrap();
            if let Some(linked) = &state.linked
                && (!linked.descriptor().provisional || matches!(demand, Demand::Hover(_)))
            {
                return Ok(linked.clone());
            }
            if state.cancelled {
                return Err(unavailable(Unavailable::Cancelled));
            }
        }
        let linked = Linked::acquire(
            self.offer,
            self.roots.iter().map(|root| root.id).collect(),
            demand,
            self.link.clone(),
        )
        .await?;
        let mut state = self.state.lock().unwrap();
        if state.cancelled {
            return Err(unavailable(Unavailable::Cancelled));
        }
        if let Some(previous) = &state.linked
            && previous.descriptor().token != linked.descriptor().token
        {
            return Err(unavailable(Unavailable::Permission));
        }
        // Publish authority to readers only after every native catalog has
        // committed it. A failed write leaves the old metadata-only view live.
        for native in state.native.iter().filter_map(Weak::upgrade) {
            native.persist_link(linked.descriptor())?;
        }
        state.linked = Some(linked.clone());
        Ok(linked)
    }
    pub fn descriptor(&self) -> Option<Descriptor> {
        self.state
            .lock()
            .unwrap()
            .linked
            .as_ref()
            .map(|linked| linked.descriptor().clone())
    }
    pub fn roots(&self) -> Vec<FileNode> {
        self.state
            .lock()
            .unwrap()
            .linked
            .as_ref()
            .map_or_else(|| self.roots.clone(), |linked| linked.descriptor().roots.clone())
    }
    pub fn ready(&self) -> io::Result<Linked> {
        self.runtime.block_on(self.prepare())
    }
    pub fn bind_native(&self, native: Weak<dyn NativeFiles>) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let owner = native.upgrade().ok_or_else(|| unavailable(Unavailable::Cancelled))?;
        let linked = state
            .linked
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        owner.persist_link(linked.descriptor())?;
        state.native.retain(|native| native.strong_count() != 0);
        if !state.native.iter().any(|existing| existing.ptr_eq(&native)) {
            state.native.push(native);
        }
        self.changed.notify_waiters();
        Ok(())
    }
    pub fn native(&self) -> io::Result<Arc<dyn NativeFiles>> {
        self.state
            .lock()
            .unwrap()
            .native
            .iter()
            .find_map(Weak::upgrade)
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))
    }
    pub async fn wait_native(&self) -> io::Result<Arc<dyn NativeFiles>> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state.lock().unwrap().cancelled {
                return Err(unavailable(Unavailable::Cancelled));
            }
            if let Ok(native) = self.native() {
                return Ok(native);
            }
            tokio::time::timeout(Duration::from_millis(NO_PROGRESS_MS), changed)
                .await
                .map_err(|_| unavailable(Unavailable::NoProgress))?;
        }
    }
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled = true;
        if let Some(linked) = &state.linked
            && linked.descriptor().provisional
        {
            linked.cancel();
        }
        self.changed.notify_waiters();
    }
}
