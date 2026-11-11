//! File logs of `oink-service.exe`: the SYSTEM service (broker and update
//! engine) and its per-session input helpers write daily rolling files into
//! `%ProgramData%\Oink\Logs`, using the same rolling writer and per-directory
//! byte budget as the user backend's logs. `--unregister-credential-provider`
//! is a one-shot installer step and keeps reporting on stderr only.
//!
//! Logging never stops or stalls the service: when the directory cannot be
//! prepared the process runs with stderr logging only, and the file queue drops
//! records instead of waiting on a stuck filesystem. There is no Windows event
//! log source to report that failure to, so it is not recorded anywhere else.
//!
//! Level: `info`, or the `OINK_LOG` directive (`tracing` filter syntax, the
//! same variable as `oink_foundation::logging::init`) from the service's
//! machine environment. Statements here never carry passwords, tokens or user
//! content; auto unlock logs session numbers only.
use super::*;
use oink_foundation::logging::{FileWriter, RollingFiles};
use std::{
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// SYSTEM and Administrators full control; Users may list the directory and
/// read the files but never write. SYSTEM must not write where an ordinary user
/// could plant a link or junction. Protected (`P`): nothing inherited from
/// ProgramData, whose default ACL lets users create entries. Files inherit it.
pub(super) const DIRECTORY_SDDL: &str = "O:SYG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";
/// The service has no settings of its own; its logs are small.
const MAX_MB: u64 = 100;
/// Upper bound for writing queued records at exit.
const FLUSH: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    /// The OinkInput service: broker, launch broker, pre-login supervisor and
    /// update engine.
    Service,
    /// `--helper`: the input helper of one console session.
    Helper { session: u32 },
    /// `--routed-helper`: the pre-login source's helper of one console session.
    RoutedHelper { session: u32 },
}
impl Role {
    /// The rolling-file role, i.e. the file name before `.<date>.log`.
    fn file_role(self) -> String {
        match self {
            Self::Service => "oink-service".into(),
            Self::Helper { session } => format!("oink-service-helper-{session}"),
            Self::RoutedHelper { session } => format!("oink-service-routed-{session}"),
        }
    }
}

/// `None` inside: initialized without a file (directory unavailable).
static WRITER: OnceLock<Option<FileWriter>> = OnceLock::new();

/// Install this process's subscriber. Later calls do nothing.
pub(super) fn init(role: Role) {
    init_with(&WRITER, role, directory);
}

/// Log how the mode ended and write queued records before the process exits.
pub(super) fn finish(role: Role, result: io::Result<()>) -> io::Result<()> {
    match &result {
        Ok(()) => tracing::info!(?role, "exiting"),
        Err(error) => tracing::error!(?role, %error, "exiting with error"),
    }
    flush();
    result
}

/// Bounded: a stuck filesystem never holds up a stop.
pub(super) fn flush() {
    if let Some(Some(writer)) = WRITER.get() {
        let _ = writer.drain_until(Instant::now() + FLUSH);
    }
}

/// `%ProgramData%\Oink\Logs`, created with [`DIRECTORY_SDDL`] below the
/// protected `%ProgramData%\Oink` that the pre-login store also uses. An
/// existing directory must have a privileged owner and no unprivileged write
/// access, or file logging is refused.
fn directory() -> io::Result<PathBuf> {
    let (product, _pinned) = super::prelogin::pin_product(true)?;
    let path = product.join("Logs");
    super::prelogin::protected_dir_with(&path, Some(DIRECTORY_SDDL))?;
    Ok(path)
}

fn open(role: Role, directory: impl FnOnce() -> io::Result<PathBuf>) -> io::Result<FileWriter> {
    let files = RollingFiles::new(directory()?, &role.file_role(), || MAX_MB)?;
    FileWriter::lossy(files)
}

/// Returns whether this call installed the subscriber.
fn init_with(
    state: &OnceLock<Option<FileWriter>>,
    role: Role,
    directory: impl FnOnce() -> io::Result<PathBuf>,
) -> bool {
    let mut installed = false;
    state.get_or_init(|| {
        installed = true;
        let file = open(role, directory);
        let writer = file.as_ref().ok().cloned();
        install(writer.clone());
        match file {
            Ok(_) => tracing::info!(?role, version = env!("CARGO_PKG_VERSION"), "file logging started"),
            Err(error) => tracing::warn!(?role, %error, "file logging unavailable; continuing without it"),
        }
        writer
    });
    installed
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_env("OINK_LOG").unwrap_or_else(|_| EnvFilter::new("info"))
}

