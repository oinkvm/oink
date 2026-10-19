//! Local policy and presence records. Presence is display evidence only; neither
//! store authorizes membership or a live session. Unknown fields round-trip.
use super::store::{self, StoreError};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{RawMap, RawValue, schema::WireValue};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub const BLOCKS_FILE: &str = "pairing-blocks.cbor";
pub const PRESENCE_FILE: &str = "presence.cbor";
pub const MAX_BLOCKS: usize = 1024;
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedDevice {
    pub device: DeviceId,
    pub name: Option<String>,
    pub os: Option<String>,
    pub blocked_at_ms: u64,
}

#[derive(Debug)]
pub struct BlockList {
    path: PathBuf,
    raw: RawMap,
}
impl BlockList {
    pub fn load(data_dir: &Path) -> Result<Self, StoreError> {
        let path = data_dir.join(BLOCKS_FILE);
        let raw = store::read(&path, MAX_RECORD_BYTES)?.unwrap_or_else(store::empty);
        validate_blocks(&raw)?;
        Ok(Self { path, raw })
    }
    pub fn entries(&self) -> Vec<BlockedDevice> {
        store::entries(&self.raw)
            .expect("validated entries")
            .iter()
            .map(|entry| parse_block(entry).expect("validated block"))
            .collect()
    }
    pub fn contains(&self, device: DeviceId) -> bool {
        self.entries().iter().any(|entry| entry.device == device)
    }
    pub fn block(&mut self, entry: BlockedDevice) -> Result<(), StoreError> {
        let mut candidate = self.raw.clone();
        let RawValue::Array(entries) = candidate.get_mut(&2).expect("validated entries") else {
            unreachable!()
        };
        let index = entries
            .iter()
            .position(|v| parse_block(v).expect("validated block").device == entry.device);
        if index.is_none() && entries.len() == MAX_BLOCKS {
            return Err(StoreError::LimitExceeded);
        }
        let mut raw = index
            .map(|i| entries[i].as_map().expect("validated block").clone())
            .unwrap_or_default();
        raw.entry(1).or_insert_with(|| store::wire_device(entry.device).write());
        for (key, value) in [(2, entry.name), (3, entry.os)] {
            if let Some(value) = value {
                raw.insert(key, RawValue::Text(value));
            } else {
                raw.remove(&key);
            }
        }
        raw.insert(4, entry.blocked_at_ms.write());
        let raw = RawValue::Map(raw);
        if let Some(index) = index {
            entries[index] = raw;
        } else {
            entries.push(raw);
        }
        validate_blocks(&candidate)?;
        store::save(&self.path, &candidate, MAX_RECORD_BYTES)?;
        self.raw = candidate;
        Ok(())
    }
    pub fn unblock(&mut self, device: DeviceId) -> Result<bool, StoreError> {
        let mut candidate = self.raw.clone();
        let RawValue::Array(entries) = candidate.get_mut(&2).expect("validated entries") else {
            unreachable!()
        };
        let previous = entries.len();
        entries.retain(|entry| parse_block(entry).expect("validated block").device != device);
        if previous == entries.len() {
            return Ok(false);
        }
        store::save(&self.path, &candidate, MAX_RECORD_BYTES)?;
        self.raw = candidate;
        Ok(true)
    }
}

