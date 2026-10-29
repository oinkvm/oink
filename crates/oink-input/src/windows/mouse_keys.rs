//! Ordinary user-session worker. Never call SPI or filesystem I/O on the input owner.
use crate::mouse_keys::{Action, Compensation, Marker, Reason, Settings, System};
use oink_foundation::snapshot::Snapshot;
use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0},
        System::{
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{CreateMutexW, GetCurrentProcessId, ReleaseMutex, WaitForSingleObject},
        },
        UI::{
            Accessibility::MOUSEKEYS,
            Input::{GetRawInputDeviceList, RAWINPUTDEVICELIST, RIM_TYPEMOUSE},
            WindowsAndMessaging::{
                GetSystemMetrics, SM_MOUSEPRESENT, SPI_GETMOUSEKEYS, SPI_SETMOUSEKEYS,
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
            },
        },
    },
    core::w,
};

static WORKER: OnceLock<Snapshot<Option<thread::Thread>>> = OnceLock::new();
static REASON: AtomicU32 = AtomicU32::new(0);
pub(super) fn edge(reason: Reason) {
    // Healthy injected frames need only a metric read, not a worker wakeup at
    // touchpad frame rate. The active 250ms check covers delayed summaries.
    // SAFETY: read-only session metric, callable on the input owner.
    if reason == Reason::DeviceInject && unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } != 0 {
        return;
    }
    REASON.store(reason as u32, Ordering::Relaxed);
    if let Some(slot) = WORKER.get()
        && let Some(worker) = slot.load().as_ref()
    {
        worker.unpark();
    }
}

pub(super) struct Worker {
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn new() -> Self {
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (active.clone(), stop.clone());
        let join = thread::Builder::new()
            .name("oink-mouse-keys".into())
            .spawn(move || run(a, s));
        if let Err(error) = &join {
            tracing::warn!(%error, "Mouse Keys compensation worker unavailable");
        }
        Self {
            active,
            stop,
            join: join.ok(),
        }
    }
    pub fn sharing(&self, active: bool) {
        if self.active.swap(active, Ordering::Release) != active {
            edge(if active { Reason::Sharing } else { Reason::Pause });
            if let Some(join) = &self.join {
                join.thread().unpark();
            }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.thread().unpark();
            let _ = join.join();
        }
    }
}

fn run(active: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    // One lease per interactive session, across backend/probe processes. A second
    // backend must not restore the first backend's crash marker or original state.
    // SAFETY: named kernel mutex, owned/released by this worker only.
    let Ok(mutex) = (unsafe { CreateMutexW(None, false, w!("Local\\OinkMouseKeysCompensation")) }) else {
        return;
    };
    // SAFETY: valid owned handle, zero-timeout acquire on this worker.
    let wait = unsafe { WaitForSingleObject(mutex, 0) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
        // SAFETY: close only our handle; we did not acquire the mutex.
        unsafe {
            let _ = CloseHandle(mutex);
        }
        return;
    }
    let slot = WORKER.get_or_init(|| Snapshot::new(None));
    slot.publish(Some(thread::current()));
    let mut system = Native::new();
    let mut policy = Compensation::default();
    if let Some(marker) = system.load() {
        policy.recover(&mut system, marker);
    }
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let enabled = active.load(Ordering::Acquire);
        let reason = match REASON.swap(0, Ordering::Relaxed) {
            1 => Reason::Sharing,
            2 => Reason::DeviceCreate,
            3 => Reason::DeviceInject,
            4 => Reason::DeviceDestroy,
            5 => Reason::DeviceChange,
            6 => Reason::Local,
            8 => Reason::Pause,
            _ => Reason::Periodic,
        };
        policy.update(&mut system, enabled, if enabled { reason } else { Reason::Pause });
        // Device summaries can arrive ~17ms AFTER Destroy returns (Win11 26200).
        // A 250ms ordinary-thread check also covers third-party synthetic input;
        // inactive sharing with a retained lease still reconciles delayed summaries.
        // Never show Oink's parked cursor here.
        if enabled || policy.owned() {
            thread::park_timeout(Duration::from_millis(250));
        } else {
            thread::park();
        }
    }
    // The hook owner destroys its synthetic devices before dropping this worker.
    // Destruction summaries were measured ~17ms later on build 26200. Preserve
    // compensation across exit and reconcile again after those queued summaries;
    // do not turn MK OFF or leave the session with a stale mouse-present bit.
    for delay in [0, 50, 200] {
        if delay != 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        policy.update(&mut system, false, Reason::Exit);
    }
    slot.publish(None);
    // SAFETY: this thread acquired the mutex and has finished all setting/marker I/O.
    unsafe {
        let _ = ReleaseMutex(mutex);
        let _ = CloseHandle(mutex);
    }
}

