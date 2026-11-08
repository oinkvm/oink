use super::*;
unsafe extern "C" {
    fn oink_unlock_start(auth: extern "C" fn(HANDLE, u32) -> HANDLE) -> u32;
    fn oink_unlock_stop();
    fn oink_unlock_wake();
    fn oink_unlock_session(kind: u32, session: u32) -> bool;
    fn oink_unlock_register(dll: *const u16, remove: bool) -> u32;
}
extern "C" fn authenticate(pipe: HANDLE, session: u32) -> HANDLE {
    // Native worker calls only after reading a complete request. Retain the
    // authenticated creation-object handle through this check; native processing
    // remains bound to this connected pipe and rechecks the current console.
    match security::peer(pipe, session, false) {
        Ok(handle) => {
            let raw = handle.0;
            std::mem::forget(handle);
            raw
        }
        Err(_) => std::ptr::null_mut(),
    }
}
fn result(code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}
pub(super) fn register(remove: bool) -> io::Result<()> {
    let path = security::install_dir()?.join("OinkCredentialProvider.dll");
    if !remove {
        security::protected_file(&path)?;
    }
    let path = wide(path.to_str().ok_or_else(denied)?);
    // SAFETY: native registration uses only the fixed private CLSID.
    result(unsafe { oink_unlock_register(path.as_ptr(), remove) })
}
pub(super) fn start() -> io::Result<()> {
    register(false)?;
    // SAFETY: function pointer remains live throughout the native worker.
    result(unsafe { oink_unlock_start(authenticate) })
}
pub(super) fn stop() {
    // SAFETY: no Rust-owned memory crosses the call; the native side owns its worker.
    unsafe { oink_unlock_stop() };
}
pub(super) fn session_event(kind: u32, session: u32) {
    // SAFETY: scalar notification values cross the ABI; the native side owns its state.
    let own = unsafe { oink_unlock_session(kind, session) };
    if own {
        tracing::info!(session, "auto unlock own event suppressed");
    }
}

static EVENTS: std::sync::LazyLock<crossbeam_queue::ArrayQueue<(u32, u32)>> =
    std::sync::LazyLock::new(|| crossbeam_queue::ArrayQueue::new(64));
pub(super) fn queue_event(kind: u32, session: u32) {
    let _ = EVENTS.push((kind, session));
    // SAFETY: native event is process-lived; this callback only signals it.
    unsafe { oink_unlock_wake() };
}
pub(super) fn drain_events() {
    while let Some((kind, session)) = EVENTS.pop() {
        session_event(kind, session);
    }
}
