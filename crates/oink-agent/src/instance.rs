//! Independent backend/UI process ownership across all login sessions.
//! Kernel file locks are authoritative; persistent file existence proves nothing.
//! Data-directory-scoped service.lock and ui.lock are released by process death.

use std::path::Path;

/// 另一个服务进程已经在同一个数据目录上运行。
#[derive(Debug, thiserror::Error)]
#[error("另一个 Oink 服务进程已在运行（数据目录 {0}）")]
pub struct AlreadyRunning(pub String);

/// 服务进程持有的单实例锁；随进程退出释放。
#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
}

/// 哪一种进程的锁。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Service,
    Ui,
}

impl Role {
    fn file(self) -> &'static str {
        match self {
            Role::Service => "service.lock",
            Role::Ui => "ui.lock",
        }
    }
}

/// Stable data-directory identifier used by native foreground-window discovery.
/// This string is not an ownership mutex; ownership uses session-independent files.
pub fn lock_name(data_dir: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data_dir.to_string_lossy().to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("Local\\Oink-service-{h:016x}")
}

fn acquire_role(data_dir: &Path, role: Role) -> anyhow::Result<InstanceLock> {
    #[cfg(windows)]
    {
        windows_impl::acquire(data_dir, role)
    }
    #[cfg(unix)]
    {
        unix_impl::acquire(data_dir, role)
    }
}

/// 取得单实例锁；已有服务进程在同一数据目录上运行时返回 [`AlreadyRunning`]。
pub fn acquire(data_dir: &Path) -> anyhow::Result<InstanceLock> {
    acquire_role(data_dir, Role::Service)
}

/// 界面进程的单实例锁（服务在独立进程里时用）；已有界面进程在同一数据目录上运行时返回 [`AlreadyRunning`]。
pub fn acquire_ui(data_dir: &Path) -> anyhow::Result<InstanceLock> {
    acquire_role(data_dir, Role::Ui)
}

/// Whether a backend owns this data directory. An unqueryable kernel lock is `None`.
pub fn is_running(data_dir: &Path) -> Option<bool> {
    is_role_running(data_dir, Role::Service)
}

/// Whether the UI owns its independent single-instance lock.
pub fn is_ui_running(data_dir: &Path) -> Option<bool> {
    is_role_running(data_dir, Role::Ui)
}

fn is_role_running(data_dir: &Path, role: Role) -> Option<bool> {
    #[cfg(windows)]
    {
        windows_impl::exists(data_dir, role)
    }
    #[cfg(unix)]
    {
        match acquire_role(data_dir, role) {
            Ok(_) => Some(false),
            Err(e) if e.downcast_ref::<AlreadyRunning>().is_some() => Some(true),
            Err(_) => None,
        }
    }
}

#[cfg(unix)]
mod unix_impl {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::{AlreadyRunning, InstanceLock, Role};

