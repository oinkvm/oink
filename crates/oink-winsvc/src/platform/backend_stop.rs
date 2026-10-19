//! Read-only SCM notification: a planned broker stop also stops the user backend.
use super::*;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread::JoinHandle,
};
use windows_sys::Win32::System::Services::*;

struct Service(SC_HANDLE);
impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: owned SCM handle. Closing cancels the outstanding notification.
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

#[derive(Debug)]
pub struct StopWatch {
    cancel: Handle,
    worker: Option<JoinHandle<()>>,
}
// SAFETY: only a kernel event and JoinHandle cross threads; SCM handles and APC
// buffers stay on their registering worker. Drop joins before closing the event.
unsafe impl Send for StopWatch {}
impl Drop for StopWatch {
    fn drop(&mut self) {
        // SAFETY: the event outlives the worker and is used only for cancellation.
        unsafe {
            SetEvent(self.cancel.0);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// SAFETY: SCM calls this on the registering alertable thread with its live buffer.
unsafe extern "system" fn stopped(raw: *const core::ffi::c_void) {
    // SAFETY: SCM supplies the still-live registration buffer on this worker.
    let notification = unsafe { &*raw.cast::<SERVICE_NOTIFY_2W>() };
    // SAFETY: watch_status keeps this AtomicBool live until it cancels registration.
    let stop = unsafe { &*notification.pContext.cast::<AtomicBool>() };
    stop.store(true, Ordering::Release);
}

/// Registers before platform startup; no process launch, service control or polling.
pub fn watch(on_stop: impl FnOnce() + Send + 'static) -> io::Result<StopWatch> {
    watch_status(SERVICE_NOTIFY_STOP_PENDING | SERVICE_NOTIFY_STOPPED, on_stop)
}
fn watch_status(mask: u32, on_stop: impl FnOnce() + Send + 'static) -> io::Result<StopWatch> {
    // SAFETY: private unnamed manual-reset cancellation event, no inheritance.
    let cancel = Handle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
    // The owner joins before closing; the worker borrows this handle for waits only.
    let cancellation = cancel.0 as usize;
    let (ready, registered) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("oink-service-stop".into())
        .spawn(move || {
            let result = (|| -> io::Result<()> {
                // SAFETY: fixed local service, query-only access; no system mutation.
                let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
                if manager.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let manager = Service(manager);
                // SAFETY: live SCM handle, fixed service and read-only query rights.
                let service = unsafe { OpenServiceW(manager.0, wide("OinkInput").as_ptr(), SERVICE_QUERY_STATUS) };
                if service.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let service = Service(service);
                let stop = AtomicBool::new(false);
                let mut notification = SERVICE_NOTIFY_2W {
                    dwVersion: SERVICE_NOTIFY_STATUS_CHANGE,
                    pfnNotifyCallback: Some(stopped),
                    pContext: ptr::addr_of!(stop).cast_mut().cast(),
                    ..Default::default()
                };
                // SAFETY: buffer remains on this alertable thread until service closes.
                let error = unsafe { NotifyServiceStatusChangeW(service.0, mask, ptr::addr_of_mut!(notification)) };
                if error != 0 {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
                let _ = ready.send(Ok(()));
                loop {
                    // SAFETY: owner's cancellation event remains live through this join.
                    let result = unsafe { WaitForSingleObjectEx(cancellation as HANDLE, INFINITE, 1) };
                    if stop.load(Ordering::Acquire) {
                        on_stop();
                        break;
                    }
                    if result != WAIT_IO_COMPLETION {
                        break;
                    }
                }
                // Explicitly cancel before the notification/context stack storage drops.
                drop(service);
                Ok(())
            })();
            if let Err(error) = result {
                let _ = ready.send(Err(error));
            }
        })?;
    let watch = StopWatch {
        cancel,
        worker: Some(worker),
    };
    registered.recv().map_err(io::Error::other)??;
    Ok(watch)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn scm_callback_requests_stop() {
        let stop = AtomicBool::new(false);
        let notification = SERVICE_NOTIFY_2W {
            pContext: ptr::addr_of!(stop).cast_mut().cast(),
            ..Default::default()
        };
        // SAFETY: exactly the buffer/context lifetime and ABI used by registration.
        unsafe {
            stopped(ptr::addr_of!(notification).cast());
        }
        assert!(stop.load(Ordering::Acquire));
    }
    #[test]
    fn scm_notification_can_be_cancelled_without_stopping_service() {
        if std::env::var_os("OINK_SCM_STOP_NATIVE_TEST").is_none() {
            return;
        }
        let called = std::sync::Arc::new(AtomicBool::new(false));
        let copy = called.clone();
        let watch = watch(move || {
            copy.store(true, Ordering::Release);
        })
        .unwrap();
        drop(watch);
        assert!(!called.load(Ordering::Acquire));
    }
    #[test]
    fn scm_running_notification_delivers_a_real_apc_without_changing_service() {
        if std::env::var_os("OINK_SCM_STOP_NATIVE_TEST").is_none() {
            return;
        }
        let (send, receive) = std::sync::mpsc::channel();
        // SCM immediately reports an already-running service through the same APC
        // path as STOP_PENDING. No service start/stop or installed app is involved.
        let watch = watch_status(SERVICE_NOTIFY_RUNNING, move || {
            let _ = send.send(());
        })
        .unwrap();
        receive.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        drop(watch);
    }
}
