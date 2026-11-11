//! Windows pre-login backend: the protected machine store, the SYSTEM service's
//! supervision of `oink.exe --prelogin`, that process's entry checks, and the
//! user-side client of the launch pipe.
//!
//! Storage (design: docs/design/winsvc.md "登录前后台"):
//! - Machine marker `HKLM\SOFTWARE\Oink\Prelogin` (`Enabled`, `Owner` SID). HKLM
//!   is administrator-only writable; the MSI (elevated) and this SYSTEM service
//!   are its only writers.
//! - `%ProgramData%\Oink\Prelogin\` with a protected SYSTEM + Administrators DACL:
//!   `snapshot\` (the owner's exported files plus `owner.sid`), `work\` (the
//!   pre-login backend's data directory, recreated from the snapshot on every
//!   start and never copied back) and `Logs\`. Every level from `Oink` down is
//!   opened without following reparse points and must have a privileged owner
//!   and no unprivileged write ACE. SYSTEM never reads or writes the user's own
//!   `%LOCALAPPDATA%\Oink`: the user can write there, so a SYSTEM writer would be
//!   a junction escalation, and its private files grant only the user SID.
use super::*;
use crate::backend_launch::BackendLaunch;
use crate::prelogin_policy::{self, Machine, OwnerView, SnapshotFile, Store};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::{Diagnostics::ToolHelp::*, Registry::*, RemoteDesktop::*},
    UI::Shell::*,
};

const KEY: &str = r"SOFTWARE\Oink\Prelogin";
/// SYSTEM and Administrators only; children inherit.
const DIRECTORY_SDDL: &str = "O:SYG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// Every user may read the marker (the UI shows it); only SYSTEM/Administrators write.
const KEY_SDDL: &str = "O:SYG:SYD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KR;;;BU)";
const OWNER_FILE: &str = "owner.sid";
const MAX_OWNER: u64 = 256;

fn stop_event_name(id: u32) -> Vec<u16> {
    wide(&format!(r"Global\Oink.Prelogin.Stop.{id}"))
}

fn program_data() -> io::Result<PathBuf> {
    let mut raw = ptr::null_mut();
    // SAFETY: static known-folder id; the COM allocation is freed after copying.
    unsafe {
        if SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, ptr::null_mut(), &mut raw) < 0 {
            return Err(context("SHGetKnownFolderPath(ProgramData)", denied()));
        }
        let mut n = 0;
        while *raw.add(n) != 0 {
            n += 1;
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(raw, n)));
        windows_sys::Win32::System::Com::CoTaskMemFree(raw.cast());
        Ok(path)
    }
}
fn product_dir() -> io::Result<PathBuf> {
    Ok(program_data()?.join("Oink"))
}
/// `%ProgramData%\Oink\Prelogin`.
pub fn root() -> io::Result<PathBuf> {
    Ok(product_dir()?.join("Prelogin"))
}
fn snapshot_dir() -> io::Result<PathBuf> {
    Ok(root()?.join("snapshot"))
}

/// Storage roots of the pre-login backend: data, cache and log directories.
#[derive(Clone, Debug)]
pub struct Roots {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
}
pub fn roots() -> io::Result<Roots> {
    let root = root()?;
    Ok(Roots {
        data: root.join("work"),
        cache: root.join("work").join("Cache"),
        logs: root.join("Logs"),
    })
}

/// Open (or create) one protected directory level. Creation applies the
/// protected DACL atomically; an existing level must already be protected.
fn protected_dir(path: &Path, create: bool) -> io::Result<File> {
    protected_dir_with(path, create.then_some(DIRECTORY_SDDL))
}

/// [`protected_dir`] with the creation DACL `create` (`None` opens only). The
/// opened level must have a privileged owner and no unprivileged write ACE.
pub(super) fn protected_dir_with(path: &Path, create: Option<&str>) -> io::Result<File> {
    if let Some(sddl) = create {
        let sd = security::descriptor(sddl)?;
        let attrs = sd.attrs();
        // SAFETY: fixed child path below ProgramData and a live descriptor.
        let created = unsafe { CreateDirectoryW(wide(path.to_str().ok_or_else(denied)?).as_ptr(), &attrs) };
        // SAFETY: immediately after the call.
        if created == 0 && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            return Err(context(
                "CreateDirectoryW(ProgramData store)",
                io::Error::last_os_error(),
            ));
        }
    }
    super::update::protected::open_one(path, true).map_err(|e| context("protected ProgramData directory", e))
}