fn install(file: Option<FileWriter>) {
    let file_layer = file.clone().map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(filter())
    });
    // Service processes have no console; stderr only matters when run by hand.
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter());
    if tracing_subscriber::registry()
        .with(stderr)
        .with(file_layer)
        .try_init()
        .is_err()
    {
        return;
    }
    if let Some(writer) = file {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            writer.record_panic(format!("ERROR oink_panic: {info}\n"));
            previous(info);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{Security::Authorization::*, Storage::FileSystem::*};

    #[test]
    fn each_role_has_its_own_rolling_file() {
        assert_eq!(Role::Service.file_role(), "oink-service");
        assert_eq!(Role::Helper { session: 3 }.file_role(), "oink-service-helper-3");
        assert_eq!(
            Role::RoutedHelper { session: u32::MAX }.file_role(),
            "oink-service-routed-4294967295"
        );
        let dir = tempfile::tempdir().unwrap();
        for role in [
            Role::Service,
            Role::Helper { session: 1 },
            Role::RoutedHelper { session: 1 },
        ] {
            RollingFiles::new(dir.path().into(), &role.file_role(), || MAX_MB).unwrap();
        }
    }

    #[test]
    fn directory_acl_lets_users_read_but_never_write() {
        super::super::update::protected::sddl_policy(DIRECTORY_SDDL).unwrap();
        // The owner part needs SeRestorePrivilege; the DACL is what files inherit.
        let dacl = DIRECTORY_SDDL.split_once("D:").unwrap().1;
        let fixture = tempfile::tempdir().unwrap();
        let dir = fixture.path().join("Logs");
        let sd = security::descriptor(&format!("D:{dacl}")).unwrap();
        let attrs = sd.attrs();
        // SAFETY: fresh child of a private temporary directory, live descriptor.
        check(unsafe { CreateDirectoryW(wide(dir.to_str().unwrap()).as_ptr(), &attrs) }).unwrap();
        let file = dir.join("oink-service.2026-01-01.log");
        std::fs::write(&file, b"record").unwrap();
        let users = |path: &std::path::Path| -> Vec<(u32, u8)> {
            let (mut dacl, mut descriptor) = (ptr::null_mut(), ptr::null_mut());
            let mut users_sid = [0u8; 68];
            let mut size = users_sid.len() as u32;
            // SAFETY: fixed well-known SID into a sufficiently large buffer;
            // Windows allocates the descriptor, freed below.
            unsafe {
                check(CreateWellKnownSid(
                    WinBuiltinUsersSid,
                    ptr::null_mut(),
                    users_sid.as_mut_ptr().cast(),
                    &mut size,
                ))
                .unwrap();
                let code = GetNamedSecurityInfoW(
                    wide(path.to_str().unwrap()).as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut descriptor,
                );
                assert_eq!(code, 0);
                let mut grants = Vec::new();
                for index in 0..u32::from((*dacl).AceCount) {
                    let mut entry = ptr::null_mut();
                    check(GetAce(dacl, index, &mut entry)).unwrap();
                    let header = &*entry.cast::<ACE_HEADER>();
                    let ace = &*entry.cast::<ACCESS_ALLOWED_ACE>();
                    let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
                    if EqualSid(sid, users_sid.as_mut_ptr().cast()) != 0 {
                        grants.push((ace.Mask, header.AceType));
                    }
                }
                LocalFree(descriptor);
                grants
            }
        };
        let write = FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | FILE_WRITE_EA
            | FILE_WRITE_ATTRIBUTES
            | FILE_DELETE_CHILD
            | DELETE
            | WRITE_DAC
            | WRITE_OWNER
            | GENERIC_WRITE
            | GENERIC_ALL;
        for path in [&dir, &file] {
            let grants = users(path);
            assert_eq!(grants.len(), 1, "{path:?}: {grants:?}");
            let (mask, kind) = grants[0];
            assert_eq!(kind, 0, "allow ACE");
            assert_eq!(mask & write, 0, "{path:?}: {mask:#x}");
            // FILE_READ_DATA is FILE_LIST_DIRECTORY on the directory.
            assert_ne!(mask & FILE_READ_DATA, 0, "{path:?}: {mask:#x}");
        }
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    const CHILD: &str = "OINK_WINSVC_LOGS_CHILD";

    /// Init installs the process-global subscriber, so each init test runs
    /// itself again in a child process. Returns the child's fixture directory
    /// when running as that child.
    fn in_child(test: &str, fixture: &std::path::Path) -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os(CHILD) {
            return Some(dir.into());
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([&format!("platform::logs::tests::{test}"), "--exact"])
            .env(CHILD, fixture)
            .env_remove("OINK_LOG")
            .status()
            .unwrap();
        assert!(status.success());
        None
    }

    #[test]
    fn unavailable_directory_continues_without_file_logs() {
        let fixture = tempfile::tempdir().unwrap();
        if in_child("unavailable_directory_continues_without_file_logs", fixture.path()).is_none() {
            assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
            return;
        }
        let state = OnceLock::new();
        let mut calls = 0;
        assert!(init_with(&state, Role::Helper { session: 2 }, || {
            calls += 1;
            Err(io::ErrorKind::PermissionDenied.into())
        }));
        assert!(!init_with(&state, Role::Helper { session: 2 }, || unreachable!()));
        assert_eq!(calls, 1);
        assert!(state.get().unwrap().is_none());
        tracing::info!("record without a file");
    }

    #[test]
    fn init_is_idempotent_and_writes_only_the_first_roles_file() {
        let fixture = tempfile::tempdir().unwrap();
        let Some(dir) = in_child(
            "init_is_idempotent_and_writes_only_the_first_roles_file",
            fixture.path(),
        ) else {
            let names: Vec<_> = std::fs::read_dir(fixture.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| name.ends_with(".log"))
                .collect();
            assert_eq!(names.len(), 1, "{names:?}");
            assert!(names[0].starts_with("oink-service-helper-7."), "{names:?}");
            let text = std::fs::read_to_string(fixture.path().join(&names[0])).unwrap();
            assert!(text.contains("helper record"), "{text}");
            assert!(!text.contains("filtered debug"), "{text}");
            return;
        };
        let state = OnceLock::new();
        assert!(init_with(&state, Role::Helper { session: 7 }, || Ok(dir.clone())));
        assert!(!init_with(&state, Role::Helper { session: 7 }, || unreachable!()));
        assert!(!init_with(&state, Role::Service, || unreachable!()));
        // A second subscriber (as from another mode's init) is refused quietly.
        install(None);
        tracing::info!("helper record");
        tracing::debug!("filtered debug");
        assert!(
            state
                .get()
                .unwrap()
                .as_ref()
                .unwrap()
                .drain_until(Instant::now() + FLUSH)
        );
    }
}
