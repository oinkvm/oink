//! Windows capture-only child. Anonymous inherited pipes carry one fixed numeric
//! fatal record. Only the per-user agent starts it; it has no HTTPS entry point.
use oink_telemetry::{capture::Manifest, *};
use std::{
    io::{Read, Write},
    os::windows::{io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Storage::FileSystem::WriteFile,
    System::{Diagnostics::Debug::*, Pipes::PeekNamedPipe, Threading::*},
};
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u16,
    pid: u32,
    generation: u64,
    component: Component,
    root: PathBuf,
    build: BuildContext,
    manifest: Manifest,
}
#[derive(Debug)]
struct Signal {
    input: ChildStdin,
    output: ChildStdout,
}
static SIGNAL: Mutex<Option<Signal>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);
#[derive(Debug)]
pub struct Collector {
    child: Child,
}
impl Collector {
    pub fn start(
        executable: &Path,
        root: &Path,
        consent: ReportConsent,
        build: BuildContext,
        manifest: &Manifest,
        component: Component,
    ) -> Result<Self> {
        if !consent.granted()
            || !matches!(component, Component::Agent | Component::UiRust)
            || !allowed_process(std::process::id())
        {
            return Err(Error::Disabled);
        }
        let config = Config {
            schema: 1,
            pid: std::process::id(),
            generation: consent.generation,
            component,
            root: root.to_path_buf(),
            build,
            manifest: manifest.clone(),
        };
        let mut bytes = serde_json::to_vec(&config).map_err(|_| Error::Invalid)?;
        if bytes.len() > 8192 {
            return Err(Error::Limit);
        }
        bytes.push(b'\n');
        let mut signal = SIGNAL.lock().map_err(|_| Error::Io)?;
        if signal.is_some() {
            return Err(Error::Busy);
        }
        let mut collector = Self {
            child: collector_command(executable).spawn().map_err(|_| Error::Io)?,
        };
        let mut input = collector.child.stdin.take().ok_or(Error::Io)?;
        let output = collector.child.stdout.take().ok_or(Error::Io)?;
        input.write_all(&bytes).map_err(|_| Error::Io)?;
        *signal = Some(Signal { input, output });
        GENERATION.store(consent.generation, Ordering::Release);
        Ok(collector)
    }
}
fn collector_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // This pipe-only child is armed on consent, before any crash.
        // Redirected stdio alone still allocates a visible console when
        // launched by the GUI or detached backend on Windows 11.
        .creation_flags(CREATE_NO_WINDOW);
    command
}