/// Pin `%ProgramData%\Oink` (created with the protected SYSTEM + Administrators
/// DACL when `create`). ProgramData itself lets users create folders, so it is
/// only checked for reparse points; every level Oink owns must be protected.
pub(super) fn pin_product(create: bool) -> io::Result<(PathBuf, File)> {
    let data = program_data()?;
    // SAFETY: read-only attribute query of a fixed path.
    let attributes = unsafe { GetFileAttributesW(wide(data.to_str().ok_or_else(denied)?).as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(context("ProgramData reparse/attribute check", denied()));
    }
    let path = data.join("Oink");
    let pinned = protected_dir(&path, create)?;
    Ok((path, pinned))
}

/// Pin `Oink`, `Prelogin` and the requested child directories; any unprotected
/// level refuses the whole store.
fn pin(children: &[&str], create: bool) -> io::Result<Vec<File>> {
    let (mut path, product) = pin_product(create)?;
    let mut pinned = vec![product];
    path.push("Prelogin");
    pinned.push(protected_dir(&path, create)?);
    for child in children {
        pinned.push(protected_dir(&path.join(child), create)?);
    }
    Ok(pinned)
}

fn read_bounded(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let file = super::update::protected::open_one(path, false)?;
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "prelogin file too large"));
    }
    Ok(bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // The parent is pinned and protected. create_new refuses an existing name,
    // and the parent's inheritable DACL applies before any byte is written.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Remove a flat directory this module wrote. Reparse points and nested
/// directories are never followed.
fn remove_flat(path: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() || kind.is_dir() {
            return Err(context("unexpected entry in prelogin store", denied()));
        }
        std::fs::remove_file(entry.path())?;
    }
    std::fs::remove_dir(path)
}

fn registry(key: &mut HKEY, access: u32, create: bool) -> u32 {
    let name = wide(KEY);
    if create {
        let Ok(sd) = security::descriptor(KEY_SDDL) else {
            return ERROR_ACCESS_DENIED;
        };
        let attrs = sd.attrs();
        // SAFETY: fixed HKLM subkey, explicit protected descriptor for a new key,
        // live output handle; the 64-bit view is used from every process.
        unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                name.as_ptr(),
                0,
                ptr::null(),
                0,
                access | KEY_WOW64_64KEY,
                &attrs,
                key,
                ptr::null_mut(),
            )
        }
    } else {
        // SAFETY: fixed HKLM subkey, live output handle.
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, name.as_ptr(), 0, access | KEY_WOW64_64KEY, key) }
    }
}

