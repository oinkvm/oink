//! Durable local pairing block list. A block is local policy only: it never
//! changes the signed roster, and a blocked device can still be found by an
//! explicit manual address query. Incoming pairing requests from a blocked
//! device are declined, pending ones are dismissed, and nearby rows hide it.

use super::*;
use std::collections::BTreeMap;

/// File next to the pairing journal in the mobile data directory.
pub(super) const FILE: &str = "pairing-blocks.json";
pub const MAX_BLOCKS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedDevice {
    pub device: DeviceId,
    pub name: Option<String>,
    pub os: Option<String>,
    pub blocked_at_ms: u64,
}

/// On-disk entry, keyed by the device's hex ID.
#[derive(serde::Serialize, serde::Deserialize)]
struct Stored {
    name: Option<String>,
    os: Option<String>,
    blocked_at_ms: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("this device cannot be blocked")]
    SelfDevice,
    #[error("block list limit reached")]
    LimitExceeded,
    #[error("block list cannot be persisted: {0}")]
    Persistence(String),
}

#[derive(Debug)]
pub(super) struct BlockList {
    path: PathBuf,
    entries: BTreeMap<DeviceId, BlockedDevice>,
    pub revision: u64,
}

impl BlockList {
    /// A missing file is an empty list. A corrupt file is a start failure:
    /// silently dropping the user's blocks would re-admit those devices.
    pub(super) fn load(path: PathBuf) -> Result<Self, MobileNetworkError> {
        let entries = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<BTreeMap<String, Stored>>(&bytes)
                .map_err(|error| MobileNetworkError::Start(format!("pairing block list: {error}")))?
                .into_iter()
                .map(|(key, entry)| {
                    let device = parse_device(&key).map_err(MobileNetworkError::Start)?;
                    Ok((
                        device,
                        BlockedDevice {
                            device,
                            name: entry.name,
                            os: entry.os,
                            blocked_at_ms: entry.blocked_at_ms,
                        },
                    ))
                })
                .collect::<Result<_, MobileNetworkError>>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(MobileNetworkError::Start(format!("pairing block list: {error}"))),
        };
        Ok(Self {
            path,
            entries,
            revision: 1,
        })
    }

    pub(super) fn contains(&self, device: DeviceId) -> bool {
        self.entries.contains_key(&device)
    }

    pub(super) fn entries(&self) -> Vec<BlockedDevice> {
        self.entries.values().cloned().collect()
    }

    /// Persists first, then applies. Returns whether the list changed.
    pub(super) fn edit(&mut self, device: DeviceId, entry: Option<BlockedDevice>) -> Result<bool, BlockError> {
        let mut next = self.entries.clone();
        let changed = match entry {
            Some(entry) => {
                if !next.contains_key(&device) && next.len() >= MAX_BLOCKS {
                    return Err(BlockError::LimitExceeded);
                }
                next.insert(device, entry).is_none()
            }
            None => next.remove(&device).is_some(),
        };
        if !changed {
            return Ok(false);
        }
        let encoded: BTreeMap<String, Stored> = next
            .iter()
            .map(|(device, entry)| {
                (
                    device.to_hex(),
                    Stored {
                        name: entry.name.clone(),
                        os: entry.os.clone(),
                        blocked_at_ms: entry.blocked_at_ms,
                    },
                )
            })
            .collect();
        let bytes = serde_json::to_vec(&encoded).map_err(|error| BlockError::Persistence(error.to_string()))?;
        oink_foundation::storage::atomic_write(&self.path, &bytes)
            .map_err(|error| BlockError::Persistence(error.to_string()))?;
        self.entries = next;
        self.revision += 1;
        Ok(true)
    }
}

impl MobileNetwork {
    pub fn blocked_devices(&self) -> (u64, Vec<BlockedDevice>) {
        let blocks = self.state.blocks.lock().expect("block list mutex poisoned");
        (blocks.revision, blocks.entries())
    }

    pub fn is_blocked(&self, device: DeviceId) -> bool {
        self.state
            .blocks
            .lock()
            .expect("block list mutex poisoned")
            .contains(device)
    }

    /// Install a durable block, then dismiss that device's pending incoming
    /// requests exactly as the desktop owner does (the initiator sees Declined).
    pub fn block(&self, device: DeviceId, name: Option<String>, os: Option<String>) -> Result<bool, BlockError> {
        if device == self.boot().device_id {
            return Err(BlockError::SelfDevice);
        }
        let changed = self.state.blocks.lock().expect("block list mutex poisoned").edit(
            device,
            Some(BlockedDevice {
                device,
                name,
                os,
                blocked_at_ms: unix_ms(),
            }),
        )?;
        if changed {
            let _ = super::member_host::pairing(&self.state, |pairs| Ok(pairs.block_pending(device)));
        }
        Ok(changed)
    }

    pub fn unblock(&self, device: DeviceId) -> Result<bool, BlockError> {
        self.state
            .blocks
            .lock()
            .expect("block list mutex poisoned")
            .edit(device, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_list_round_trips_and_rejects_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let device = DeviceId([7; 32]);
        let mut list = BlockList::load(path.clone()).unwrap();
        assert!(!list.contains(device));
        let entry = BlockedDevice {
            device,
            name: Some("Windows PC".into()),
            os: Some("windows".into()),
            blocked_at_ms: 5,
        };
        assert!(list.edit(device, Some(entry.clone())).unwrap());
        assert!(!list.edit(device, Some(entry.clone())).unwrap());
        let reloaded = BlockList::load(path.clone()).unwrap();
        assert_eq!(reloaded.entries(), vec![entry]);
        let mut reloaded = reloaded;
        assert!(reloaded.edit(device, None).unwrap());
        assert!(BlockList::load(path.clone()).unwrap().entries().is_empty());
        std::fs::write(&path, b"{").unwrap();
        assert!(BlockList::load(path).is_err());
    }
}
