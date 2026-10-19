//! Single authenticated connection shared by injection, probes and keepalive.
use super::*;
use crate::owner::{Client, Transport};
use crate::wire::{Command, FRAME_SIZE, REPLY_SIZE, Reply, Request};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use windows_sys::Win32::{Storage::FileSystem::*, System::Pipes::*};
#[derive(Clone, Debug)]
pub struct ServiceStatus {
    pub pid: u32,
    pub session: u32,
    pub desktop: String,
    pub x: i32,
    pub y: i32,
    pub caps_lock: bool,
    pub helper_priority: i32,
}
static HELPER_PRIORITY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);
/// Last read-back carried by a helper reply; no pipe operation or lock is taken.
pub fn helper_priority() -> Option<i32> {
    let value = HELPER_PRIORITY.load(std::sync::atomic::Ordering::Acquire);
    (value != i32::MIN).then_some(value)
}

/// Select the default input backend without opening any pipe. Ordinary per-user
/// and development images return false. Expected protected images with unsafe
/// ACLs return an error; callers must not turn that error into local fallback.
pub fn service_installation() -> io::Result<bool> {
    security::client_installation()
}
struct Connection {
    // Cancel the pending read before releasing its buffer/pipe.
    reader: pipe_io::OwnedOperation,
    response: Box<[u8; REPLY_SIZE]>,
    writer: pipe_io::OwnedOperation,
    frame: Box<[u8; FRAME_SIZE]>,
    outgoing: crossbeam_queue::ArrayQueue<Request>,
    sending: Option<(u32, std::time::Instant)>,
    _pipe: pipe_io::SharedPipe,
    _peer: Handle,
}
// SAFETY: the single owner transfers these pinned I/O buffers, never shares them.
unsafe impl Send for Connection {}
struct Endpoint {
    connection: Option<Connection>,
    seen: bool,
    wake: pipe_io::Signal,
}
static OWNER: Mutex<Option<Arc<Client>>> = Mutex::new(None);
static PAUSED: AtomicBool = AtomicBool::new(false);
pub fn initialize() -> io::Result<()> {
    PAUSED.store(false, Ordering::Release);
    owner().map(|_| ())
}
fn owner() -> io::Result<Arc<Client>> {
    let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
    if PAUSED.load(Ordering::Acquire) {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "input session paused"));
    }
    if owner.is_none() {
        *owner = Some(Arc::new(create_owner()?));
    }
    Ok(owner.as_ref().unwrap().clone())
}
/// Stop the pipe owner and disconnect its helper. Observers cannot recreate it
/// until an explicit fresh input session is initialized on resume.
pub fn suspend() {
    let owner = {
        let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
        PAUSED.store(true, Ordering::Release);
        owner.take()
    };
    if let Some(owner) = owner {
        owner.shutdown();
    }
}
fn create_owner() -> io::Result<Client> {
    let wake = pipe_io::Signal::new()?;
    let signal = wake.clone();
    Client::new(
        Endpoint {
            connection: None,
            seen: false,
            wake,
        },
        std::sync::Arc::new(move || signal.notify()),
        || {
            oink_foundation::realtime::apply(
                oink_foundation::realtime::ThreadRole::Inject,
                oink_foundation::realtime::Tier::Realtime,
            )
            .map(|_| ())
        },
    )
}
impl Connection {
    fn arm(&mut self) -> io::Result<()> {
        // SAFETY: pinned Box response lives until reader cancellation/completion.
        unsafe { self.reader.read(&mut *self.response) }
    }
    fn start_write(&mut self) -> io::Result<()> {
        if self.sending.is_none()
            && let Some(request) = self.outgoing.pop()
        {
            *self.frame = request.encode();
            // SAFETY: the pinned frame is not reused before completion/cancel,
            // and the writer drops/drains before the frame or pipe.
            unsafe {
                self.writer.write(&*self.frame)?;
            }
            self.sending = Some((request.id, std::time::Instant::now()));
        }
        Ok(())
    }
}
impl Transport for Endpoint {
    fn connected(&self) -> bool {
        self.connection.is_some()
    }
    fn disconnect(&mut self) {
        self.connection = None;
    }
    fn send(&mut self, request: Request) -> io::Result<bool> {
        if self.connection.is_none() {
            match connect() {
                Ok(c) => {
                    self.seen = true;
                    self.connection = Some(c);
                }
                Err(e) if !self.seen && e.kind() == io::ErrorKind::NotFound => return Err(e),
                Err(e) => return Err(io::Error::other(e)),
            }
        }
        let c = self.connection.as_mut().expect("connected");
        // Only two request flights plus one heartbeat can exist. Keep message
        // writes ordered, but NEVER wait for an ordinary write on this owner:
        // an input ACK/read may be ready while that kernel write is pending.
        c.outgoing
            .push(request)
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "helper write reservation exceeded"))?;
        c.start_write()?;
        Ok(false)
    }
    fn written(&mut self) -> io::Result<Option<u32>> {
        let Some(c) = self.connection.as_mut() else {
            return Ok(None);
        };
        let Some((id, started)) = c.sending else {
            return Ok(None);
        };
        // SAFETY: zero-time test of the live owned write event.
        match unsafe { WaitForSingleObject(c.writer.event(), 0) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT if started.elapsed() < Duration::from_millis(150) => return Ok(None),
            WAIT_TIMEOUT => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "input pipe write timeout; not replayed",
                ));
            }
            _ => return Err(io::Error::last_os_error()),
        }
        if c.writer.finish()? != FRAME_SIZE as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "input pipe write incomplete",
            ));
        }
        c.sending = None;
        c.start_write()?;
        Ok(Some(id))
    }
    fn receive(&mut self) -> io::Result<Option<Reply>> {
        let Some(c) = self.connection.as_mut() else {
            return Ok(None);
        };
        // SAFETY: read-only, nonblocking test of the owned completion event.
        if unsafe { WaitForSingleObject(c.reader.event(), 0) } != WAIT_OBJECT_0 {
            return Ok(None);
        }
        if c.reader.finish()? != REPLY_SIZE as u32 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "input pipe reply size"));
        }
        let reply = Reply(*c.response);
        c.arm()?;
        Ok(Some(reply))
    }
    fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        let read = self.connection.as_ref().map(|c| c.reader.event());
        let write = self
            .connection
            .as_ref()
            .and_then(|c| c.sending.map(|(_, at)| (c.writer.event(), at)));
        let timeout = write.map_or(timeout, |(_, at)| {
            timeout.min(Duration::from_millis(150).saturating_sub(at.elapsed()))
        });
        let handles = [
            self.wake.handle(),
            read.unwrap_or(ptr::null_mut()),
            write.map_or(ptr::null_mut(), |(handle, _)| handle),
        ];
        let milliseconds = timeout.as_millis() + u128::from(!timeout.subsec_nanos().is_multiple_of(1_000_000));
        // SAFETY: distinct owned handles remain live; unused/null slots are outside count.
        let result = unsafe {
            WaitForMultipleObjects(
                1 + u32::from(read.is_some()) + u32::from(write.is_some()),
                handles.as_ptr(),
                0,
                milliseconds.min(u128::from(u32::MAX)) as u32,
            )
        };
        if result == WAIT_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