/// Read the machine marker. Any user may read it.
pub(crate) fn machine() -> io::Result<Machine> {
    let mut key = ptr::null_mut();
    match registry(&mut key, KEY_QUERY_VALUE, false) {
        0 => {}
        ERROR_FILE_NOT_FOUND => return Ok(Machine::default()),
        code => return Err(io::Error::from_raw_os_error(code as i32)),
    }
    let result = (|| {
        let mut enabled = 0u32;
        let mut size = 4u32;
        // SAFETY: open key, DWORD output buffer of the stated size.
        let code = unsafe {
            RegGetValueW(
                key,
                ptr::null(),
                wide("Enabled").as_ptr(),
                RRF_RT_REG_DWORD,
                ptr::null_mut(),
                ptr::addr_of_mut!(enabled).cast(),
                &mut size,
            )
        };
        if code != 0 && code != ERROR_FILE_NOT_FOUND {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        let mut owner = [0u16; 256];
        let mut size = size_of_val(&owner) as u32;
        // SAFETY: open key, bounded UTF-16 output buffer of the stated byte size.
        let code = unsafe {
            RegGetValueW(
                key,
                ptr::null(),
                wide("Owner").as_ptr(),
                RRF_RT_REG_SZ,
                ptr::null_mut(),
                owner.as_mut_ptr().cast(),
                &mut size,
            )
        };
        let owner = match code {
            0 => {
                let text = String::from_utf16_lossy(&owner[..(size as usize / 2).saturating_sub(1)]);
                (!text.is_empty()).then_some(text)
            }
            ERROR_FILE_NOT_FOUND => None,
            code => return Err(io::Error::from_raw_os_error(code as i32)),
        };
        Ok(Machine {
            enabled: enabled == 1,
            owner,
        })
    })();
    // SAFETY: owned key handle.
    unsafe { RegCloseKey(key) };
    result
}

fn set_machine(machine: &Machine) -> io::Result<()> {
    let mut key = ptr::null_mut();
    let code = registry(&mut key, KEY_SET_VALUE, true);
    if code != 0 {
        return Err(context(
            "RegCreateKeyExW(prelogin)",
            io::Error::from_raw_os_error(code as i32),
        ));
    }
    let result = (|| {
        let enabled = u32::from(machine.enabled);
        // SAFETY: open key, DWORD value.
        let code = unsafe {
            RegSetValueExW(
                key,
                wide("Enabled").as_ptr(),
                0,
                REG_DWORD,
                ptr::addr_of!(enabled).cast(),
                4,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        let code = match &machine.owner {
            Some(owner) => {
                let value = wide(owner);
                // SAFETY: open key, NUL-terminated UTF-16 string and its byte length.
                unsafe {
                    RegSetValueExW(
                        key,
                        wide("Owner").as_ptr(),
                        0,
                        REG_SZ,
                        value.as_ptr().cast(),
                        (value.len() * 2) as u32,
                    )
                }
            }
            // SAFETY: open key and fixed value name.
            None => match unsafe { RegDeleteValueW(key, wide("Owner").as_ptr()) } {
                ERROR_FILE_NOT_FOUND => 0,
                code => code,
            },
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(())
    })();
    // SAFETY: owned key handle.
    unsafe { RegCloseKey(key) };
    result
}

/// The SYSTEM-side store. Only the service and the pre-login entry (both SYSTEM) use it.
pub(crate) struct Protected;

fn store_access() -> &'static prelogin_policy::StoreAccess {
    // A final startup check can overlap an already-authenticated request at
    // logout. Serialize that check and the launch worker's complete request so
    // readiness's deny-delete pins
    // cannot collide with snapshot rename/delete. Do not relax protected file
    // sharing flags: pinned ancestors still prevent path replacement attacks.
    static ACCESS: OnceLock<prelogin_policy::StoreAccess> = OnceLock::new();
    ACCESS.get_or_init(prelogin_policy::StoreAccess::default)
}

impl Store for Protected {
    fn machine(&self) -> io::Result<Machine> {
        machine()
    }
    fn set_machine(&mut self, machine: &Machine) -> io::Result<()> {
        set_machine(machine)
    }
    fn snapshot_owner(&self) -> io::Result<Option<String>> {
        let directory = snapshot_dir()?;
        if !directory.exists() {
            return Ok(None);
        }
        let _pinned = pin(&["snapshot"], false)?;
        for file in SnapshotFile::ALL.into_iter().filter(|f| f.required()) {
            if !directory.join(file.name()).exists() {
                return Ok(None);
            }
        }
        match read_bounded(&directory.join(OWNER_FILE), MAX_OWNER) {
            Ok(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|_| denied())?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn write_snapshot(&mut self, owner: &str, files: &[(SnapshotFile, Vec<u8>)]) -> io::Result<()> {
        prelogin_policy::validate_snapshot(files)?;
        let root = root()?;
        let _pinned = pin(&[], true)?;
        let next = root.join("snapshot.new");
        remove_flat(&next)?;
        let _next = protected_dir(&next, true)?;
        for (file, bytes) in files {
            write_new(&next.join(file.name()), bytes)?;
        }
        // The owner record is written last: an interrupted write is never complete.
        write_new(&next.join(OWNER_FILE), owner.as_bytes())?;
        drop(_next);
        let current = root.join("snapshot");
        let old = root.join("snapshot.old");
        remove_flat(&old)?;
        if current.exists() {
            std::fs::rename(&current, &old)?;
        }
        std::fs::rename(&next, &current)?;
        remove_flat(&old)
    }
    fn delete_snapshot(&mut self) -> io::Result<()> {
        let root = root()?;
        if !root.exists() {
            return Ok(());
        }
        let _pinned = pin(&[], false)?;
        for name in ["snapshot.new", "snapshot", "snapshot.old"] {
            remove_flat(&root.join(name))?;
        }
        Ok(())
    }
}

/// Service side: one authenticated request from the console session's user.
/// `process` is the kernel-authenticated, PID-pinned client process.
pub(super) fn handle(request: &BackendLaunch, id: u32, process: &Handle) -> io::Result<BackendLaunch> {
    let sid = security::sid_string(&security::user_sid(id)?)?;
    let elevated = super::update::elevated(process);
    let reply = store_access().handle(
        &mut Protected,
        request,
        &prelogin_policy::Caller { sid: &sid, elevated },
    );
    if reply.is_ok() && !matches!(request, BackendLaunch::PreloginStatus) {
        super::broker::changed();
    }
    if let Ok(BackendLaunch::PreloginState {
        enabled,
        owner,
        snapshot,
    }) = &reply
    {
        let kind = match request {
            BackendLaunch::PreloginClaim => "claim",
            BackendLaunch::PreloginRelease => "release",
            BackendLaunch::PreloginSnapshot { .. } => "snapshot",
            _ => "status",
        };
        tracing::info!(
            kind,
            session = id,
            enabled,
            ?owner,
            snapshot,
            "pre-login store request applied"
        );
    }
    reply
}

/// A running `oink.exe --prelogin` created by this service.
pub(super) struct Child {
    id: u32,
    owner: String,
    process: Handle,
    stop: Handle,
}
// SAFETY: kernel process and event handles, used for waits and signals only.
unsafe impl Send for Child {}
impl prelogin_policy::Child for Child {
    fn session(&self) -> u32 {
        self.id
    }
    fn alive(&self) -> bool {
        // SAFETY: owned process handle, nonblocking query.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }
    fn stop(&mut self) -> io::Result<()> {
        // Handoff depends on the pinned network process exiting. Final input
        // release remains best effort; its ACK and exit code are diagnostic.
        // SAFETY: owned event/process handles; every wait and termination is bounded.
        unsafe {
            if WaitForSingleObject(self.process.0, 0) == WAIT_OBJECT_0 {
                return Ok(());
            }
            check(SetEvent(self.stop.0))?;
            match WaitForSingleObject(self.process.0, 2500) {
                WAIT_OBJECT_0 => {
                    let mut code = 1;
                    if GetExitCodeProcess(self.process.0, &mut code) == 0 {
                        let error = io::Error::last_os_error();
                        tracing::warn!(%error, "pre-login exit code unavailable");
                    } else if code != 0 {
                        tracing::warn!(
                            code = format_args!("{code:#010x}"),
                            "pre-login backend exited during handoff"
                        );
                    }
                    Ok(())
                }
                WAIT_TIMEOUT => {
                    check(TerminateProcess(self.process.0, 1))?;
                    let exited = WaitForSingleObject(self.process.0, 500) == WAIT_OBJECT_0;
                    tracing::warn!(process_exit_confirmed = exited, "pre-login stop timed out");
                    if exited {
                        Ok(())
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "prelogin_process_still_running",
                        ))
                    }
                }
                _ => Err(io::Error::last_os_error()),
            }
        }
    }
}

fn supervisor() -> &'static Mutex<prelogin_policy::Supervisor<Child>> {
    static SUPERVISOR: OnceLock<Mutex<prelogin_policy::Supervisor<Child>>> = OnceLock::new();
    SUPERVISOR.get_or_init(Mutex::default)
}

/// Protected user backends and older SYSTEM pre-login processes. A service
/// restart must not create the same identity while its previous child is still
/// draining after parent death. Exclude only our retained, live child handle.
fn other_backend_alive(owned_pid: Option<u32>, include_user: bool) -> bool {
    backend_processes(owned_pid, include_user).map_or(true, |processes| !processes.is_empty())
}

pub(super) fn backend_processes(owned_pid: Option<u32>, include_user: bool) -> io::Result<Vec<Handle>> {
    let expected = security::install_dir()?.join("oink.exe");
    let mut processes = Vec::new();
    // SAFETY: process snapshot handle owned below; entries copied by value.
    let snapshot = Handle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: live snapshot and correctly sized entry.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        let name_len = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
        if name.eq_ignore_ascii_case("oink.exe") && Some(entry.th32ProcessID) != owned_pid {
            let opened =
                // SAFETY: query-only handle, owned by Handle.
                Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, entry.th32ProcessID) });
            if let Ok(process) = opened
                // SAFETY: owned synchronize handle; exited pinned processes are not alive.
                && unsafe { WaitForSingleObject(process.0, 0) } == WAIT_TIMEOUT
                && security::image(process.0)
                    .is_ok_and(|path| path.as_os_str().eq_ignore_ascii_case(expected.as_os_str()))
                && token(process.0).is_ok_and(|t| {
                    let system = security::sid_bytes(t.0).is_ok_and(|sid| {
                        // SAFETY: SID bytes copied from a validated process token.
                        unsafe { IsWellKnownSid(sid.as_ptr().cast_mut().cast(), WinLocalSystemSid) != 0 }
                    });
                    system || (include_user && super::backend::token_uiaccess(t.0).unwrap_or(0) != 0)
                })
            {
                processes.push(process);
            }
        }
        // SAFETY: same live snapshot and entry.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    Ok(processes)
}

