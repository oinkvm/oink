//! Retained address hints and display-only facts. Neither store grants current
//! membership, Boot freshness, claim authority or permission to send input.
use crate::config::store::{self, StoreError};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::BTreeSet,
    io::Read,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

pub const ADDRESSES_FILE: &str = "addresses.json";
pub const FACTS_FILE: &str = "metadata-cache.cbor";
const MAX_ADDRESSES_BYTES: usize = 1024 * 1024;
const MAX_FACTS_BYTES: usize = 16 * 1024 * 1024;

/// Schema1 JSON: {schema:1,entries:[{device:{algorithm,public_key},ipv4,port}]}.
/// Newest remembered hints come first. Unknown envelope and entry keys survive.
#[derive(Clone, Debug)]
pub struct AddressStore {
    path: PathBuf,
    raw: serde_json::Value,
}
impl AddressStore {
    pub fn load(data_dir: &Path) -> Result<Self, StoreError> {
        let path = data_dir.join(ADDRESSES_FILE);
        let raw = match std::fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_ADDRESSES_BYTES as u64 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > MAX_ADDRESSES_BYTES {
                    return Err(StoreError::LimitExceeded);
                }
                serde_json::from_slice(&bytes).map_err(|_| StoreError::Invalid)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({"schema":1,"entries":[]}),
            Err(error) => return Err(error.into()),
        };
        validate_addresses(&raw)?;
        Ok(Self { path, raw })
    }
    pub fn candidates(&self, device: DeviceId) -> Vec<SocketAddr> {
        address_entries(&self.raw)
            .expect("validated addresses")
            .iter()
            .filter_map(|entry| {
                let (id, address) = parse_address(entry).expect("validated address");
                (id == device).then_some(address)
            })
            .collect()
    }
    /// The caller has verified the TLS identity or explicitly pinned the member
    /// identity. A failed save does not publish the candidate hint in memory.
    pub fn remember(&mut self, device: DeviceId, address: SocketAddr) -> Result<bool, StoreError> {
        let SocketAddr::V4(address) = address else {
            return Err(StoreError::UnsupportedValue);
        };
        if address.port() == 0 {
            return Err(StoreError::Invalid);
        }
        let mut candidate = self.raw.clone();
        let entries = candidate
            .get_mut("entries")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or(StoreError::Invalid)?;
        let index = entries
            .iter()
            .position(|entry| parse_address(entry).is_ok_and(|value| value == (device, address.into())));
        let entry = match index {
            Some(0) => return Ok(false),
            Some(index) => entries.remove(index),
            None => {
                serde_json::json!({"device":{"algorithm":"ed25519","public_key":device.to_hex()},"ipv4":address.ip().to_string(),"port":address.port()})
            }
        };
        entries.insert(0, entry);
        let bytes = serde_json::to_vec_pretty(&candidate).map_err(|_| StoreError::Invalid)?;
        if bytes.len() > MAX_ADDRESSES_BYTES {
            return Err(StoreError::LimitExceeded);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        oink_foundation::storage::atomic_write(&self.path, &bytes)?;
        self.raw = candidate;
        Ok(true)
    }
}
fn address_entries(raw: &serde_json::Value) -> Result<&Vec<serde_json::Value>, StoreError> {
    raw.get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or(StoreError::Invalid)
}
fn parse_address(raw: &serde_json::Value) -> Result<(DeviceId, SocketAddr), StoreError> {
    let device = raw.get("device").ok_or(StoreError::Invalid)?;
    if device.get("algorithm").and_then(serde_json::Value::as_str) != Some("ed25519") {
        return Err(StoreError::IdentityMismatch);
    }
    let public_key = device
        .get("public_key")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::Invalid)?;
    if public_key.len() != 64
        || !public_key
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(StoreError::Invalid);
    }
    let mut key = [0; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&public_key[index * 2..index * 2 + 2], 16).map_err(|_| StoreError::Invalid)?;
    }
    let ip: Ipv4Addr = raw
        .get("ipv4")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::Invalid)?
        .parse()
        .map_err(|_| StoreError::Invalid)?;
    let port = raw
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or(StoreError::Invalid)?;
    Ok((DeviceId(key), SocketAddr::from((ip, port))))
}
fn validate_addresses(raw: &serde_json::Value) -> Result<(), StoreError> {
    if raw.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(StoreError::UnsupportedSchema);
    }
    let mut seen = BTreeSet::new();
    for entry in address_entries(raw)? {
        if !seen.insert(parse_address(entry)?) {
            return Err(StoreError::Invalid);
        }
    }
    Ok(())
}

