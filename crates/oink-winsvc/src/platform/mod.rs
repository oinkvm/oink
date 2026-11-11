//! Windows handles remain thread-owned; none are borrowed across worker lifetimes.
mod auto_unlock;
pub mod backend;
mod backend_stop;
mod broker;
pub mod client;
mod console;
mod pipe_io;
mod power;
pub mod prelogin;
mod prelogin_peer;
mod security;
mod server;
mod service_peer;
pub mod update;
mod wake;
use std::{io, ptr};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{RemoteDesktop::ProcessIdToSessionId, Threading::*},
};
#[derive(Debug)]
struct Handle(HANDLE);
impl Handle {
    fn duplicate(&self) -> io::Result<Self> {
        let mut copy = ptr::null_mut();
        // SAFETY: source remains owned by self throughout duplication; the
        // result is a separate creation-object reference, never a PID reopen.
        check(unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.0,
                GetCurrentProcess(),
                &mut copy,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        })?;
        Self::new(copy)
    }
    fn new(h: HANDLE) -> io::Result<Self> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(h))
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns one valid kernel handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn context(api: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{api}: {error}"))
}
fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "input service identity verification failed",
    )
}
fn token(process: HANDLE) -> io::Result<Handle> {
    let mut h = ptr::null_mut();
    // SAFETY: process is live and h is an output handle.
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut h) })
        .map_err(|e| context("OpenProcessToken(TOKEN_QUERY)", e))?;
    Handle::new(h)
}
fn session() -> io::Result<u32> {
    let mut id = 0;
    // SAFETY: valid output pointer for own process ID.
    check(unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) })?;
    Ok(id)
}
fn pipe_name(id: u32) -> Vec<u16> {
    wide(&format!(r"\\.\pipe\Oink.Input.v1.{id}"))
}
fn prelogin_pipe_name(id: u32) -> Vec<u16> {
    wide(&format!(r"\\.\pipe\Oink.Prelogin.Input.v1.{id}"))
}
pub fn run() -> io::Result<()> {
    security::verify_self()?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => broker::run(),
        [arg] if arg == "--unregister-credential-provider" => auto_unlock::unregister(),
        [arg, pid] if arg == "--helper" => server::run(pid.parse().map_err(|_| denied())?, None),
        [arg, encoded] if arg == "--routed-helper" => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| denied())?;
            let source: crate::prelogin_route::Source = serde_json::from_slice(&bytes).map_err(|_| denied())?;
            server::run(source.identity.parent, Some(source))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no public command-line options",
        )),
    }
}