/// Only read the loaded user's registry; never load a hive or touch a user path
/// as SYSTEM. A missing/disabled Run entry means no replacement is expected.
fn owner_autostart(id: u32, owner: &str) -> bool {
    let Ok(sid) = security::user_sid(id).and_then(|s| security::sid_string(&s)) else {
        return false;
    };
    if !owner.eq_ignore_ascii_case(&sid) {
        return false;
    }
    let Ok(exe) = security::install_dir().map(|d| d.join("oink.exe")) else {
        return false;
    };
    // WTS can expose the token before User Profile Service has mounted HKU.
    // An unmounted hive is not an explicit opt-out: the machine marker remains
    // our bounded expectation until the same 60 s handoff deadline. Once the
    // hive exists, a missing/disabled Run entry immediately withdraws it.
    let mut hive = ptr::null_mut();
    // SAFETY: read-only open; no hive loading, filesystem or registry mutation.
    let opened = unsafe { RegOpenKeyExW(HKEY_USERS, wide(&sid).as_ptr(), 0, KEY_QUERY_VALUE, &mut hive) };
    if opened == ERROR_FILE_NOT_FOUND {
        return true;
    }
    if opened != 0 {
        return false;
    }
    // SAFETY: successful open above returned one owned registry handle.
    unsafe {
        RegCloseKey(hive);
    }
    let run = wide(&format!(r"{sid}\Software\Microsoft\Windows\CurrentVersion\Run"));
    let mut command = [0u16; 2048];
    let mut size = size_of_val(&command) as u32;
    // SAFETY: read-only predefined hive; bounded UTF-16 output.
    let code = unsafe {
        RegGetValueW(
            HKEY_USERS,
            run.as_ptr(),
            wide("Oink").as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            command.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if code != 0 {
        return false;
    }
    let command = String::from_utf16_lossy(&command[..(size as usize / 2).saturating_sub(1)]);
    if !command
        .trim()
        .eq_ignore_ascii_case(&format!("\"{}\" --autostart", exe.display()))
    {
        return false;
    }
    let approved = wide(&format!(
        r"{sid}\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
    ));
    let mut data = [0u8; 64];
    let mut size = data.len() as u32;
    // StartupApproved's undocumented parity matches oink-autostart's read-only
    // Windows 10/11 policy. Never write it or override the user's OS choice.
    // SAFETY: bounded byte output, registry query only.
    let code = unsafe {
        RegGetValueW(
            HKEY_USERS,
            approved.as_ptr(),
            wide("Oink").as_ptr(),
            RRF_RT_REG_BINARY,
            ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    code == ERROR_FILE_NOT_FOUND || (code == 0 && (size == 0 || data[0] & 1 == 0))
}

fn observe(origin: Option<&Child>, owned_pid: Option<u32>) -> prelogin_policy::Observation {
    // SAFETY: read-only active console session query.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    let console = (id != 0 && id != u32::MAX).then_some(id);
    let user_session = origin.map(|child| child.id).or(console);
    let user_present = user_session.is_some_and(|id| security::user_sid(id).is_ok());
    prelogin_policy::Observation {
        console,
        user_present,
        owner_autostart: user_present && origin.is_some_and(|child| owner_autostart(child.id, &child.owner)),
        // A live child must still yield to an existing user backend. Without a
        // child, no process scan is needed while a user token prevents a start.
        user_backend_alive: (origin.is_some() || (console.is_some() && !user_present))
            && other_backend_alive(owned_pid, true),
        maintenance: super::update::pending(),
    }
}

/// Broker observation: start, keep or stop the pre-login backend.
pub(super) struct Watch {
    pub deadline: Option<Instant>,
    pub owner: Option<String>,
    pub running: bool,
}

pub(super) fn processes() -> io::Result<Vec<Handle>> {
    let supervisor = supervisor().lock().unwrap_or_else(|e| e.into_inner());
    supervisor
        .child()
        .map(|child| child.process.duplicate())
        .transpose()
        .map(|child| child.into_iter().collect())
}

pub(super) fn tick() -> Watch {
    let mut supervisor = supervisor().lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: retained process handle is the identity exclusion, not a PID file.
    let owned_pid = supervisor
        .child()
        .filter(|c| prelogin_policy::Child::alive(*c))
        .map(|c| unsafe { GetProcessId(c.process.0) });
    let origin = supervisor.child().filter(|c| prelogin_policy::Child::alive(*c));
    let observed = observe(origin, owned_pid);
    let result = supervisor.tick(
        &observed,
        Instant::now(),
        || match store_access().ready(&Protected) {
            Ok(owner) => Ok(owner),
            Err(error) => {
                static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(%error, "pre-login store unavailable");
                }
                Ok(None)
            }
        },
        spawn,
    );
    let retry = result.is_err().then(|| Instant::now() + Duration::from_millis(250));
    match result {
        Ok(prelogin_policy::Tick::Started(id)) => tracing::info!(session = id, "pre-login backend started"),
        Ok(prelogin_policy::Tick::Stopped) => tracing::info!(?observed, "pre-login backend stopped"),
        Ok(prelogin_policy::Tick::Exited) => {
            tracing::info!("pre-login backend exited");
            // Reaping returns before startup decisions. Continue once without
            // relying on a future poll; existing restart backoff still applies.
            super::broker::changed();
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "pre-login backend start failed"),
    }
    Watch {
        deadline: supervisor
            .deadline(&observed, Instant::now())
            .into_iter()
            .chain(retry)
            .min(),
        owner: observed
            .user_present
            .then(|| supervisor.child().map(|child| child.owner.clone()))
            .flatten(),
        running: supervisor.child().is_some(),
    }
}

/// Called before any user backend is created, and at service stop. Waits for
/// the pre-login backend to release keys and exit, so one DeviceId never has two
/// live processes.
pub(super) fn stop() -> io::Result<()> {
    if supervisor().lock().unwrap_or_else(|e| e.into_inner()).stop()? {
        tracing::info!("pre-login backend stop confirmed");
    }
    Ok(())
}

pub(super) fn handoff(
    client: &Handle,
    create: impl FnOnce() -> io::Result<BackendLaunch>,
) -> io::Result<BackendLaunch> {
    let mut supervisor = supervisor().lock().unwrap_or_else(|e| e.into_inner());
    let result = supervisor.handoff(|| {
        if other_backend_alive(None, false) {
            return Err(io::Error::other("an earlier SYSTEM backend has not exited"));
        }
        create()
    });
    match result {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let child = supervisor.child().ok_or_else(denied)?;
            let mut raw = ptr::null_mut();
            // SAFETY: duplicate the exact old generation while its supervisor is
            // still held. Only synchronize rights enter the authenticated UI.
            check(unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    child.process.0,
                    client.0,
                    &mut raw,
                    SYNCHRONIZE,
                    0,
                    0,
                )
            })?;
            Ok(BackendLaunch::PreloginStopping {
                process_handle: raw as usize as u64,
            })
        }
        result => result,
    }
}

