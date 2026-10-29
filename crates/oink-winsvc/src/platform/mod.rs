//! Windows handles remain thread-owned; none are borrowed across worker lifetimes.
pub mod backend;
mod backend_stop;
mod broker;
pub mod client;
mod pipe_io;
mod power;
mod security;
mod server;
mod service_peer;
pub mod update;
use std::{io, ptr};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{RemoteDesktop::ProcessIdToSessionId, Threading::*},
};
#[derive(Debug)]
struct Handle(HANDLE);
impl Handle {
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
pub fn run() -> io::Result<()> {
    security::verify_self()?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => broker::run(),
        [arg, pid] if arg == "--helper" => server::run(pid.parse().map_err(|_| denied())?),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no public command-line options",
        )),
    }
}