impl Drop for Collector {
    fn drop(&mut self) {
        GENERATION.store(0, Ordering::Release);
        let _ = self.child.kill();
        // Reap our capture-only child on opt-out and orderly shutdown.
        let _ = self.child.wait();
        if let Ok(mut signal) = SIGNAL.try_lock() {
            signal.take();
        }
    }
}
/// Called only by the last-chance exception filter, not performance callbacks.
/// One tiny write to an empty preopened pipe; the fatal process is held at most
/// two seconds so the unprivileged collector can read its exception context.
/// This is never used by a panic hook, graceful shutdown, or an opt-out request.
pub(crate) fn capture(info: *const EXCEPTION_POINTERS) {
    let generation = GENERATION.load(Ordering::Acquire);
    if generation == 0 {
        return;
    }
    let Ok(signal) = SIGNAL.try_lock() else { return };
    let Some(signal) = signal.as_ref() else { return };
    let mut request = [0u8; 32];
    request[..8].copy_from_slice(&generation.to_le_bytes());
    // SAFETY: current thread ID has no ownership or pointer preconditions.
    request[8..12].copy_from_slice(&unsafe { GetCurrentThreadId() }.to_le_bytes());
    request[16..24].copy_from_slice(&(info as usize as u64).to_le_bytes());
    request[24..32].copy_from_slice(&now().to_le_bytes());
    let mut written = 0;
    // SAFETY: pipe handles remain owned under the signal guard, fixed valid buffer.
    if unsafe {
        WriteFile(
            signal.input.as_raw_handle(),
            request.as_ptr(),
            request.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    } == 0
        || written != 32
    {
        return;
    }
    for _ in 0..200 {
        if GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        let mut available = 0;
        // SAFETY: read-only availability query on our preopened anonymous pipe.
        if unsafe {
            PeekNamedPipe(
                signal.output.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
            || available > 0
        {
            return;
        }
        // SAFETY: bounded last-chance wait; never runs on a surviving input path.
        unsafe { Sleep(10) };
    }
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this guard owns a non-null handle returned by Windows.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}
fn token(process: HANDLE) -> Option<(Handle, Vec<usize>)> {
    let mut handle = std::ptr::null_mut();
    // SAFETY: valid process handle; output handle owned by the guard below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut handle) } == 0 {
        return None;
    }
    let guard = Handle(handle);
    let mut bytes = vec![0usize; 64];
    let mut needed = 0;
    // SAFETY: aligned fixed buffer and correct byte size for TOKEN_USER.
    if unsafe {
        GetTokenInformation(
            handle,
            TokenUser,
            bytes.as_mut_ptr().cast(),
            (bytes.len() * std::mem::size_of::<usize>()) as u32,
            &mut needed,
        )
    } == 0
    {
        return None;
    }
    Some((guard, bytes))
}
pub fn allowed_process(pid: u32) -> bool {
    // SAFETY: read-only process query; failure is off, never privilege escalation.
    let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if process.0.is_null() {
        return false;
    }
    let Some((_guard, other)) = token(process.0) else {
        return false;
    };
    // SAFETY: the successful token query initialized TOKEN_USER in an aligned buffer.
    let sid = unsafe { (*other.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    for known in [WinLocalSystemSid, WinLocalServiceSid, WinNetworkServiceSid] {
        // SAFETY: token query owns a valid SID for this buffer lifetime.
        if unsafe { IsWellKnownSid(sid, known) } != 0 {
            return false;
        }
    }
    // SAFETY: current-process pseudo handle requires no close.
    let Some((_current, own)) = token(unsafe { GetCurrentProcess() }) else {
        return false;
    };
    // SAFETY: both SID pointers remain owned by live token-query buffers.
    unsafe { EqualSid(sid, (*own.as_ptr().cast::<TOKEN_USER>()).User.Sid) != 0 }
}
/// Binary entry point. Errors have fixed exit status and no payload/path logging.
pub fn run() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut config = Vec::new();
    for _ in 0..8193 {
        let mut b = [0];
        input.read_exact(&mut b).map_err(|_| Error::Io)?;
        if b[0] == b'\n' {
            break;
        }
        config.push(b[0]);
    }
    if config.len() > 8192 {
        return Err(Error::Limit);
    }
    let config: Config = serde_json::from_slice(&config).map_err(|_| Error::Invalid)?;
    if config.schema != 1
        || config.generation == 0
        || !matches!(config.component, Component::Agent | Component::UiRust)
        || config.build.app_version != config.manifest.version
        || !allowed_process(config.pid)
    {
        return Err(Error::Disabled);
    }
    // SAFETY: holds the exact process object before waiting; PID reuse cannot
    // redirect capture. Windows validates requested access and the PID.
    let process = Handle(unsafe {
        OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_DUP_HANDLE,
            0,
            config.pid,
        )
    });
    if process.0.is_null() {
        return Err(Error::Io);
    }
    let mut request = [0; 32];
    input.read_exact(&mut request).map_err(|_| Error::Io)?;
    let generation = u64::from_le_bytes(request[..8].try_into().unwrap());
    if generation != config.generation {
        return Err(Error::Stale);
    }
    let thread = u32::from_le_bytes(request[8..12].try_into().unwrap());
    let pointer = u64::from_le_bytes(request[16..24].try_into().unwrap());
    let at = u64::from_le_bytes(request[24..32].try_into().unwrap());
    if pointer == 0 || at > now() || now().saturating_sub(at) > 10 {
        return Err(Error::Invalid);
    }
    let lease = crate::staging::Lease {
        generation,
        collected_at: at,
        build: config.build,
        component: config.component,
    };
    restrict_directory(&config.root)?;
    let mut writer = crate::staging::Writer::reserve(&config.root, lease)?;
    let exception = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: thread,
        ExceptionPointers: pointer as usize as *mut _,
        ClientPointers: 1,
    };
    let callback = MINIDUMP_CALLBACK_INFORMATION {
        CallbackRoutine: Some(write_callback),
        CallbackParam: (&mut writer as *mut crate::staging::Writer).cast(),
    };
    // SAFETY: exact held process, exception pointer belongs to it (ClientPointers),
    // synchronous callback and writer live throughout. All writes use capped IO.
    let ok = unsafe {
        MiniDumpWriteDump(
            process.0,
            config.pid,
            std::ptr::null_mut(),
            MiniDumpNormal,
            &exception,
            std::ptr::null(),
            &callback,
        )
    };
    if ok != 0 {
        let _ = writer.project(&config.manifest);
    }
    let _ = std::io::stdout().write_all(&[1]);
    Ok(())
}
unsafe extern "system" fn write_callback(
    context: *mut std::ffi::c_void,
    input: *const MINIDUMP_CALLBACK_INPUT,
    output: *mut MINIDUMP_CALLBACK_OUTPUT,
) -> i32 {
    if context.is_null() || input.is_null() || output.is_null() {
        return 0;
    }
    // SAFETY: DbgHelp supplies valid callback structures for the synchronous call.
    let kind = unsafe { (*input).CallbackType } as i32;
    match kind {
        kind if kind == IoStartCallback => {
            // SAFETY: this callback kind initializes the Status union member.
            unsafe { (*output).Anonymous.Status = 1 };
            1
        } // S_FALSE selects custom IO.
        kind if kind == IoWriteAllCallback => {
            // SAFETY: this union arm is defined by IoWriteAllCallback.
            let io = unsafe { (*input).Anonymous.Io };
            if io.Buffer.is_null() || io.BufferBytes as u64 > crate::dump::MAX_DUMP {
                return 0;
            }
            // SAFETY: DbgHelp's valid buffer exists for this callback duration.
            let bytes = unsafe { std::slice::from_raw_parts(io.Buffer.cast::<u8>(), io.BufferBytes as usize) };
            // SAFETY: the callback exclusively borrows the synchronous writer.
            let ok = unsafe { &mut *context.cast::<crate::staging::Writer>() }
                .write_at(io.Offset, bytes)
                .is_ok();
            // SAFETY: IoWriteAllCallback returns its HRESULT in Status.
            unsafe { (*output).Anonymous.Status = if ok { 0 } else { 0x8000_4004u32 as i32 } };
            i32::from(ok)
        }
        kind if kind == IoFinishCallback => {
            // SAFETY: IoFinishCallback returns its HRESULT in Status.
            unsafe { (*output).Anonymous.Status = 0 };
            1
        }
        _ => 1,
    }
}

/// A protected inheritable DACL for the actual process user. Raw staging must
/// remain private even when a downstream profile chooses a shared parent folder.
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    std::fs::create_dir_all(path).map_err(|_| Error::Io)?;
    // SAFETY: current-process pseudo handle is valid and not owned by this call.
    let (_token, buffer) = token(unsafe { GetCurrentProcess() }).ok_or(Error::Io)?;
    // SAFETY: successful aligned TOKEN_USER query; buffer remains alive below.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut acl = [0usize; 32];
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: aligned bounded ACL/descriptor storage, valid token SID, terminated
    // path. Protected DACL replaces inherited broad access with this one user.
    let ok = unsafe {
        InitializeAcl(
            acl.as_mut_ptr().cast(),
            std::mem::size_of_val(&acl) as u32,
            ACL_REVISION,
        ) != 0
            && AddAccessAllowedAceEx(
                acl.as_mut_ptr().cast(),
                ACL_REVISION,
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                0x1000_0000,
                sid,
            ) != 0
            && InitializeSecurityDescriptor((&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(), 1) != 0
            && SetSecurityDescriptorDacl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                acl.as_ptr().cast(),
                0,
            ) != 0
            && SetFileSecurityW(
                name.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            ) != 0
    };
    if ok { Ok(()) } else { Err(Error::Io) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pipe_child_never_allocates_a_console() {
        const KEY: &str = "OINK_COLLECTOR_CONSOLE_TEST";
        if std::env::var_os(KEY).is_some() {
            // SAFETY: read-only query of this isolated child's own console.
            assert!(unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null());
            return;
        }
        let result = super::collector_command(&std::env::current_exe().unwrap())
            .env(KEY, "1")
            .args([
                "--exact",
                "collector::tests::pipe_child_never_allocates_a_console",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
    }
}