/// The service's retained process handle is the authority, never a PID supplied
/// by the backend. Keep it pinned through all helper lifetimes.
pub(super) fn route_identity() -> io::Result<Option<crate::prelogin_route::Source>> {
    let supervisor = supervisor().lock().unwrap_or_else(|e| e.into_inner());
    let Some(child) = supervisor.child().filter(|c| prelogin_policy::Child::alive(*c)) else {
        return Ok(None);
    };
    // SAFETY: owned child handle and current service pseudo handle.
    unsafe {
        let mut raw = ptr::null_mut();
        check(windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW(
            wide(&child.owner).as_ptr(),
            &mut raw,
        ))?;
        let owner_sid = std::slice::from_raw_parts(raw.cast::<u8>(), GetLengthSid(raw) as usize).to_vec();
        LocalFree(raw);
        Ok(Some(crate::prelogin_route::Source {
            identity: crate::prelogin_route::Identity {
                pid: GetProcessId(child.process.0),
                session: child.id,
                created: created(child.process.0)?,
                parent: GetCurrentProcessId(),
                parent_created: created(GetCurrentProcess())?,
            },
            user_sid: None,
            logon: None,
            owner_sid,
        }))
    }
}

fn spawn(id: u32, owner: String) -> io::Result<Child> {
    let path = security::install_dir()?.join("oink.exe");
    security::protected_file(&path).map_err(|e| context("protected pre-login image", e))?;
    let primary = super::broker::session_system_token(id)?;
    // The process and its stop event are SYSTEM-only: there is no user to grant.
    let process_sd = security::system_security()?;
    let process_sa = process_sd.attrs();
    let event_sd = security::system_security()?;
    let event_sa = event_sd.attrs();
    // SAFETY: named SYSTEM-only manual-reset event, never inherited.
    let stop = Handle::new(unsafe { CreateEventW(&event_sa, 1, 0, stop_event_name(id).as_ptr()) })?;
    // SAFETY: immediately after CreateEvent; never trust a pre-existing object.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(context("pre-login stop event already exists", denied()));
    }
    let app = wide(path.to_str().ok_or_else(denied)?);
    let mut command = wide(&format!("\"{}\" --prelogin", path.display()));
    // LogonUI and the credential UI are on Winlogon before anyone signs in.
    // The process creates no visible window; its capture thread hooks this desktop.
    let mut desktop = wide(r"winsta0\Winlogon");
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: desktop.as_mut_ptr(),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    let directory = wide(security::install_dir()?.to_str().ok_or_else(denied)?);
    // SAFETY: protected image, fixed argument, SYSTEM token for the console
    // session, no inherited handles; the service's own environment.
    check(unsafe {
        CreateProcessAsUserW(
            primary.0,
            app.as_ptr(),
            command.as_mut_ptr(),
            &process_sa,
            ptr::null(),
            0,
            CREATE_NO_WINDOW | NORMAL_PRIORITY_CLASS,
            ptr::null(),
            directory.as_ptr(),
            &startup,
            &mut info,
        )
    })
    .map_err(|e| context("CreateProcessAsUserW(prelogin)", e))?;
    let process = Handle::new(info.hProcess)?;
    drop(Handle::new(info.hThread)?);
    Ok(Child {
        id,
        owner,
        process,
        stop,
    })
}

