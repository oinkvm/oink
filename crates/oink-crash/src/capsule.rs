use oink_telemetry::{capture::Manifest, *};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

const MAGIC: [u8; 8] = *b"OINKCR03";
const SIZE: usize = 128;
#[derive(Debug)]
struct ArmedFile {
    file: File,
    path: PathBuf,
    component: Component,
    build: BuildContext,
    capability: [u8; 32],
}
#[derive(Debug, Default)]
pub struct Capture {
    generation: AtomicU64,
    file: Mutex<Option<ArmedFile>>,
}
thread_local! {
    static PANIC_LOCATION: std::cell::Cell<(u32,u32)> = const { std::cell::Cell::new((0,0)) };
}

/// The process entry point owns this boundary. Caught panics never write a
/// report candidate; an escaping panic is committed immediately before abort.
pub fn fatal_boundary<T>(run: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(value) => value,
        Err(_) => {
            if let Some(capture) = HOOK.get() {
                let (line, column) = PANIC_LOCATION.try_with(std::cell::Cell::get).unwrap_or_default();
                capture.record(CrashKind::RustPanic, 0, line, column);
            }
            std::process::abort();
        }
    }
}

static HOOK: OnceLock<Arc<Capture>> = OnceLock::new();
impl Capture {
    /// Compose once. The old hook's local logging behavior is unchanged; none of
    /// its output or PanicHookInfo payload/file/thread enters the capsule.
    pub fn install() -> Arc<Self> {
        HOOK.get_or_init(|| {
            let state = Arc::new(Self::default());
            let hook = state.clone();
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let (line, column) = info.location().map(|l| (l.line(), l.column())).unwrap_or_default();
                let _ = PANIC_LOCATION.try_with(|location| location.set((line, column)));
                // Abort-profile panics are intrinsically fatal. Unwinding
                // profiles commit only at the application entry boundary.
                if cfg!(panic = "abort") {
                    hook.record(CrashKind::RustPanic, 0, line, column);
                }
                previous(info);
            }));
            #[cfg(windows)]
            crate::windows::install();
            state
        })
        .clone()
    }
    pub fn arm(&self, path: &Path, generation: u64, component: Component, build: BuildContext) -> Result<()> {
        self.arm_with_capability(path, generation, component, build, [0; 32])
    }
    pub fn arm_with_capability(
        &self,
        path: &Path,
        generation: u64,
        component: Component,
        build: BuildContext,
        capability: [u8; 32],
    ) -> Result<()> {
        if generation == 0 {
            return Err(Error::Invalid);
        }
        let parent = path.parent().ok_or(Error::Io)?;
        std::fs::create_dir_all(parent).map_err(|_| Error::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(|_| Error::Io)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path).map_err(|_| Error::Io)?;
        file.set_len(SIZE as u64).map_err(|_| Error::Io)?;
        let mut slot = self.file.lock().map_err(|_| Error::Io)?;
        *slot = Some(ArmedFile {
            file,
            path: path.to_path_buf(),
            component,
            build,
            capability,
        });
        self.generation.store(generation, Ordering::Release);
        Ok(())
    }
    pub fn disarm(&self) {
        self.generation.store(0, Ordering::Release);
        if let Ok(mut slot) = self.file.lock()
            && let Some(ArmedFile { file, path, .. }) = slot.take()
        {
            // Close first: Windows otherwise retains the capsule after an orderly
            // exit, which could turn a caught panic into a report on next launch.
            drop(file);
            let _ = std::fs::remove_file(path);
        }
    }
    /// Fixed-size best effort, no allocator, formatter, panic payload, or blocking
    /// report lock. A busy slot loses coverage instead of delaying a faulting thread.
    pub fn record(&self, kind: CrashKind, code: u32, line: u32, column: u32) {
        let generation = self.generation.load(Ordering::Acquire);
        if generation == 0 {
            return;
        }
        let Ok(slot) = self.file.try_lock() else { return };
        let Some(file) = slot.as_ref() else { return };
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        let bytes = encode(
            generation,
            file.component,
            kind,
            code,
            line,
            column,
            oink_telemetry::now(),
            file.build,
            file.capability,
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let _ = file.file.write_at(&bytes, 0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let _ = file.file.seek_write(&bytes, 0);
        }
    }
}
#[cfg(windows)]
pub(crate) fn native_exception(code: u32) {
    if let Some(hook) = HOOK.get() {
        hook.record(CrashKind::NativeException, code, 0, 0);
    }
}
// Fixed binary layout keeps the fatal hook independent of serializers.
#[allow(clippy::too_many_arguments)]
fn encode(
    generation: u64,
    component: Component,
    kind: CrashKind,
    code: u32,
    line: u32,
    column: u32,
    at: u64,
    build: BuildContext,
    capability: [u8; 32],
) -> [u8; SIZE] {
    let mut out = [0; SIZE];
    out[..8].copy_from_slice(&MAGIC);
    out[8..16].copy_from_slice(&generation.to_le_bytes());
    out[16] = component as u8;
    out[17] = kind as u8;
    out[20..24].copy_from_slice(&code.to_le_bytes());
    out[24..28].copy_from_slice(&line.to_le_bytes());
    out[28..32].copy_from_slice(&column.to_le_bytes());
    out[32..40].copy_from_slice(&at.to_le_bytes());
    out[40..72].copy_from_slice(&capability);
    out[72..74].copy_from_slice(&build.app_version.major.to_le_bytes());
    out[74..76].copy_from_slice(&build.app_version.minor.to_le_bytes());
    out[76..78].copy_from_slice(&build.app_version.patch.to_le_bytes());
    out[78..82].copy_from_slice(&build.app_version.build.to_le_bytes());
    (out[82], out[83]) = match build.os_version_class {
        OsVersionClass::Windows10 => (1, 0),
        OsVersionClass::Windows11 => (2, 0),
        OsVersionClass::MacOSMajor(m) => (3, m),
        OsVersionClass::Unknown => (0, 0),
    };
    let check = out[..120].iter().fold(0u64, |a, b| a.rotate_left(7) ^ *b as u64);
    out[120..].copy_from_slice(&check.to_le_bytes());
    out
}
pub fn recover(
    path: &Path,
    consent: ReportConsent,
    manifest: &Manifest,
    _os: OsVersionClass,
    now: u64,
) -> Option<(SafeCrash, u64)> {
    recover_with_capability(path, consent, manifest, now, [0; 32])
}
pub fn recover_with_capability(
    path: &Path,
    consent: ReportConsent,
    manifest: &Manifest,
    now: u64,
    capability: [u8; 32],
) -> Option<(SafeCrash, u64)> {
    if !consent.granted() {
        return None;
    }
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() != SIZE as u64 {
        return None;
    }
    let mut bytes = [0; SIZE];
    use std::io::Read;
    (&file).read_exact(&mut bytes).ok()?;
    let u64_at = |i| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
    if bytes[..8] != MAGIC
        || u64_at(8) != consent.generation
        || u64_at(120) != bytes[..120].iter().fold(0u64, |a, b| a.rotate_left(7) ^ *b as u64)
    {
        return None;
    }
    if bytes[40..72] != capability {
        return None;
    }
    let version = AppVersion {
        major: u16::from_le_bytes(bytes[72..74].try_into().ok()?),
        minor: u16::from_le_bytes(bytes[74..76].try_into().ok()?),
        patch: u16::from_le_bytes(bytes[76..78].try_into().ok()?),
        build: u32::from_le_bytes(bytes[78..82].try_into().ok()?),
    };
    if version != manifest.version {
        return None;
    }
    let os = match bytes[82] {
        0 => OsVersionClass::Unknown,
        1 => OsVersionClass::Windows10,
        2 => OsVersionClass::Windows11,
        3 => OsVersionClass::MacOSMajor(bytes[83]),
        _ => return None,
    };
    let at = u64_at(32);
    if at > now || now - at >= 7 * 86400 {
        return None;
    }
    let component = match bytes[16] {
        0 => Component::Agent,
        1 => Component::UiRust,
        2 => Component::UiWebview,
        3 => Component::FileProvider,
        _ => return None,
    };
    let kind = match bytes[17] {
        0 => CrashKind::RustPanic,
        1 => CrashKind::NativeException,
        2 => CrashKind::SwiftTrap,
        3 => CrashKind::JsUnhandled,
        4 => CrashKind::Unknown,
        _ => return None,
    };
    let mut crash = SafeCrash::empty(
        BuildContext {
            app_version: manifest.version,
            os_version_class: os,
        },
        component,
        kind,
        if kind == CrashKind::RustPanic {
            ExceptionClass::Panic
        } else {
            ExceptionClass::Unknown
        },
    );
    let u32_at = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
    crash.exception_code = (u32_at(20) != 0).then_some(u32_at(20));
    crash.line = (u32_at(24) != 0).then_some(u32_at(24));
    crash.column = (u32_at(28) != 0).then_some(u32_at(28));
    Some((crash, at))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        Manifest {
            version: AppVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 1,
            },
            images: vec![],
        }
    }
    #[test]
    fn bounded_capsule_off_stale_corrupt_and_repeated_faults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capsule");
        let c = Capture::default();
        c.record(CrashKind::RustPanic, 0, 9, 1);
        assert!(!path.exists());
        c.arm(
            &path,
            1,
            Component::Agent,
            BuildContext {
                app_version: manifest().version,
                os_version_class: OsVersionClass::Unknown,
            },
        )
        .unwrap();
        for _ in 0..100 {
            c.record(CrashKind::RustPanic, 0, 9, 1);
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128);
        let consent = ReportConsent {
            choice: Choice::Granted,
            policy_version: 3,
            generation: 1,
        };
        let (crash, _) = recover(&path, consent, &manifest(), OsVersionClass::Unknown, now()).unwrap();
        assert_eq!(crash.line, Some(9));
        assert!(
            recover(
                &path,
                ReportConsent {
                    generation: 2,
                    ..consent
                },
                &manifest(),
                OsVersionClass::Unknown,
                now()
            )
            .is_none()
        );
        c.disarm();
        assert!(!path.exists());
        c.record(CrashKind::RustPanic, 0, 1, 1);
        assert!(!path.exists());
    }
    #[test]
    fn caught_panic_then_orderly_exit_never_leaves_a_report() {
        const KEY: &str = "OINK_TEST_CAUGHT_PANIC";
        if let Some(path) = std::env::var_os(KEY) {
            let capture = Capture::install();
            capture
                .arm(
                    Path::new(&path),
                    1,
                    Component::Agent,
                    BuildContext {
                        app_version: manifest().version,
                        os_version_class: OsVersionClass::Unknown,
                    },
                )
                .unwrap();
            assert!(std::panic::catch_unwind(|| panic!("private handled error")).is_err());
            capture.disarm();
            assert!(!Path::new(&path).exists());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capsule");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capsule::tests::caught_panic_then_orderly_exit_never_leaves_a_report",
                "--nocapture",
            ])
            .env(KEY, &path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!path.exists());
    }
    #[test]
    fn panic_subprocess_capture() {
        const KEY: &str = "OINK_CRASH_UNIT_CHILD";
        if let Some(path) = std::env::var_os(KEY) {
            let capture = Capture::install();
            capture
                .arm(
                    Path::new(&path),
                    1,
                    Component::Agent,
                    BuildContext {
                        app_version: manifest().version,
                        os_version_class: OsVersionClass::Unknown,
                    },
                )
                .unwrap();
            fatal_boundary(|| panic!("poison-private-content must never enter capsule"));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capsule");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "capsule::tests::panic_subprocess_capture", "--nocapture"])
            .env(KEY, &path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 128);
        assert!(!String::from_utf8_lossy(&bytes).contains("poison"));
        assert!(
            recover(
                &path,
                ReportConsent {
                    choice: Choice::Granted,
                    policy_version: 3,
                    generation: 1
                },
                &manifest(),
                OsVersionClass::Unknown,
                now()
            )
            .is_some()
        );
    }
}

