//! Platform storage roots. `OINK_HOME` isolates all roots for a local test instance.
//!
//! A process entry may instead pass its roots explicitly with
//! [`StoragePaths::set_process_roots`] before any storage is touched. The
//! Windows pre-login backend runs as SYSTEM: its per-account folders
//! (`LOCALAPPDATA` of the system profile) must never be used, so that entry
//! fixes machine-level roots and every later `current()` returns them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PROCESS_ROOTS: OnceLock<StoragePaths> = OnceLock::new();

/// The process roots were already fixed by an earlier entry decision.
#[derive(Debug, thiserror::Error)]
#[error("process storage roots are already fixed")]
pub struct RootsAlreadySet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoragePaths {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
}

impl StoragePaths {
    pub fn macos(home: &Path) -> Self {
        Self {
            data: home.join("Library/Application Support/Oink"),
            cache: home.join("Library/Caches/com.hachiwii.oink"),
            logs: home.join("Library/Logs/Oink"),
        }
    }

    pub fn windows(local_app_data: &Path) -> Self {
        Self::isolated(local_app_data.join("Oink"))
    }

    pub fn isolated(data: PathBuf) -> Self {
        Self {
            cache: data.join("Cache"),
            logs: data.join("Logs"),
            data,
        }
    }

    /// Fix this process's roots once, before any storage access. Later
    /// [`Self::current`] calls (and `data_dir`/`log_dir`/`cache_dir`) return
    /// these roots; neither `OINK_HOME` nor the account's platform folders are read.
    pub fn set_process_roots(roots: Self) -> Result<(), RootsAlreadySet> {
        PROCESS_ROOTS.set(roots).map_err(|_| RootsAlreadySet)
    }

    /// Whether an entry fixed this process's roots explicitly.
    pub fn explicit() -> Option<&'static Self> {
        PROCESS_ROOTS.get()
    }

    pub fn current() -> Self {
        if let Some(roots) = PROCESS_ROOTS.get() {
            return roots.clone();
        }
        Self::environment()
    }

    fn environment() -> Self {
        let defaults = Self::platform();
        match std::env::var_os("OINK_HOME") {
            Some(path) if Path::new(&path) != defaults.data => Self::isolated(path.into()),
            _ => defaults,
        }
    }

    fn platform() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::macos(&home())
        }
        #[cfg(windows)]
        {
            // LOCALAPPDATA is the per-user local store; APPDATA is roaming and must
            // never be used as a substitute for identity or delivery files.
            Self::windows(&PathBuf::from(
                std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA is required"),
            ))
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let home = home();
            let root = |key: &str, fallback: &str| {
                std::env::var_os(key)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(fallback))
                    .join("oink")
            };
            Self {
                data: root("XDG_DATA_HOME", ".local/share"),
                cache: root("XDG_CACHE_HOME", ".cache"),
                logs: root("XDG_STATE_HOME", ".local/state").join("Logs"),
            }
        }
    }
}

pub fn data_dir() -> PathBuf {
    StoragePaths::current().data
}
pub fn cache_dir() -> PathBuf {
    StoragePaths::current().cache
}
pub fn log_dir() -> PathBuf {
    StoragePaths::current().logs
}

#[cfg(not(windows))]
fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).expect("HOME is required")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_roots_are_separate() {
        let home = Path::new("user");
        let mac = StoragePaths::macos(home);
        assert_eq!(mac.data, home.join("Library/Application Support/Oink"));
        assert_eq!(mac.cache, home.join("Library/Caches/com.hachiwii.oink"));
        assert_eq!(mac.logs, home.join("Library/Logs/Oink"));
        let win = StoragePaths::windows(Path::new("local"));
        assert_eq!(win.data, Path::new("local/Oink"));
        assert_eq!(win.cache, win.data.join("Cache"));
        assert_eq!(win.logs, win.data.join("Logs"));
    }

    #[test]
    fn explicit_process_roots_replace_environment_and_platform_roots() {
        // The only test in this binary that fixes process roots: run it in a
        // child process so other tests keep the environment-derived defaults.
        if std::env::var_os("OINK_PATHS_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "paths::tests::explicit_process_roots_replace_environment_and_platform_roots",
                    "--exact",
                ])
                .env("OINK_PATHS_CHILD", "1")
                .env("OINK_HOME", "ignored-home")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        assert!(StoragePaths::explicit().is_none());
        let roots = StoragePaths {
            data: PathBuf::from("machine/work"),
            cache: PathBuf::from("machine/work/Cache"),
            logs: PathBuf::from("machine/logs"),
        };
        StoragePaths::set_process_roots(roots.clone()).unwrap();
        assert!(StoragePaths::set_process_roots(StoragePaths::isolated("other".into())).is_err());
        assert_eq!(StoragePaths::current(), roots);
        assert_eq!(data_dir(), roots.data);
        assert_eq!(log_dir(), roots.logs);
        assert_eq!(cache_dir(), roots.cache);
        assert_eq!(StoragePaths::explicit(), Some(&roots));
    }
}