/// Entry guard of `oink.exe --prelogin`: SYSTEM, a non-zero active console
/// session, the protected image, and the running OinkInput service as parent.
#[derive(Debug)]
pub struct Entry {
    pub session: u32,
    parent: Handle,
    stop: Handle,
}
// SAFETY: kernel handles, used only for waits.
unsafe impl Send for Entry {}
// SAFETY: kernel handles, used only for waits.
unsafe impl Sync for Entry {}

pub(super) fn parent_pid(me: u32) -> io::Result<u32> {
    // SAFETY: process snapshot; the handle is owned below.
    let snapshot = Handle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: live snapshot and sized entry.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        if entry.th32ProcessID == me {
            return Ok(entry.th32ParentProcessID);
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    Err(context("own process entry", denied()))
}

pub(super) fn created(process: HANDLE) -> io::Result<u64> {
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: live process handle and writable FILETIMEs.
    check(unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) })?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

pub fn verify_entry() -> io::Result<Entry> {
    // SAFETY: pseudo handle of this process.
    let me = unsafe { GetCurrentProcess() };
    let path = security::image(me)?;
    if !path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("oink.exe"))
    {
        return Err(context("pre-login image name", denied()));
    }
    security::protected_file(&path).map_err(|e| context("protected pre-login image", e))?;
    let sid = security::sid_bytes(token(me)?.0)?;
    // SAFETY: SID bytes from this process token.
    if unsafe { IsWellKnownSid(sid.as_ptr().cast_mut().cast(), WinLocalSystemSid) } == 0 {
        return Err(context("pre-login token is not SYSTEM", denied()));
    }
    let id = session()?;
    // SAFETY: read-only console query.
    if id == 0 || unsafe { WTSGetActiveConsoleSessionId() } != id {
        return Err(context("pre-login session is not the active console", denied()));
    }
    // Kernel parent id + the SCM's running service PID + creation order: a
    // reused parent PID cannot be older than this child.
    // SAFETY: read-only own process ID.
    let parent_id = parent_pid(unsafe { GetCurrentProcessId() })?;
    if parent_id == 0 || parent_id != super::service_peer::installed_pid()? {
        return Err(context("pre-login parent is not OinkInput", denied()));
    }
    // SAFETY: query/synchronize handle pins the parent for the process lifetime.
    let parent = Handle::new(unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, parent_id) })?;
    if created(parent.0)? > created(me)? {
        return Err(context("pre-login parent creation order", denied()));
    }
    // SAFETY: the service created this SYSTEM-only event before the process.
    let stop = Handle::new(unsafe { OpenEventW(SYNCHRONIZE, 0, stop_event_name(id).as_ptr()) })
        .map_err(|e| context("OpenEventW(prelogin stop)", e))?;
    Ok(Entry {
        session: id,
        parent,
        stop,
    })
}