#[cfg(test)]
mod handled_regression {
    use super::*;
    #[test]
    fn a_caught_panic_followed_by_abnormal_exit_is_not_a_fatal_report() {
        const CHILD: &str = "OINK_CAUGHT_PANIC_UNIT_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let capture = Capture::install();
            capture
                .arm(
                    Path::new(&path),
                    1,
                    Component::Agent,
                    BuildContext {
                        app_version: AppVersion {
                            major: 0,
                            minor: 1,
                            patch: 0,
                            build: 1,
                        },
                        os_version_class: OsVersionClass::Unknown,
                    },
                )
                .unwrap();
            assert!(std::panic::catch_unwind(|| panic!("private handled error")).is_err());
            std::process::exit(77);
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capsule");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capsule::handled_regression::a_caught_panic_followed_by_abnormal_exit_is_not_a_fatal_report",
                "--nocapture",
            ])
            .env(CHILD, &path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(77));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, [0; SIZE]);
        assert!(
            recover(
                &path,
                ReportConsent {
                    choice: Choice::Granted,
                    policy_version: POLICY_VERSION,
                    generation: 1
                },
                &Manifest {
                    version: AppVersion {
                        major: 0,
                        minor: 1,
                        patch: 0,
                        build: 1
                    },
                    images: vec![]
                },
                OsVersionClass::Unknown,
                now()
            )
            .is_none()
        );
    }
}