fn text(raw: &RawMap, key: u32) -> Result<Option<String>, StoreError> {
    match raw.get(&key) {
        None | Some(RawValue::Null) => Ok(None),
        Some(RawValue::Text(value)) if value.len() <= 256 => Ok(Some(value.clone())),
        _ => Err(StoreError::Invalid),
    }
}
fn parse_block(value: &RawValue) -> Result<BlockedDevice, StoreError> {
    let raw = value.as_map()?;
    Ok(BlockedDevice {
        device: store::device(store::required(raw, 1)?)?,
        name: text(raw, 2)?,
        os: text(raw, 3)?,
        blocked_at_ms: store::required(raw, 4)?.as_u64()?,
    })
}
fn validate_blocks(raw: &RawMap) -> Result<(), StoreError> {
    let entries = store::entries(raw)?;
    if entries.len() > MAX_BLOCKS {
        return Err(StoreError::LimitExceeded);
    }
    let mut seen = BTreeSet::new();
    for entry in entries {
        if !seen.insert(parse_block(entry)?.device) {
            return Err(StoreError::Invalid);
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct PresenceStore {
    path: PathBuf,
    raw: RawMap,
    dirty: bool,
}
impl PresenceStore {
    pub fn load(data_dir: &Path) -> Result<Self, StoreError> {
        let path = data_dir.join(PRESENCE_FILE);
        let raw = store::read(&path, MAX_RECORD_BYTES)?.unwrap_or_else(store::empty);
        let mut seen = BTreeSet::new();
        for entry in store::entries(&raw)? {
            let (group, device, _) = parse_presence(entry)?;
            if !seen.insert((group, device)) {
                return Err(StoreError::Invalid);
            }
        }
        Ok(Self {
            path,
            raw,
            dirty: false,
        })
    }
    /// Call only for an authenticated logical endpoint, never an address hint.
    pub fn observe(&mut self, group: GroupId, device: DeviceId, at_ms: u64) -> Result<(), StoreError> {
        let mut candidate = self.raw.clone();
        let RawValue::Array(entries) = candidate.get_mut(&2).expect("validated entries") else {
            unreachable!()
        };
        if let Some(entry) = entries.iter_mut().find(|entry| {
            let (g, d, _) = parse_presence(entry).expect("validated presence");
            (g, d) == (group, device)
        }) {
            let RawValue::Map(raw) = entry else { unreachable!() };
            raw.insert(3, at_ms.write());
        } else {
            entries.push(RawValue::Map(
                [
                    (1, RawValue::Bytes(group.0.to_vec())),
                    (2, store::wire_device(device).write()),
                    (3, at_ms.write()),
                ]
                .into(),
            ));
        }
        oink_protocol::v1::cbor::encode_map(&candidate, MAX_RECORD_BYTES)?;
        self.raw = candidate;
        self.dirty = true;
        Ok(())
    }
    pub fn last_connected(&self, group: GroupId, device: DeviceId) -> Option<u64> {
        store::entries(&self.raw)
            .expect("validated entries")
            .iter()
            .find_map(|entry| {
                let (g, d, time) = parse_presence(entry).expect("validated presence");
                ((g, d) == (group, device)).then_some(time)
            })
    }
    /// Flush on offline transition and graceful shutdown. On failure the pending
    /// evidence remains dirty, so the owner can report and retry the same commit.
    pub fn flush(&mut self) -> Result<(), StoreError> {
        if self.dirty {
            store::save(&self.path, &self.raw, MAX_RECORD_BYTES)?;
            self.dirty = false;
        }
        Ok(())
    }
}
fn parse_presence(value: &RawValue) -> Result<(GroupId, DeviceId, u64), StoreError> {
    let raw = value.as_map()?;
    Ok((
        store::group(store::required(raw, 1)?)?,
        store::device(store::required(raw, 2)?)?,
        store::required(raw, 3)?.as_u64()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(n: u32) -> BlockedDevice {
        let mut device = [0; 32];
        device[..4].copy_from_slice(&n.to_le_bytes());
        BlockedDevice {
            device: DeviceId(device),
            name: Some("Device".into()),
            os: Some("macos".into()),
            blocked_at_ms: 100,
        }
    }
    #[test]
    fn blocks_persist_unknown_fields_and_unblock_is_local() {
        let dir = tempfile::tempdir().unwrap();
        let mut blocks = BlockList::load(dir.path()).unwrap();
        blocks.raw.insert(100, RawValue::Uint(u64::MAX));
        blocks.block(entry(1)).unwrap();
        let RawValue::Array(entries) = blocks.raw.get_mut(&2).unwrap() else {
            unreachable!()
        };
        let RawValue::Map(raw) = &mut entries[0] else {
            unreachable!()
        };
        raw.insert(100, RawValue::Text("future".into()));
        let RawValue::Map(id) = raw.get_mut(&1).unwrap() else {
            unreachable!()
        };
        id.insert(100, RawValue::Text("future identity".into()));
        blocks.block(entry(1)).unwrap();
        let mut loaded = BlockList::load(dir.path()).unwrap();
        assert_eq!(loaded.raw, blocks.raw);
        assert_eq!(
            store::entries(&loaded.raw).unwrap()[0].as_map().unwrap()[&1]
                .as_map()
                .unwrap()[&100],
            RawValue::Text("future identity".into())
        );
        assert_eq!(loaded.entries(), [entry(1)]);
        assert!(loaded.unblock(entry(1).device).unwrap());
        assert!(!loaded.unblock(entry(1).device).unwrap());
        assert_eq!(loaded.raw.get(&100), Some(&RawValue::Uint(u64::MAX)));
    }
    #[test]
    fn block_limits_and_corruption_do_not_become_an_empty_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut blocks = BlockList::load(dir.path()).unwrap();
        let entries = (0..1024)
            .map(|n| {
                let e = entry(n);
                RawValue::Map([(1, store::wire_device(e.device).write()), (4, 100u64.write())].into())
            })
            .collect();
        blocks.raw.insert(2, RawValue::Array(entries));
        blocks.block(entry(0)).unwrap();
        assert!(matches!(blocks.block(entry(1024)), Err(StoreError::LimitExceeded)));
        let path = dir.path().join(BLOCKS_FILE);
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(BlockList::load(dir.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt");
        std::fs::write(&path, vec![0; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(matches!(BlockList::load(dir.path()), Err(StoreError::LimitExceeded)));
    }
    #[test]
    fn presence_flush_is_group_scoped_and_failure_keeps_pending_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut presence = PresenceStore::load(dir.path()).unwrap();
        let a = GroupId([1; 16]);
        let b = GroupId([2; 16]);
        let device = entry(1).device;
        assert_eq!(presence.last_connected(a, device), None);
        presence.observe(a, device, 123).unwrap();
        presence.observe(b, device, 456).unwrap();
        presence.flush().unwrap();
        let read = PresenceStore::load(dir.path()).unwrap();
        assert_eq!(read.last_connected(a, device), Some(123));
        assert_eq!(read.last_connected(b, device), Some(456));
        presence.path = dir.path().to_owned();
        presence.observe(a, device, 789).unwrap();
        assert!(presence.flush().is_err());
        assert!(presence.dirty);
    }
}