/// Schema1 retained CBOR: {1:1,2:[DeviceFacts...]}. Entries are keyed by group
/// and complete device identity; every nested fact extension is stored as raw.
#[derive(Clone, Debug)]
pub struct FactsCache {
    path: PathBuf,
    raw: RawMap,
    dirty: bool,
}
impl FactsCache {
    pub fn load(data_dir: &Path) -> Result<Self, StoreError> {
        let path = data_dir.join(FACTS_FILE);
        let raw = store::read(&path, MAX_FACTS_BYTES)?.unwrap_or_else(store::empty);
        let mut seen = BTreeSet::new();
        for entry in store::entries(&raw)? {
            let facts = dto::DeviceFacts::read(entry)?;
            facts.encode(v1::limits::CONFIG_FRAME)?;
            if !seen.insert(facts_key(&facts)?) {
                return Err(StoreError::Invalid);
            }
        }
        Ok(Self {
            path,
            raw,
            dirty: false,
        })
    }
    /// Cached facts may populate historical labels, layout preferences and address hints only.
    pub fn get(&self, group: GroupId, device: DeviceId) -> Option<dto::DeviceFacts> {
        store::entries(&self.raw)
            .expect("validated facts")
            .iter()
            .find_map(|raw| {
                let facts = dto::DeviceFacts::read(raw).expect("validated facts");
                (facts_key(&facts).expect("validated identity") == (group, device)).then_some(facts)
            })
    }
    /// Call after authenticating the complete live facts. This cache does not
    /// compare Boot freshness or provide the durable audio claim baseline.
    pub fn observe(&mut self, facts: dto::DeviceFacts) -> Result<(), StoreError> {
        facts.encode(v1::limits::CONFIG_FRAME)?;
        let key = facts_key(&facts)?;
        let mut candidate = self.raw.clone();
        let RawValue::Array(entries) = candidate.get_mut(&2).expect("validated facts") else {
            unreachable!()
        };
        if let Some(entry) = entries.iter_mut().find(|entry| {
            facts_key(&dto::DeviceFacts::read(entry).expect("validated facts")).expect("validated identity") == key
        }) {
            if *entry == facts.write() {
                return Ok(());
            }
            *entry = facts.write();
        } else {
            entries.push(facts.write());
        }
        v1::cbor::encode_map(&candidate, MAX_FACTS_BYTES)?;
        self.raw = candidate;
        self.dirty = true;
        Ok(())
    }
    /// Failed persistence leaves the pending cache dirty for retry.
    pub fn flush(&mut self) -> Result<(), StoreError> {
        if self.dirty {
            store::save(&self.path, &self.raw, MAX_FACTS_BYTES)?;
            self.dirty = false;
        }
        Ok(())
    }
}
fn facts_key(facts: &dto::DeviceFacts) -> Result<(GroupId, DeviceId), StoreError> {
    Ok((
        GroupId(facts.group()?.0),
        store::device(&facts.boot()?.device_id()?.write())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn facts(group: u8) -> dto::DeviceFacts {
        let empty = RawValue::Array(vec![]);
        dto::DeviceFacts::from_raw(
            [
                (
                    1,
                    RawValue::Map(
                        [
                            (1, store::wire_device(DeviceId([1; 32])).write()),
                            (2, RawValue::Bytes(vec![2; 16])),
                        ]
                        .into(),
                    ),
                ),
                (2, 1u64.write()),
                (3, RawValue::Map([(1, 1u16.write()), (2, 0u16.write())].into())),
                (4, RawValue::Text("test".into())),
                (5, RawValue::Bytes(vec![group; 16])),
                (6, RawValue::Text("Device".into())),
                (7, RawValue::Text("test".into())),
                (8, RawValue::Text("test".into())),
                (9, RawValue::Text("test".into())),
                (10, empty.clone()),
                (
                    11,
                    RawValue::Map(
                        [
                            (1, true.write()),
                            (2, 0u8.write()),
                            (4, 0u64.write()),
                            (99, true.write()),
                        ]
                        .into(),
                    ),
                ),
                (12, empty.clone()),
                (
                    13,
                    RawValue::Map(
                        [
                            (1, empty.clone()),
                            (2, 0u8.write()),
                            (3, empty.clone()),
                            (4, empty.clone()),
                        ]
                        .into(),
                    ),
                ),
                (
                    14,
                    RawValue::Map(
                        [
                            (1, true.write()),
                            (2, false.write()),
                            (3, 1.0f32.write()),
                            (4, false.write()),
                            (5, false.write()),
                            (6, false.write()),
                        ]
                        .into(),
                    ),
                ),
                (15, empty),
                (99, RawValue::Bytes(vec![7, 8, 9])),
            ]
            .into(),
        )
        .unwrap()
    }
    #[test]
    fn address_hints_retain_unknown_fields_and_commit_before_publish() {
        let dir = tempfile::tempdir().unwrap();
        let peer = DeviceId([1; 32]);
        let a: SocketAddr = "192.0.2.1:47470".parse().unwrap();
        let b: SocketAddr = "192.0.2.2:47470".parse().unwrap();
        let mut addresses = AddressStore::load(dir.path()).unwrap();
        assert!(!addresses.path.exists());
        assert!(addresses.remember(peer, a).unwrap());
        addresses.raw["future"] = serde_json::json!({"value":true});
        addresses.raw["entries"][0]["device"]["future"] = true.into();
        addresses.raw["entries"][0]["future"] = true.into();
        addresses.remember(peer, b).unwrap();
        addresses.remember(peer, a).unwrap();
        let loaded = AddressStore::load(dir.path()).unwrap();
        assert_eq!(loaded.raw, addresses.raw);
        assert_eq!(loaded.candidates(peer), [a, b]);
        assert_eq!(loaded.raw["entries"][0]["device"]["future"], true);
        assert!(!addresses.remember(peer, a).unwrap());
        let before = addresses.raw.clone();
        addresses.path = dir.path().join("blocked");
        std::fs::create_dir(&addresses.path).unwrap();
        assert!(addresses.remember(peer, b).is_err());
        assert_eq!(addresses.raw, before);
        assert!(addresses.remember(peer, "[::1]:47470".parse().unwrap()).is_err());
    }
    #[test]
    fn cached_facts_preserve_raw_nested_fields_and_group_separation() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FactsCache::load(dir.path()).unwrap();
        assert!(!cache.path.exists());
        cache.raw.insert(99, true.write());
        cache.observe(facts(3)).unwrap();
        cache.observe(facts(4)).unwrap();
        cache.flush().unwrap();
        let mut loaded = FactsCache::load(dir.path()).unwrap();
        assert_eq!(loaded.raw, cache.raw);
        assert_eq!(loaded.get(GroupId([3; 16]), DeviceId([1; 32])), Some(facts(3)));
        assert_eq!(loaded.get(GroupId([4; 16]), DeviceId([1; 32])), Some(facts(4)));
        assert!(loaded.get(GroupId([5; 16]), DeviceId([1; 32])).is_none());
        loaded.path = dir.path().join("blocked");
        std::fs::create_dir(&loaded.path).unwrap();
        loaded.observe(facts(5)).unwrap();
        assert!(loaded.flush().is_err());
        assert!(loaded.dirty);
        loaded.path = dir.path().join("recovered.cbor");
        loaded.flush().unwrap();
        assert!(!loaded.dirty);
    }
    #[test]
    fn unsupported_local_envelopes_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let address = dir.path().join(ADDRESSES_FILE);
        let old = b"{\"entries\":[]}";
        std::fs::write(&address, old).unwrap();
        assert!(matches!(
            AddressStore::load(dir.path()),
            Err(StoreError::UnsupportedSchema)
        ));
        assert_eq!(std::fs::read(&address).unwrap(), old);
        let path = dir.path().join(FACTS_FILE);
        let future = v1::cbor::encode_map(&[(1, 2u8.write()), (2, RawValue::Array(vec![]))].into(), 1024).unwrap();
        std::fs::write(&path, &future).unwrap();
        assert!(matches!(
            FactsCache::load(dir.path()),
            Err(StoreError::UnsupportedSchema)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), future);
    }
}