impl Entry {
    /// The network identity stays in its original session. Only the broker's
    /// policy or parent death stops it; helper routing follows the console.
    pub fn wait_stop(&self) {
        loop {
            let handles = [self.stop.0, self.parent.0];
            // SAFETY: both handles live for self.
            let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 250) };
            if result != WAIT_TIMEOUT {
                return;
            }
        }
    }
}

/// Recreate the working data directory from the owner's snapshot. Runs in the
/// pre-login backend (SYSTEM) before any storage is opened. Nothing is copied back.
pub fn prepare_work() -> io::Result<Roots> {
    let roots = roots()?;
    let machine = machine()?;
    let owner = Protected.snapshot_owner()?;
    if !machine.enabled
        || machine.owner.is_none()
        || !owner
            .as_deref()
            .zip(machine.owner.as_deref())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "pre-login is not enabled or the owner's snapshot is missing",
        ));
    }
    let _pinned = pin(&["snapshot"], false)?;
    let work = roots.data.clone();
    if work.exists() {
        // std's Windows remove_dir_all deletes links without following them.
        std::fs::remove_dir_all(&work)?;
    }
    let _work = protected_dir(&work, true)?;
    let _logs = protected_dir(&roots.logs, true)?;
    let source = snapshot_dir()?;
    for file in SnapshotFile::ALL {
        match read_bounded(&source.join(file.name()), file.max_bytes() as u64) {
            Ok(bytes) => write_new(&work.join(file.name()), &bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !file.required() => {}
            Err(error) => return Err(context(file.name(), error)),
        }
    }
    Ok(roots)
}

