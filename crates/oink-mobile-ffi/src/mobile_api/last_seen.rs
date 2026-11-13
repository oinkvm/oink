//! When this phone last saw each group member online, kept on disk so the
//! offline device cards show the right "上次在线" right after launch instead
//! of "很久以前" until the member is seen again.
//!
//! Same record and meaning as the PC's `presence.cbor` (oink-agent
//! `config::records::PresenceStore`): this device's UTC wall clock in
//! milliseconds at the end of the live link to that member, never a peer's
//! clock; `{1: 1, 2: [{1: group, 2: device, 3: ms}]}`. Display evidence only.
//!
//! The time of a member that is online now is the latest presence refresh
//! that saw its link ready. It reaches the disk when the member goes offline,
//! when the phone prepares to suspend or shuts down, and while it stays
//! online at most every [`SAVE_EVERY_MS`], so a phone killed without notice
//! loses no more than that. Kept in the device-only directory with the group.

use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v2::{
    RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 1024 * 1024;
/// How stale the stored time of a member that stays online may get.
pub(super) const SAVE_EVERY_MS: u64 = 60_000;

type Key = (GroupId, DeviceId);

#[derive(Debug, Default)]
pub(super) struct LastSeen {
    path: PathBuf,
    seen: BTreeMap<Key, u64>,
    saved: BTreeMap<Key, u64>,
}

impl LastSeen {
    /// An unreadable record starts empty (display evidence only); the next
    /// save replaces it.
    pub(super) fn load(data_dir: &Path) -> Self {
        let path = crate::device_only::last_seen(data_dir);
        let seen = match read(&path) {
            Ok(seen) => seen,
            Err(error) => {
                tracing::warn!(%error, "last seen record unreadable; starting empty");
                BTreeMap::new()
            }
        };
        Self {
            path,
            saved: seen.clone(),
            seen,
        }
    }

    pub(super) fn get(&self, group: GroupId, device: DeviceId) -> Option<u64> {
        self.seen.get(&(group, device)).copied()
    }

    /// `device` is online in `group` at `at_ms`.
    pub(super) fn observe(&mut self, group: GroupId, device: DeviceId, at_ms: u64) {
        self.seen.insert((group, device), at_ms);
    }

    /// Keeps only the members of the phone's current group.
    pub(super) fn retain(&mut self, group: GroupId, member: impl Fn(DeviceId) -> bool) {
        self.seen.retain(|(g, device), _| *g == group && member(*device));
    }

    /// Writes the record when it changed: always with `now`, otherwise when
    /// a member was added or removed or an online member's time moved by
    /// [`SAVE_EVERY_MS`]. A failed write is retried by the next save.
    pub(super) fn save(&mut self, now: bool) {
        if self.seen == self.saved {
            return;
        }
        let due = now
            || self.seen.len() != self.saved.len()
            || self.seen.iter().any(|(key, at)| {
                self.saved
                    .get(key)
                    .is_none_or(|saved| at.saturating_sub(*saved) >= SAVE_EVERY_MS)
            });
        if !due {
            return;
        }
        match write(&self.path, &self.seen) {
            Ok(()) => self.saved = self.seen.clone(),
            Err(error) => tracing::warn!(%error, "last seen record not written"),
        }
    }
}

fn read(path: &Path) -> Result<BTreeMap<Key, u64>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.to_string()),
    };
    let raw = oink_protocol::v2::cbor::decode_map(&bytes, MAX_BYTES).map_err(|error| error.to_string())?;
    if raw.get(&1) != Some(&RawValue::Uint(1)) {
        return Err("unsupported schema".into());
    }
    let Some(RawValue::Array(entries)) = raw.get(&2) else {
        return Err("no entries".into());
    };
    entries
        .iter()
        .map(|entry| entry_of(entry).ok_or_else(|| "invalid entry".to_owned()))
        .collect()
}

