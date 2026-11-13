//! Files that are only valid on this phone live in one subdirectory of
//! `MobileConfig.data_dir`, so the hosts can keep them out of system backup
//! and device migration with a single rule (iOS: `isExcludedFromBackup` on
//! the directory, which also covers every file the core later replaces
//! atomically inside it; Android: `data_extraction_rules.xml`).
//!
//! The identity is never migrated (iOS Keychain `ThisDeviceOnly`; the Android
//! `device.key` is in here too). Everything that names this identity or this
//! phone's group membership therefore stays behind as well: a new phone starts
//! unpaired instead of carrying another device's group state (D8 keeps
//! `MobileGroupProvision::Replaced` for state restored some other way). The
//! custom device name (`device-name.json`) is kept here too: a new phone
//! starts with its own system name. Transfer history, attachments and the
//! other settings stay in `data_dir` itself and are backed up as usual.
//!
//! The hosts spell the directory name out (iOS `DeviceOnlyStorage`, Android
//! `res/xml/data_extraction_rules.xml` and `backup_rules.xml`); a rename must
//! change them too.

use std::path::{Path, PathBuf};

pub(crate) const DIR: &str = "device-only";

pub(crate) fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

/// Signed roster with this phone as the local member.
pub(crate) fn group_state(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("group-state.v2")
}

/// Pairing recovery journal; oink-mobile-core keeps the pairing block list
/// (`pairing-blocks.json`) next to it.
pub(crate) fn pairing_journal(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("pairing-recovery.cbor")
}

/// The last online transfer targets (`cached_transfer_targets`): device ids
/// and names of this phone's group.
pub(crate) fn transfer_targets(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("transfer-targets.json")
}

/// When each group member was last seen online (same record as the PC's
/// `presence.cbor`).
pub(crate) fn last_seen(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("presence.cbor")
}

/// Legacy trust file path (never grants membership; kept with the group).
pub(crate) fn trust(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("mobile-trusted-members")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// iOS and Android name this directory in their backup rules.
    #[test]
    fn directory_name_matches_the_host_backup_rules() {
        assert_eq!(DIR, "device-only");
        let data = Path::new("data");
        for path in [
            group_state(data),
            pairing_journal(data),
            trust(data),
            transfer_targets(data),
            last_seen(data),
        ] {
            assert_eq!(path.parent(), Some(dir(data).as_path()));
        }
    }
}