fn connect() -> io::Result<Connection> {
    let id = session()?;
    // Exact rights exclude FILE_CREATE_PIPE_INSTANCE; identification impersonation allows
    // token identity inspection, never impersonation capable of acting as the user.
    // SAFETY: local fixed pipe name, non-inherited handle, existing object only.
    let pipe = Handle::new(unsafe {
        CreateFileW(
            pipe_name(id).as_ptr(),
            FILE_READ_DATA
                | FILE_WRITE_DATA
                | FILE_READ_ATTRIBUTES
                | FILE_WRITE_ATTRIBUTES
                | READ_CONTROL
                | SYNCHRONIZE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        )
    })
    .map_err(|e| context("CreateFileW(input pipe)", e))?;
    let mode = PIPE_READMODE_MESSAGE | PIPE_WAIT;
    // SAFETY: valid connected pipe; mode pointer exists for call duration.
    check(unsafe { SetNamedPipeHandleState(pipe.0, &mode, ptr::null(), ptr::null()) })
        .map_err(|e| non_absence(context("SetNamedPipeHandleState", e)))?;
    let peer = security::peer(pipe.0, id, true).map_err(non_absence)?;
    let pipe = pipe_io::SharedPipe::new(pipe);
    let reader = pipe_io::OwnedOperation::new(pipe.clone())?;
    let mut connection = Connection {
        reader,
        response: Box::new([0; REPLY_SIZE]),
        writer: pipe_io::OwnedOperation::new(pipe.clone())?,
        frame: Box::new([0; FRAME_SIZE]),
        outgoing: crossbeam_queue::ArrayQueue::new(3),
        sending: None,
        _pipe: pipe,
        _peer: peer,
    };
    connection.arm()?;
    Ok(connection)
}
// NotFound is reserved for CreateFile finding no service pipe before the first
// connection. A missing object during authentication or a helper operation is
// an operation failure, and must never authorize ordinary-backend replay.
fn non_absence(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        io::Error::other(error)
    } else {
        error
    }
}
fn status(reply: Reply) -> io::Result<ServiceStatus> {
    let reply = reply.0;
    let n = |i| u32::from_le_bytes(reply[i..i + 4].try_into().expect("fixed reply"));
    HELPER_PRIORITY.store(
        n(crate::wire::REPLY_PRIORITY_OFFSET) as i32,
        std::sync::atomic::Ordering::Release,
    );
    if n(0) != 0 {
        return Err(context(
            crate::wire::error_stage_name(n(84)),
            io::Error::from_raw_os_error(n(0) as i32),
        ));
    }
    let end = reply[20..84].iter().position(|b| *b == 0).unwrap_or(64);
    Ok(ServiceStatus {
        pid: n(4),
        session: n(8),
        x: n(12) as i32,
        y: n(16) as i32,
        desktop: String::from_utf8_lossy(&reply[20..20 + end]).into_owned(),
        caps_lock: n(92) != 0,
        helper_priority: n(crate::wire::REPLY_PRIORITY_OFFSET) as i32,
    })
}