// ---- user side: the launch pipe client ----

/// Machine-level state as seen by the calling user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub enabled: bool,
    pub owner: OwnerView,
    pub snapshot: bool,
}
fn exchange(message: BackendLaunch) -> io::Result<State> {
    match super::backend::exchange(message)? {
        BackendLaunch::PreloginState {
            enabled,
            owner,
            snapshot,
        } => Ok(State {
            enabled,
            owner,
            snapshot,
        }),
        BackendLaunch::Failed { error } => Err(io::Error::new(io::ErrorKind::PermissionDenied, error)),
        _ => Err(denied()),
    }
}
pub fn status() -> io::Result<State> {
    exchange(BackendLaunch::PreloginStatus)
}
/// Requires an elevated caller; see [`elevate_claim`].
pub fn claim() -> io::Result<State> {
    exchange(BackendLaunch::PreloginClaim)
}
pub fn release() -> io::Result<State> {
    exchange(BackendLaunch::PreloginRelease)
}
pub fn push_snapshot(files: Vec<(SnapshotFile, Vec<u8>)>) -> io::Result<State> {
    exchange(BackendLaunch::PreloginSnapshot { files })
}

/// Outcome of asking for administrator approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Elevation {
    Approved,
    Declined,
    Failed,
}

/// Run the protected `oink-agent.exe prelogin-claim` through UAC (the same
/// shell elevation as local update approval). The elevated CLI sends the claim
/// over this session's launch pipe; the service checks its elevated token.
pub fn elevate_claim() -> Elevation {
    use windows_sys::Win32::{System::Com::*, UI::WindowsAndMessaging::SW_HIDE};
    let Ok(agent) = security::install_dir().map(|d| d.join("oink-agent.exe")) else {
        return Elevation::Failed;
    };
    if security::protected_file(&agent).is_err() {
        return Elevation::Failed;
    }
    let Some(agent) = agent.to_str().map(wide) else {
        return Elevation::Failed;
    };
    let arguments = wide("prelogin-claim");
    let verb = wide("runas");
    // SAFETY: NUL-terminated inputs and the output structure stay live; the
    // returned process handle is owned and closed; COM is balanced on success.
    unsafe {
        let com = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: verb.as_ptr(),
            lpFile: agent.as_ptr(),
            lpParameters: arguments.as_ptr(),
            nShow: SW_HIDE,
            ..Default::default()
        };
        let launched = ShellExecuteExW(&mut info);
        let error = GetLastError();
        if com >= 0 {
            CoUninitialize();
        }
        if launched == 0 || info.hProcess.is_null() {
            return if error == ERROR_CANCELLED {
                Elevation::Declined
            } else {
                Elevation::Failed
            };
        }
        let Ok(process) = Handle::new(info.hProcess) else {
            return Elevation::Failed;
        };
        if WaitForSingleObject(process.0, 120_000) != WAIT_OBJECT_0 {
            return Elevation::Failed;
        }
        let mut exit = 1;
        if GetExitCodeProcess(process.0, &mut exit) == 0 || exit != 0 {
            return Elevation::Failed;
        }
    }
    Elevation::Approved
}

#[cfg(test)]
mod exit_tests {
    use super::*;
    use crate::prelogin_policy::Child as _;
    use std::os::windows::io::IntoRawHandle;
    #[test]
    fn nonzero_exit_including_unconfirmed_release_is_diagnostic_not_a_handoff_barrier() {
        for exit in [0, 1, 7] {
            let process = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", &format!("exit {exit}")])
                .spawn()
                .unwrap();
            let process = Handle::new(process.into_raw_handle()).unwrap();
            // SAFETY: private test child and unnamed event; no installed process or desktop.
            let stop = Handle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) }).unwrap();
            // SAFETY: owned private child process handle, bounded wait.
            let exited = unsafe { WaitForSingleObject(process.0, 5000) };
            assert_eq!(exited, WAIT_OBJECT_0);
            let mut child = Child {
                id: 1,
                owner: String::new(),
                process,
                stop,
            };
            child.stop().unwrap();
            assert!(!child.alive());
        }
    }
}