fn entry_of(value: &RawValue) -> Option<(Key, u64)> {
    let raw = value.as_map().ok()?;
    let RawValue::Bytes(group) = raw.get(&1)? else {
        return None;
    };
    let group = GroupId(group.as_slice().try_into().ok()?);
    let device = dto::DeviceId::read(raw.get(&2)?).ok()?;
    if device.algorithm().ok()?.0 != "ed25519" {
        return None;
    }
    let device = DeviceId(device.public_key().ok()?.0.try_into().ok()?);
    Some(((group, device), raw.get(&3)?.as_u64().ok()?))
}

fn write(path: &Path, seen: &BTreeMap<Key, u64>) -> Result<(), String> {
    let entries = seen
        .iter()
        .map(|((group, device), at)| {
            let device = dto::DeviceId::from_raw(
                [
                    (1, RawValue::Text("ed25519".into())),
                    (2, RawValue::Bytes(device.0.to_vec())),
                ]
                .into(),
            )
            .map_err(|error| error.to_string())?;
            Ok(RawValue::Map(
                [
                    (1, RawValue::Bytes(group.0.to_vec())),
                    (2, device.write()),
                    (3, at.write()),
                ]
                .into(),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let raw: RawMap = [(1, RawValue::Uint(1)), (2, RawValue::Array(entries))].into();
    let bytes = oink_protocol::v2::cbor::encode_map(&raw, MAX_BYTES).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    oink_foundation::storage::atomic_write(path, &bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUP: GroupId = GroupId([1; 16]);

    fn device(byte: u8) -> DeviceId {
        DeviceId([byte; 32])
    }

    /// Times survive a reload; a member that stays online is rewritten
    /// only once its time moved by the save interval, a forced save writes
    /// any change, and members that left the group are dropped.
    #[test]
    fn times_round_trip_and_an_online_member_is_saved_at_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen = LastSeen::load(dir.path());
        assert_eq!(seen.get(GROUP, device(2)), None);
        seen.observe(GROUP, device(2), 1_000);
        seen.observe(GROUP, device(3), 2_000);
        seen.save(false);
        let reloaded = LastSeen::load(dir.path());
        assert_eq!(reloaded.get(GROUP, device(2)), Some(1_000));
        assert_eq!(reloaded.get(GROUP, device(3)), Some(2_000));

        seen.observe(GROUP, device(2), 1_000 + SAVE_EVERY_MS - 1);
        seen.save(false);
        assert_eq!(LastSeen::load(dir.path()).get(GROUP, device(2)), Some(1_000));
        seen.observe(GROUP, device(2), 1_000 + SAVE_EVERY_MS);
        seen.save(false);
        assert_eq!(
            LastSeen::load(dir.path()).get(GROUP, device(2)),
            Some(1_000 + SAVE_EVERY_MS)
        );
        seen.observe(GROUP, device(2), 1_000 + SAVE_EVERY_MS + 5);
        seen.save(true);
        assert_eq!(
            LastSeen::load(dir.path()).get(GROUP, device(2)),
            Some(1_000 + SAVE_EVERY_MS + 5)
        );

        seen.retain(GROUP, |member| member == device(2));
        seen.save(false);
        let reloaded = LastSeen::load(dir.path());
        assert_eq!(reloaded.get(GROUP, device(3)), None);
        assert_eq!(reloaded.get(GROUP, device(2)), Some(1_000 + SAVE_EVERY_MS + 5));
        seen.retain(GroupId([9; 16]), |_| true);
        assert_eq!(seen.get(GROUP, device(2)), None);
    }

    #[test]
    fn the_record_lives_in_the_device_only_directory_and_a_corrupt_one_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen = LastSeen::load(dir.path());
        seen.observe(GROUP, device(2), 7);
        seen.save(true);
        let path = crate::device_only::last_seen(dir.path());
        assert!(path.exists());
        std::fs::write(&path, b"not cbor").unwrap();
        assert_eq!(LastSeen::load(dir.path()).get(GROUP, device(2)), None);
    }
}
