//! Explicit cross-process invalidation from the File Provider's mutation owners.
//! notify(3) coalesces wakeups; consumers reconcile the entire root. Unlike
//! FSEvents this does not infer mutations from filesystem event history.
use block2::{Block, RcBlock};
use dispatch2::{DispatchQueue, DispatchRetained};
use std::{
    ffi::{CStr, c_char},
    path::Path,
};

const NAME: &CStr = c"com.hachiwii.oink.cache-usage.changed";

unsafe extern "C" {
    fn notify_register_dispatch(
        name: *const c_char,
        token: *mut i32,
        queue: &DispatchQueue,
        handler: &Block<dyn Fn(i32)>,
    ) -> u32;
    fn notify_cancel(token: i32) -> u32;
}

pub(super) struct OwnerWatch {
    token: i32,
    queue: DispatchRetained<DispatchQueue>,
}
impl OwnerWatch {
    pub(super) fn new(root: &Path) -> Result<Self, String> {
        let root = root.to_owned();
        Self::register(NAME, move || crate::cache::changed(&root))
    }
    fn register(name: &CStr, changed: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
        let queue = DispatchQueue::new("com.hachiwii.oink.cache-owner", None);
        let handler = RcBlock::new(move |_: i32| changed());
        let mut token = 0;
        // SAFETY: notify.h copies the block and retains the queue until cancel.
        // The block owns its root, captures no borrowed state and only publishes.
        let status = unsafe { notify_register_dispatch(name.as_ptr(), &mut token, &queue, &handler) };
        if status != 0 {
            return Err(format!("cache owner notification unavailable ({status})"));
        }
        Ok(Self { token, queue })
    }
}
impl Drop for OwnerWatch {
    fn drop(&mut self) {
        // SAFETY: this object owns the successful registration. Queued callbacks
        // own their captures; draining does not run from the callback itself.
        unsafe { notify_cancel(self.token) };
        self.queue.exec_sync(|| {});
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn explicit_owner_post_reaches_dispatch_without_filesystem_activity() {
        let (send, receive) = std::sync::mpsc::channel();
        let name = std::ffi::CString::new(format!("com.hachiwii.oink.cache-test.{}", std::process::id())).unwrap();
        let _watch = OwnerWatch::register(&name, move || {
            let _ = send.send(());
        })
        .unwrap();
        // Real cross-process notifyd delivery, with no filesystem mutations.
        assert!(
            std::process::Command::new("/usr/bin/notifyutil")
                .args(["-p", name.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        receive.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    }
}
