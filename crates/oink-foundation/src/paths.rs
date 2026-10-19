//! Platform storage roots. `OINK_HOME` isolates all roots for a local test instance.

use std::path::{Path, PathBuf};

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

    pub fn current() -> Self {
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
}
