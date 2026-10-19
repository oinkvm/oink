//! A grant's physical resources; no session, initialization or cleanup receipt.
use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct PoolKey {
    pub peer: DeviceId,
    pub connection: ConnectionId,
}
pub(super) struct Gate {
    pub closed: watch::Sender<bool>,
    resources: Mutex<Vec<Weak<io::Life>>>,
}
impl Gate {
    pub fn new() -> Self {
        Self {
            closed: watch::channel(false).0,
            resources: Mutex::new(vec![]),
        }
    }
    pub fn bind(&self, life: Arc<io::Life>) -> Result<(), NetError> {
        let mut resources = lock(&self.resources);
        if *self.closed.borrow() {
            drop(resources);
            life.close();
            return Err(NetError::Closed);
        }
        resources.retain(|r| r.strong_count() != 0);
        resources.push(Arc::downgrade(&life));
        Ok(())
    }
    pub fn with_current<T>(&self, action: impl FnOnce() -> Option<T>) -> Option<T> {
        let _resources = lock(&self.resources);
        if *self.closed.borrow() { None } else { action() }
    }
    pub fn cancel(&self) {
        let resources = {
            let mut resources = lock(&self.resources);
            if self.closed.send_replace(true) {
                return;
            }
            std::mem::take(&mut *resources)
        };
        for life in resources.into_iter().filter_map(|r| r.upgrade()) {
            life.close();
        }
    }
}
