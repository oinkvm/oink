//! This phone's own device name. The host supplies the system device name
//! (`MobileConfig.device_name`, e.g. "My iPhone" / the Android model or
//! user-set device name) as the default; a name the user sets in Oink's
//! settings overrides it and is persisted here. The effective name is what
//! the group sees: it is published in this phone's DeviceFacts (PC device
//! list) and in its discovery advert.
//!
//! The custom name belongs to this phone (user decision 2026-11-10): it is
//! kept in the device-only directory, so it is neither backed up nor migrated
//! and a new phone starts with its own system name.

use super::*;
use std::path::{Path, PathBuf};

const FILE: &str = "device-name.json";
const MAX_NAME_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LocalDeviceName {
    /// The name the group sees.
    pub name: String,
    /// The host-supplied system device name (the default).
    pub system_name: String,
    /// True when the user set a custom name in Oink.
    pub custom: bool,
}

#[derive(Debug)]
struct State {
    system: String,
    custom: Option<String>,
}

#[derive(Debug)]
pub(super) struct DeviceNames {
    path: PathBuf,
    state: Mutex<State>,
}

pub(super) fn valid(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && name.len() <= MAX_NAME_BYTES).then(|| name.to_owned())
}

fn invalid() -> MobileError {
    MobileError::Command {
        code: UserFacingCode::InvalidValue,
        detail: "device name must be 1-256 bytes".into(),
    }
}

impl DeviceNames {
    /// A corrupt name file is a start failure rather than a silent reset.
    pub(super) fn load(data_dir: &Path, system: &str) -> Result<Self, MobileError> {
        let system = valid(system).ok_or_else(|| MobileError::InvalidArgument {
            detail: "device_name (system device name) must be 1-256 bytes".into(),
        })?;
        let path = crate::device_only::dir(data_dir).join(FILE);
        let custom = match std::fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<String>(&bytes)
                    .ok()
                    .and_then(|name| valid(&name))
                    .ok_or_else(|| MobileError::Startup {
                        detail: "custom device name file is corrupt".into(),
                    })?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(MobileError::Startup {
                    detail: format!("custom device name: {error}"),
                });
            }
        };
        Ok(Self {
            path,
            state: Mutex::new(State { system, custom }),
        })
    }

    pub(super) fn view(&self) -> LocalDeviceName {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        LocalDeviceName {
            name: state.custom.clone().unwrap_or_else(|| state.system.clone()),
            system_name: state.system.clone(),
            custom: state.custom.is_some(),
        }
    }

    /// `None` returns to the system name. Persists before it applies.
    fn set_custom(&self, name: Option<&str>) -> Result<LocalDeviceName, MobileError> {
        let custom = name.map(|name| valid(name).ok_or_else(invalid)).transpose()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match &custom {
            Some(name) => {
                let bytes = serde_json::to_vec(name).map_err(|error| persistence(error.to_string()))?;
                oink_foundation::storage::atomic_write(&self.path, &bytes)
                    .map_err(|error| persistence(error.to_string()))?;
            }
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(persistence(error.to_string())),
            },
        }
        state.custom = custom;
        drop(state);
        Ok(self.view())
    }

    fn set_system(&self, name: &str) -> Result<LocalDeviceName, MobileError> {
        let system = valid(name).ok_or_else(invalid)?;
        self.state.lock().unwrap_or_else(|error| error.into_inner()).system = system;
        Ok(self.view())
    }
}

fn persistence(detail: String) -> MobileError {
    MobileError::Command {
        code: UserFacingCode::PersistenceFailed,
        detail,
    }
}

/// The effective name for a data directory, used before a session exists
/// (first-launch group creation, offline management).
pub(super) fn effective(data_dir: &Path, system: &str) -> Result<String, MobileError> {
    Ok(DeviceNames::load(data_dir, system)?.view().name)
}

impl MobileSession {
    fn publish_local_name(&self, view: LocalDeviceName) -> Result<LocalDeviceName, MobileError> {
        self.network
            .rename_local(view.name.clone())
            .map_err(device_control::membership_error)?;
        self.refresh_presence();
        Ok(view)
    }

    pub(super) fn set_custom_device_name(&self, name: Option<&str>) -> Result<LocalDeviceName, MobileError> {
        let view = self.names.set_custom(name)?;
        self.publish_local_name(view)
    }
}

#[uniffi::export]
impl MobileSession {
    /// This phone's name as the group sees it, plus the system default.
    pub fn local_device_name(&self) -> LocalDeviceName {
        self.names.view()
    }

    /// Set a custom name (settings page), or `None` to use the system name
    /// again. The new name reaches every online group member immediately.
    pub fn set_local_device_name(&self, name: Option<String>) -> Result<LocalDeviceName, MobileError> {
        self.set_custom_device_name(name.as_deref())
    }

    /// The host reports a changed system device name while running. It only
    /// changes the published name when no custom name is set.
    pub fn update_system_device_name(&self, name: String) -> Result<LocalDeviceName, MobileError> {
        let view = self.names.set_system(&name)?;
        self.publish_local_name(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_name_overrides_and_resets_to_the_system_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(crate::device_only::dir(dir.path())).unwrap();
        let names = DeviceNames::load(dir.path(), " Pixel 8 ").unwrap();
        assert_eq!(names.view().name, "Pixel 8");
        assert!(!names.view().custom);
        names.set_custom(Some("  Desk phone ")).unwrap();
        assert_eq!(effective(dir.path(), "Pixel 8").unwrap(), "Desk phone");
        assert!(names.set_custom(Some("   ")).is_err());
        names.set_system("Pixel 9").unwrap();
        assert_eq!(names.view().name, "Desk phone");
        names.set_custom(None).unwrap();
        assert_eq!(names.view().name, "Pixel 9");
        assert_eq!(effective(dir.path(), "Pixel 8").unwrap(), "Pixel 8");
        std::fs::write(crate::device_only::dir(dir.path()).join(FILE), b"7").unwrap();
        assert!(DeviceNames::load(dir.path(), "Pixel 8").is_err());
    }

    /// The custom name is read from and written to the device-only
    /// directory (not backed up); a name file in the data root is ignored.
    #[test]
    fn custom_name_lives_in_the_device_only_directory() {
        let dir = tempfile::tempdir().unwrap();
        let device_only = crate::device_only::dir(dir.path());
        std::fs::create_dir(&device_only).unwrap();
        std::fs::write(dir.path().join(FILE), b"\"Old phone\"").unwrap();
        assert_eq!(effective(dir.path(), "Pixel 9").unwrap(), "Pixel 9");

        let names = DeviceNames::load(dir.path(), "Pixel 9").unwrap();
        names.set_custom(Some("Desk phone")).unwrap();
        assert_eq!(std::fs::read(device_only.join(FILE)).unwrap(), b"\"Desk phone\"");
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), b"\"Old phone\"");

        std::fs::write(device_only.join(FILE), b"\"Kitchen\"").unwrap();
        assert_eq!(effective(dir.path(), "Pixel 9").unwrap(), "Kitchen");
    }
}