    pub(super) fn acquire(data_dir: &Path, role: Role) -> anyhow::Result<InstanceLock> {
        std::fs::create_dir_all(data_dir)?;
        // Never unlink or truncate this file: all processes must lock the same inode.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(data_dir.join(role.file()))?;
        // SAFETY: file owns a valid descriptor; flock is nonblocking and does not retain Rust references.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(AlreadyRunning(data_dir.display().to_string()).into());
            }
            return Err(err.into());
        }
        // Process termination closes the descriptor; Drop explicitly unlocks it.
        Ok(InstanceLock { _file: file })
    }

    impl Drop for InstanceLock {
        fn drop(&mut self) {
            // flock belongs to the open file description shared across fork.
            // CLOEXEC only closes a child's copy at exec; explicitly unlock so
            // a concurrently spawning child cannot extend this owner's lease.
            // SAFETY: the file still owns this descriptor and flock retains no references.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt, path::Path};

    use fs2::FileExt;
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    use super::{AlreadyRunning, InstanceLock, Role};

    pub(super) fn acquire(data_dir: &Path, role: Role) -> anyhow::Result<InstanceLock> {
        std::fs::create_dir_all(data_dir)?;
        // Local\ mutexes are session-scoped: an SSH/installer caller in another
        // session would falsely see no owner. All sessions using this same home
        // share state and IPC, so they must also share one backend and one UI lock.
        // User-only file locks use a Medium minimum integrity label, so an
        // elevated UI and normal backend still share ownership. LockFileEx needs
        // only read access, including when opening an older High-labeled file.
        // Never unlink/truncate or allow deletion of
        // an open lock file: a replacement could otherwise admit a second owner.
        let file = oink_foundation::storage::open_private_lock(&data_dir.join(role.file()))?;
        if let Err(error) = file.try_lock_exclusive() {
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                return Err(AlreadyRunning(data_dir.display().to_string()).into());
            }
            return Err(error.into());
        }
        Ok(InstanceLock { _file: file })
    }

    pub(super) fn exists(data_dir: &Path, role: Role) -> Option<bool> {
        // LockFileEx accepts a read-only handle. Querying a UIAccess owner's
        // file must not require write-up, nor create files from an elevated CLI.
        let file = match OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .open(data_dir.join(role.file()))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return match std::fs::metadata(data_dir) {
                    Ok(meta) if meta.is_dir() => Some(false),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
                    _ => None,
                };
            }
            Err(_) => return None,
        };
        match file.try_lock_exclusive() {
            Ok(()) => Some(false),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Some(true),
            Err(_) => None,
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn file_owner_is_visible_without_a_callers_session_mutex() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().unwrap();
        for (file, running, acquire) in [
            (
                "service.lock",
                is_running as fn(&Path) -> Option<bool>,
                super::acquire as fn(&Path) -> anyhow::Result<InstanceLock>,
            ),
            ("ui.lock", is_ui_running, acquire_ui),
        ] {
            // Simulate an owner in another session: it holds the shared file,
            // but no named mutex exists in the querying caller's namespace.
            let owner = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(temp.path().join(file))
                .unwrap();
            owner.try_lock_exclusive().unwrap();
            assert_eq!(running(temp.path()), Some(true));
            assert!(
                acquire(temp.path())
                    .unwrap_err()
                    .downcast_ref::<AlreadyRunning>()
                    .is_some()
            );
            assert!(!crate::host::stop_service(temp.path(), std::time::Duration::ZERO).unwrap());
            drop(owner);
            assert_eq!(running(temp.path()), Some(false));
            assert!(crate::host::stop_service(temp.path(), std::time::Duration::ZERO).unwrap());
        }
    }

    #[test]
    fn inaccessible_ownership_is_not_successful_stop() {
        let temp = tempfile::tempdir().unwrap();
        let not_a_directory = temp.path().join("not-a-directory");
        std::fs::write(&not_a_directory, b"").unwrap();
        assert_eq!(is_running(&not_a_directory), None);
        assert_eq!(is_ui_running(&not_a_directory), None);
        assert!(crate::host::stop_service(&not_a_directory, std::time::Duration::ZERO).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn open_owner_file_cannot_be_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let owner = acquire(temp.path()).unwrap();
        let path = temp.path().join("service.lock");
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, temp.path().join("replaced.lock")).is_err());
        assert_eq!(is_running(temp.path()), Some(true));
        drop(owner);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn ownership_queries_do_not_require_write_access_or_create_files() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(is_running(temp.path()), Some(false));
        let path = temp.path().join("service.lock");
        assert!(!path.exists());
        let owner = acquire(temp.path()).unwrap();
        let original = std::fs::metadata(&path).unwrap().permissions();
        let mut permissions = original.clone();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions.clone()).unwrap();
        let owned = is_running(temp.path());
        drop(owner);
        let unowned = is_running(temp.path());
        std::fs::set_permissions(path, original).unwrap();
        assert_eq!(owned, Some(true));
        assert_eq!(unowned, Some(false));
    }

    #[test]
    fn lock_name_depends_on_data_dir_only() {
        let a = lock_name(Path::new("C:\\Users\\x\\AppData\\Roaming\\Oink"));
        let b = lock_name(Path::new("c:\\users\\X\\appdata\\roaming\\oink"));
        let c = lock_name(Path::new("C:\\tmp\\oink-test"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("Local\\Oink-service-"));
    }

    #[test]
    fn ui_lock_is_separate_from_service_lock() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        // 服务在运行不妨碍第一个界面；第二个界面被拒绝，直到第一个退出。
        let _service = acquire(dir).unwrap();
        let ui = acquire_ui(dir).unwrap();
        assert!(dir.join("service.lock").is_file());
        assert!(dir.join("ui.lock").is_file());
        let err = acquire_ui(dir).unwrap_err();
        assert!(err.downcast_ref::<AlreadyRunning>().is_some());
        assert_eq!(is_running(dir), Some(true));
        drop(ui);
        drop(acquire_ui(dir).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn dropping_owner_unlocks_a_descriptor_inherited_by_a_spawning_child() {
        let temp = tempfile::tempdir().unwrap();
        let owner = acquire(temp.path()).unwrap();
        // dup and fork share the same open file description. CLOEXEC closes the
        // child's copy at exec, but not during the preceding spawn window.
        let inherited = owner._file.try_clone().unwrap();
        drop(owner);
        let next = acquire(temp.path());
        drop(inherited);
        assert!(
            next.is_ok(),
            "dropping the owner must release its lock before a child's exec"
        );
    }

    #[test]
    fn second_lock_is_refused_until_released() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        assert_eq!(is_running(dir), Some(false));
        let lock = acquire(dir).unwrap();
        assert_eq!(is_running(dir), Some(true));
        let err = acquire(dir).unwrap_err();
        assert!(err.downcast_ref::<AlreadyRunning>().is_some());
        drop(lock);
        assert_eq!(is_running(dir), Some(false));
        drop(acquire(dir).unwrap());
    }
}