struct Native {
    marker: Option<PathBuf>,
}
impl Native {
    fn new() -> Self {
        let mut session = 0;
        // SAFETY: writable session ID for the current process.
        let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.is_ok();
        Self {
            marker: std::env::var_os("LOCALAPPDATA").filter(|_| ok).map(|base| {
                PathBuf::from(base)
                    .join("Oink")
                    .join(format!("mouse-keys-session-{session}.bin"))
            }),
        }
    }
    fn load(&self) -> Option<Marker> {
        let path = self.marker.as_ref()?;
        let bytes = std::fs::read(path).ok()?;
        if bytes.len() != 48 {
            tracing::warn!("Discarding invalid Mouse Keys recovery marker");
            let _ = std::fs::remove_file(path);
            return None;
        }
        let words: [u32; 12] =
            std::array::from_fn(|i| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("fixed slice")));
        Some(Marker {
            original: Settings(words[..6].try_into().expect("six words")),
            applied: Settings(words[6..].try_into().expect("six words")),
        })
    }
}
impl System for Native {
    fn settings(&self) -> Option<Settings> {
        let mut mk = MOUSEKEYS {
            cbSize: size_of::<MOUSEKEYS>() as u32,
            ..Default::default()
        };
        // SAFETY: initialized correctly-sized native structure; no profile writes.
        unsafe {
            SystemParametersInfoW(
                SPI_GETMOUSEKEYS,
                mk.cbSize,
                Some((&mut mk as *mut MOUSEKEYS).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .ok()?;
        Some(Settings([
            mk.dwFlags,
            mk.iMaxSpeed,
            mk.iTimeToMaxSpeed,
            mk.iCtrlSpeed,
            mk.dwReserved1,
            mk.dwReserved2,
        ]))
    }
    fn enable(&mut self, value: Settings) -> bool {
        // Defense in depth: this backend has no automatic MK OFF write path.
        if !value.enabled() {
            return false;
        }
        let [flags, max_speed, time_to_max, ctrl_speed, reserved1, reserved2] = value.0;
        let mut mk = MOUSEKEYS {
            cbSize: size_of::<MOUSEKEYS>() as u32,
            dwFlags: flags,
            iMaxSpeed: max_speed,
            iTimeToMaxSpeed: time_to_max,
            iCtrlSpeed: ctrl_speed,
            dwReserved1: reserved1,
            dwReserved2: reserved2,
        };
        // fWinIni MUST stay zero: compensation is session-only, never persisted.
        // Synthetic PTP lacks an opened PnP handle and cannot maintain win32k's
        // pointing-device count. SendInput/scheme reload do not repair that count;
        // Mouse Keys does (Win11 build 26200). This never calls ShowCursor/SetCursor.
        // SAFETY: valid initialized MOUSEKEYS structure, used only for this call.
        unsafe {
            SystemParametersInfoW(
                SPI_SETMOUSEKEYS,
                mk.cbSize,
                Some((&mut mk as *mut MOUSEKEYS).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .is_ok()
    }
    fn present(&self) -> bool {
        // SAFETY: read-only session metric.
        unsafe { GetSystemMetrics(SM_MOUSEPRESENT) != 0 }
    }
    fn real_mouse(&self) -> bool {
        let mut count = 0;
        let size = size_of::<RAWINPUTDEVICELIST>() as u32;
        // SAFETY: first query size; second supplies capacity. Enumeration races
        // fail closed and are retried next tick. Enumeration is diagnostic evidence,
        // never authorization to switch Mouse Keys OFF.
        unsafe {
            if GetRawInputDeviceList(None, &mut count, size) == u32::MAX || count > 4096 {
                return false;
            }
            let mut devices = vec![RAWINPUTDEVICELIST::default(); count as usize];
            let got = GetRawInputDeviceList(Some(devices.as_mut_ptr()), &mut count, size);
            got != u32::MAX
                && devices
                    .iter()
                    .take(got as usize)
                    .any(|d| d.dwType == RIM_TYPEMOUSE && !d.hDevice.is_invalid())
        }
        // PT_TOUCHPAD synthetic devices have no mouse TLC/raw mouse handle.
        // Mouse Keys' logical present bit and device-less SendInput cannot match.
    }
    fn save(&mut self, marker: Marker) -> bool {
        use std::io::Write;
        let Some(path) = &self.marker else { return false };
        let result = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(path.parent().expect("marker parent"))?;
            let mut file = std::fs::File::create(path)?;
            for word in marker.original.0.into_iter().chain(marker.applied.0) {
                file.write_all(&word.to_le_bytes())?;
            }
            file.sync_all()
        })();
        if let Err(error) = &result {
            tracing::warn!(%error, "Mouse Keys recovery marker write failed");
        }
        result.is_ok()
    }
    fn clear(&mut self) {
        if let Some(path) = &self.marker {
            let _ = std::fs::remove_file(path);
        }
    }
    fn event(&mut self, action: Action, reason: Reason, before: bool, after: bool, success: bool) {
        crate::native_lifecycle::emit(
            crate::native_lifecycle::Kind::MouseKeysCompensation,
            crate::native_lifecycle::Data {
                reason: reason as u32,
                message: action as u32,
                flags: u32::from(before),
                extra: u64::from(after),
                success: Some(success),
                ..Default::default()
            },
        );
        tracing::info!(
            ?action,
            ?reason,
            present_before = before,
            present_after = after,
            success,
            "Mouse Keys cursor compensation (Retained keeps session setting until logoff)"
        );
    }
}
