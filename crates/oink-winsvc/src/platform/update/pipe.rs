use super::super::{Handle, check, denied, pipe_io, security, session, token, wide};
use super::{protected, service};
use oink_update::{
    Error,
    ipc::{self, Reply, Request},
};
use std::{
    io, ptr,
    sync::{Arc, Mutex, atomic::Ordering},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Storage::FileSystem::*,
    System::{Pipes::*, RemoteDesktop::*, Threading::*},
};
fn name(id: u32) -> Vec<u16> {
    wide(&format!(r"\\.\pipe\Oink.Update.v1.{id}"))
}
fn send(pipe: &Handle, value: &impl serde::Serialize) -> io::Result<()> {
    let bytes = ipc::encode(value).map_err(io::Error::other)?;
    let mut operation = pipe_io::Operation::new(pipe)?;
    // SAFETY: bytes outlives the operation, including cancellation on every error.
    unsafe {
        operation.write(&bytes)?;
    }
    if operation.wait(5000)? as usize != bytes.len() {
        return Err(denied());
    }
    Ok(())
}
fn receive<T: serde::de::DeserializeOwned + serde::Serialize>(pipe: &Handle) -> io::Result<T> {
    let mut bytes = vec![0u8; ipc::MAX_FRAME];
    let mut operation = pipe_io::Operation::new(pipe)?;
    // SAFETY: bounded storage outlives pending I/O and its cancellation.
    unsafe {
        operation.read(&mut bytes)?;
    }
    let count = operation.wait(5000)? as usize;
    ipc::decode(&bytes[..count]).map_err(io::Error::other)
}
pub(super) fn request(request: Request) -> Result<Reply, Error> {
    let id = session().map_err(|_| Error::Policy)?;
    // SAFETY: fixed local pipe; no inherited handles, remote servers or caller-defined security.
    let pipe = Handle::new(unsafe {
        CreateFileW(
            name(id).as_ptr(),
            FILE_READ_DATA | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | READ_CONTROL,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ptr::null_mut(),
        )
    })
    .map_err(|_| Error::NotConfigured)?;
    let mode = PIPE_READMODE_MESSAGE | PIPE_WAIT;
    // SAFETY: connected owned pipe, valid mode.
    check(unsafe { SetNamedPipeHandleState(pipe.0, &mode, ptr::null(), ptr::null()) }).map_err(|_| Error::Policy)?;
    super::super::service_peer::verify_update(pipe.0).map_err(|_| Error::Policy)?;
    send(&pipe, &request).map_err(|_| Error::Transaction)?;
    receive(&pipe).map_err(|_| Error::Transaction)
}
pub(super) fn sessions() -> io::Result<Vec<u32>> {
    let (mut raw, mut count) = (ptr::null_mut(), 0);
    // SAFETY: OS supplies array; copied before WTSFreeMemory. Include disconnected
    // logged-on sessions as well as the active console; they may own live work.
    unsafe {
        check(WTSEnumerateSessionsW(
            WTS_CURRENT_SERVER_HANDLE,
            0,
            1,
            &mut raw,
            &mut count,
        ))?;
        let ids = std::slice::from_raw_parts(raw, count as usize)
            .iter()
            .filter(|s| s.SessionId != 0 && security::user_sid(s.SessionId).is_ok())
            .map(|s| s.SessionId)
            .collect();
        WTSFreeMemory(raw.cast());
        Ok(ids)
    }
}
pub(super) fn serve(id: u32, state: Arc<Mutex<service::State>>) -> io::Result<()> {
    let user = security::user_sid(id)?;
    let descriptor = security::pipe_security(id)?;
    let attrs = descriptor.attrs();
    // SAFETY: SYSTEM-owned first local instance with exact session user DACL.
    let pipe = Handle::new(unsafe {
        CreateNamedPipeW(
            name(id).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            ipc::MAX_FRAME as u32,
            ipc::MAX_FRAME as u32,
            0,
            &attrs,
        )
    })?;
    while !super::super::broker::STOP.load(Ordering::Acquire) && security::user_sid(id).ok().as_ref() == Some(&user) {
        let mut operation = pipe_io::Operation::new(&pipe)?;
        operation.connect()?;
        loop {
            // SAFETY: non-owning wait on event owned by operation.
            let ready = unsafe { WaitForSingleObject(operation.event(), 250) };
            if ready == WAIT_OBJECT_0 {
                break;
            }
            if ready != WAIT_TIMEOUT
                || super::super::broker::STOP.load(Ordering::Acquire)
                || security::user_sid(id).ok().as_ref() != Some(&user)
            {
                return Ok(());
            }
        }
        operation.cancel();
        drop(operation);
        let reply = (|| {
            let request = receive::<Request>(&pipe)?;
            let process = peer(pipe.0, id)?;
            let reply = service::dispatch(&state, id, &process, request);
            Ok::<_, io::Error>(reply)
        })()
        .unwrap_or(Reply::Error { error: Error::Policy });
        let _ = send(&pipe, &reply);
        // A short read waits for close so DisconnectNamedPipe cannot discard reply.
        let mut ack = [0u8; 1];
        let mut operation = pipe_io::Operation::new(&pipe)?;
        // SAFETY: buffer remains live until operation is dropped/cancelled below.
        if unsafe { operation.read(&mut ack) }.is_ok() {
            let _ = operation.wait(1000);
        }
        drop(operation);
        // SAFETY: no pending operations remain.
        unsafe {
            DisconnectNamedPipe(pipe.0);
        }
    }
    Ok(())
}
fn peer(pipe: HANDLE, id: u32) -> io::Result<Handle> {
    let (mut pid, mut actual_session) = (0, 0);
    // SAFETY: connected pipe, kernel-reported identity; output DWORDs valid.
    unsafe {
        check(GetNamedPipeClientProcessId(pipe, &mut pid))?;
        check(GetNamedPipeClientSessionId(pipe, &mut actual_session))?;
    }
    if actual_session != id {
        return Err(denied());
    }
    // SAFETY: pins PID against reuse for the entire dispatch.
    let process = Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) })?;
    let path = security::image(process.0)?;
    let root = protected::root()?;
    if !["oink.exe", "oink-agent.exe"]
        .iter()
        .any(|n| root.join(n).as_os_str().eq_ignore_ascii_case(path.as_os_str()))
    {
        return Err(denied());
    }
    let _image = protected::open(&path)?;
    let actual = security::sid_bytes(token(process.0)?.0)?;
    if actual != security::user_sid(id)? {
        return Err(denied());
    }
    // SAFETY: impersonate only to read identity; always revert before privileged work.
    unsafe {
        check(ImpersonateNamedPipeClient(pipe))?;
        let mut raw = ptr::null_mut();
        let result = check(OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw))
            .and_then(|()| security::sid_bytes(Handle::new(raw)?.0));
        if RevertToSelf() == 0 {
            std::process::abort();
        }
        if result? != actual {
            return Err(denied());
        }
    }
    Ok(process)
}
pub(super) fn elevated(process: &Handle) -> bool {
    let Ok(token) = token(process.0) else {
        return false;
    };
    let (mut value, mut length) = (TOKEN_ELEVATION::default(), 0);
    // SAFETY: query-only token and correctly sized writable structure.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            ptr::addr_of_mut!(value).cast(),
            size_of_val(&value) as u32,
            &mut length,
        ) != 0
            && value.TokenIsElevated != 0
    }
}