#[cfg(test)]
pub(super) fn private_pipe_client(pipe: Handle) -> io::Result<Client> {
    // Only private test pipes. Production construction always calls connect()
    // with the unchanged protected identity/ACL validation before any frame.
    let mode = PIPE_READMODE_MESSAGE | PIPE_WAIT;
    // SAFETY: owned private pipe, mode pointer lives through the call.
    check(unsafe { SetNamedPipeHandleState(pipe.0, &mode, ptr::null(), ptr::null()) })?;
    let pipe = pipe_io::SharedPipe::new(pipe);
    let mut connection = Connection {
        reader: pipe_io::OwnedOperation::new(pipe.clone())?,
        response: Box::new([0; REPLY_SIZE]),
        writer: pipe_io::OwnedOperation::new(pipe.clone())?,
        frame: Box::new([0; FRAME_SIZE]),
        outgoing: crossbeam_queue::ArrayQueue::new(3),
        sending: None,
        _pipe: pipe,
        // SAFETY: our own process, SYNCHRONIZE only, non-inherited test pin.
        _peer: Handle::new(unsafe { OpenProcess(SYNCHRONIZE, 0, GetCurrentProcessId()) })?,
    };
    connection.arm()?;
    let wake = pipe_io::Signal::new()?;
    let signal = wake.clone();
    Client::new(
        Endpoint {
            connection: Some(connection),
            seen: true,
            wake,
        },
        std::sync::Arc::new(move || signal.notify()),
        || {
            oink_foundation::realtime::apply(
                oink_foundation::realtime::ThreadRole::Inject,
                oink_foundation::realtime::Tier::Realtime,
            )
            .map(|_| ())
        },
    )
}
/// Returns NotFound only when no service has ever been connected and no request was sent.
/// Callers may use their ordinary unprivileged backend in that one case. Other failures
/// must never replay the request: the service might already have injected it.
pub fn request(cmd: Command) -> io::Result<ServiceStatus> {
    // Read-only API/observer queries must not occupy the native FIFO. Queries
    // from an input/injection owner remain ordered barriers with fresh ACKs.
    let ordinary = matches!(cmd, Command::Ping | Command::Position)
        && oink_foundation::realtime::read_back().is_ok_and(|p| ordinary_query(cmd, p.windows.mmcss_role));
    owner()?
        .request(cmd, ordinary, false)?
        .ok_or_else(|| io::Error::other("missing native reply"))
        .and_then(status)
        .map_err(non_absence)
}
fn ordinary_query(cmd: Command, role: Option<oink_foundation::realtime::ThreadRole>) -> bool {
    use oink_foundation::realtime::ThreadRole;
    matches!(cmd, Command::Ping | Command::Position) && !matches!(role, Some(ThreadRole::Input | ThreadRole::Inject))
}
/// Ordinary queries are correlated independently; they cannot occupy input's ACK lane.
pub fn query(cmd: Command) -> io::Result<ServiceStatus> {
    owner()?
        .request(cmd, true, false)?
        .ok_or_else(|| io::Error::other("missing query reply"))
        .and_then(status)
}
pub fn probe() -> io::Result<ServiceStatus> {
    query(Command::Ping)
}
pub fn release() -> io::Result<()> {
    request(Command::Release).map(|_| ())
}
/// Never initializes or reconnects the client. Release remains an input barrier.
pub fn release_if_connected() {
    let owner = OWNER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(owner) = owner {
        let _ = owner.request(Command::Release, false, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_observers_are_isolated_but_input_queries_and_cleanup_are_barriers() {
        use oink_foundation::realtime::ThreadRole;
        for command in [Command::Ping, Command::Position] {
            assert!(ordinary_query(command, None));
            assert!(ordinary_query(command, Some(ThreadRole::Network)));
            assert!(!ordinary_query(command, Some(ThreadRole::Input)));
            assert!(!ordinary_query(command, Some(ThreadRole::Inject)));
        }
        for role in [None, Some(ThreadRole::Input), Some(ThreadRole::Inject)] {
            for command in [
                Command::Release,
                Command::Move { x: 1, y: 2 },
                Command::Button { button: 0, down: false },
            ] {
                assert!(!ordinary_query(command, role));
            }
        }
    }

    #[test]
    fn helper_missing_objects_never_authorize_local_replay() {
        for code in [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND] {
            let helper_error = context("OpenInputDesktop", io::Error::from_raw_os_error(code as i32));
            assert_eq!(helper_error.kind(), io::ErrorKind::NotFound);
            let message = helper_error.to_string();
            let result = non_absence(helper_error);
            assert_eq!(result.kind(), io::ErrorKind::Other);
            assert_eq!(result.to_string(), message);
        }
    }

    #[test]
    fn missing_objects_after_pipe_open_are_not_an_absent_service() {
        for stage in ["SetNamedPipeHandleState", "protected peer image/ACL"] {
            for code in [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND] {
                let error = context(stage, io::Error::from_raw_os_error(code as i32));
                let message = error.to_string();
                let result = non_absence(error);
                assert_eq!(result.kind(), io::ErrorKind::Other);
                assert_eq!(result.to_string(), message);
            }
        }
    }

    #[test]
    fn other_operation_errors_preserve_their_kind_and_context() {
        let error = context("SendInput", io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
        let message = error.to_string();
        let result = non_absence(error);
        assert_eq!(result.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(result.to_string(), message);
    }
}
