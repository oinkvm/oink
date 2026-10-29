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

/// Schema1 retained CBOR: {1:1,2:[historical metadata projections...]}.
/// Only offline labels, endpoint/display metadata and drawing preferences are
/// stored. `get` reconstructs the required DeviceFacts shell in memory; live
/// sequence, Boot nonce, status and link measurements never reach this file.
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
        let mut entries = Vec::new();
        for entry in store::entries(&raw)? {
            let record = cache_record(entry.as_map()?)?;
            let facts = historical_facts(&record)?;
            facts.encode(v1::limits::CONFIG_FRAME)?;
            if !seen.insert(facts_key(&facts)?) {
                return Err(StoreError::Invalid);
            }
            entries.push(RawValue::Map(record));
        }
        let projected: RawMap = [(1, 1u8.write()), (2, RawValue::Array(entries))].into();
        // Apply the same projection on load and observe. An existing full Facts
        // cache is scrubbed once by the next normal flush, with no schema branch.
        let dirty = projected != raw;
        Ok(Self {
            path,
            raw: projected,
            dirty,
        })
    }
    /// Cached facts may populate historical labels, layout preferences and address hints only.
    pub fn get(&self, group: GroupId, device: DeviceId) -> Option<dto::DeviceFacts> {
        store::entries(&self.raw)
            .expect("validated facts")
            .iter()
            .find_map(|raw| {
                let facts = historical_facts(raw.as_map().expect("validated record")).expect("validated facts");
                (facts_key(&facts).expect("validated identity") == (group, device)).then_some(facts)
            })
    }
    /// Call after authenticating the complete live facts. This cache does not
    /// compare Boot freshness or provide the durable audio claim baseline.
    pub fn observe(&mut self, facts: dto::DeviceFacts) -> Result<(), StoreError> {
        facts.encode(v1::limits::CONFIG_FRAME)?;
        let key = facts_key(&facts)?;
        let record = RawValue::Map(cache_record(facts.raw())?);
        let mut candidate = self.raw.clone();
        let RawValue::Array(entries) = candidate.get_mut(&2).expect("validated facts") else {
            unreachable!()
        };
        if let Some(entry) = entries.iter_mut().find(|entry| {
            facts_key(&historical_facts(entry.as_map().expect("validated record")).expect("validated facts"))
                .expect("validated identity")
                == key
        }) {
            if *entry == record {
                return Ok(());
            }
            *entry = record;
        } else {
            entries.push(record);
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
// Keep the disk representation narrower than DeviceFacts. The allowlist follows
// offline readers in agent_api, runtime_state and layout_profiles; authentication,
// claims, permissions and runtime controls use live facts, never this record.
fn subset(raw: &RawMap, keys: &[u32]) -> RawMap {
    raw.iter()
        .filter(|(key, _)| keys.contains(key))
        .map(|(key, value)| (*key, value.clone()))
        .collect()
}
fn cache_record(raw: &RawMap) -> Result<RawMap, StoreError> {
    let boot = store::required(raw, 1)?.as_map()?;
    let device = store::device(store::required(boot, 1)?)?;
    let mut record = subset(raw, &[5, 6, 7, 8, 9]);
    record.insert(1, RawValue::Map([(1, store::wire_device(device).write())].into()));
    for (key, fields) in [(10, &[1, 2, 3][..]), (12, &[1, 2, 3, 5, 6, 7, 8, 10, 11, 12][..])] {
        let RawValue::Array(items) = store::required(raw, key)? else {
            return Err(StoreError::Invalid);
        };
        let items = items
            .iter()
            .map(|item| {
                let mut fields = subset(item.as_map()?, fields);
                if key == 12 {
                    if let Some(RawValue::Map(bounds)) = fields.get_mut(&3) {
                        *bounds = subset(bounds, &[1, 2, 3, 4]);
                    }
                    if let Some(RawValue::Map(extent)) = fields.get_mut(&7) {
                        *extent = subset(extent, &[1, 2]);
                    }
                }
                Ok(RawValue::Map(fields))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        record.insert(key, RawValue::Array(items));
    }
    let mut retained = Vec::new();
    if raw
        .get(&18)
        .is_some_and(|value| !matches!(value, RawValue::Array(_) | RawValue::Null))
    {
        return Err(StoreError::Invalid);
    }
    if let Some(RawValue::Array(items)) = raw.get(&18) {
        for item in items {
            let fields = item.as_map()?;
            let option = dto::LocalOption::read(store::required(fields, 1)?)?;
            if !matches!(
                option,
                dto::LocalOption::ScreenShared
                    | dto::LocalOption::ScreenPreferences
                    | dto::LocalOption::SpeedMode
                    | dto::LocalOption::SyncSystemArrangement
            ) {
                continue;
            }
            let mut preference = subset(fields, &[1, 3]);
            if option == dto::LocalOption::SyncSystemArrangement {
                // Its revision participates in the offline arrangement UI token.
                preference.insert(2, store::required(fields, 2)?.clone());
            }
            if option == dto::LocalOption::ScreenPreferences {
                let screens = dto::ScreenPreferencesValue::read(store::required(fields, 3)?)?;
                let values = screens
                    .screens()?
                    .0
                    .into_iter()
                    .map(|screen| {
                        Ok(RawValue::Map(
                            [
                                (1, screen.display()?.write()),
                                (2, screen.gain()?.write()),
                                (3, screen.enabled()?.write()),
                                (4, screen.remote_gain()?.write()),
                            ]
                            .into(),
                        ))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?;
                preference.insert(
                    3,
                    RawValue::Map([(1, RawValue::Array(values)), (2, screens.device_gain()?.write())].into()),
                );
            }
            retained.push(RawValue::Map(preference));
        }
    }
    if !retained.is_empty() {
        record.insert(18, RawValue::Array(retained));
    }
    // Validate only this projection, with the same required DTO scaffolding
    // supplied to readers. These neutral values are never serialized to disk.
    historical_facts(&record)?;
    Ok(record)
}
fn historical_facts(record: &RawMap) -> Result<dto::DeviceFacts, StoreError> {
    let mut raw = record.clone();
    let mut boot = store::required(record, 1)?.as_map()?.clone();
    boot.insert(2, RawValue::Bytes(vec![0; 16]));
    raw.insert(1, RawValue::Map(boot));
    raw.insert(2, 1u64.write());
    raw.insert(3, RawValue::Map([(1, 1u16.write()), (2, 0u16.write())].into()));
    raw.insert(4, RawValue::from(""));
    raw.insert(
        11,
        RawValue::Map(
            [
                (1, false.write()),
                (2, dto::RunIntent::Stopping.write()),
                (4, 0u64.write()),
            ]
            .into(),
        ),
    );
    let empty = RawValue::Array(Vec::new());
    raw.insert(
        13,
        RawValue::Map(
            [
                (1, empty.clone()),
                (2, dto::CapturePermission::Indeterminate.write()),
                (3, empty.clone()),
                (4, empty.clone()),
            ]
            .into(),
        ),
    );
    raw.insert(
        14,
        RawValue::Map(
            [
                (1, false.write()),
                (2, false.write()),
                (3, 0.0f32.write()),
                (4, false.write()),
                (5, false.write()),
                (6, false.write()),
            ]
            .into(),
        ),
    );
    raw.insert(15, empty);
    for (key, default_key, value) in [(10, 4, 0u32.write()), (12, 9, 1.0f64.write()), (18, 2, 1u64.write())] {
        if let Some(RawValue::Array(items)) = raw.get_mut(&key) {
            for item in items {
                let RawValue::Map(fields) = item else {
                    return Err(StoreError::Invalid);
                };
                fields.entry(default_key).or_insert_with(|| value.clone());
            }
        }
    }
    Ok(dto::DeviceFacts::from_raw(raw)?)
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
    fn cached_metadata_preserves_group_separation_and_failed_flush_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FactsCache::load(dir.path()).unwrap();
        assert!(!cache.path.exists());
        cache.observe(facts(3)).unwrap();
        cache.observe(facts(4)).unwrap();
        cache.flush().unwrap();
        let mut loaded = FactsCache::load(dir.path()).unwrap();
        assert_eq!(loaded.raw, cache.raw);
        assert_eq!(
            loaded.get(GroupId([3; 16]), DeviceId([1; 32])),
            Some(historical_facts(&cache_record(facts(3).raw()).unwrap()).unwrap())
        );
        assert_eq!(
            loaded.get(GroupId([4; 16]), DeviceId([1; 32])),
            Some(historical_facts(&cache_record(facts(4).raw()).unwrap()).unwrap())
        );
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
    #[test]
    #[ignore = "controlled periodic metadata persistence profile"]
    fn resource_metadata_publication() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = FactsCache::load(dir.path()).unwrap();
        cache.observe(facts(3)).unwrap();
        cache.flush().unwrap();
        let profile = std::env::var_os("OINK_RESOURCE_PROFILE").is_some();
        let count = if profile { 1500u64 } else { 100 };
        let began = std::time::Instant::now();
        let mut saves = 0;
        for n in 0..count {
            let mut raw = facts(3).raw().clone();
            raw.insert(2, (n + 2).write());
            cache.observe(dto::DeviceFacts::from_raw(raw).unwrap()).unwrap();
            saves += u64::from(cache.dirty);
            cache.flush().unwrap();
            if profile {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        eprintln!(
            "METADATA,updates={count},saves={saves},us={}",
            began.elapsed().as_micros()
        );
    }

    fn rich_facts() -> dto::DeviceFacts {
        let mut raw = facts(3).raw().clone();
        raw.insert(2, 99u64.write());
        raw.insert(
            10,
            RawValue::Array(vec![RawValue::Map(
                [
                    (1, dto::AddressFamily::Ipv4.write()),
                    (2, RawValue::Bytes(vec![192, 0, 2, 1])),
                    (3, 47470u16.write()),
                    (4, 42u32.write()),
                    (5, 1234u16.write()),
                    (99, true.write()),
                ]
                .into(),
            )]),
        );
        raw.insert(
            12,
            RawValue::Array(vec![RawValue::Map(
                [
                    (1, RawValue::from("screen")),
                    (2, RawValue::from("Display")),
                    (
                        3,
                        RawValue::Map(
                            [
                                (1, 0.0f64.write()),
                                (2, 0.0f64.write()),
                                (3, 800.0f64.write()),
                                (4, 600.0f64.write()),
                            ]
                            .into(),
                        ),
                    ),
                    (5, 1600u32.write()),
                    (6, 1200u32.write()),
                    (8, 0u16.write()),
                    (9, 2.0f64.write()),
                    (10, true.write()),
                    (11, false.write()),
                    (12, false.write()),
                    (99, true.write()),
                ]
                .into(),
            )]),
        );
        raw.insert(
            15,
            RawValue::Array(vec![RawValue::Map(
                [
                    (1, store::wire_device(DeviceId([8; 32])).write()),
                    (2, dto::FactPath::Direct.write()),
                    (4, 100u64.write()),
                    (5, 5000u32.write()),
                    (6, 3u64.write()),
                    (7, 4u64.write()),
                    (8, true.write()),
                ]
                .into(),
            )]),
        );
        let screen_preferences = RawValue::Map(
            [
                (
                    1,
                    RawValue::Array(vec![RawValue::Map(
                        [
                            (1, RawValue::from("screen")),
                            (2, 1.5f64.write()),
                            (3, false.write()),
                            (99, true.write()),
                        ]
                        .into(),
                    )]),
                ),
                (2, 1.2f64.write()),
                (99, true.write()),
            ]
            .into(),
        );
        raw.insert(
            18,
            RawValue::Array(
                [
                    (4u16, true.write()),
                    (6, false.write()),
                    (7, screen_preferences),
                    (8, 1u8.write()),
                    (20, RawValue::Null),
                    (21, true.write()),
                    (99, RawValue::from("unused")),
                ]
                .into_iter()
                .map(|(option, value)| {
                    RawValue::Map([(1, option.write()), (2, 7u64.write()), (3, value), (99, true.write())].into())
                })
                .collect(),
            ),
        );
        raw.insert(16, RawValue::Map(RawMap::new()));
        raw.insert(
            19,
            RawValue::Map(
                [
                    (1, true.write()),
                    (2, dto::PauseReason::LocalUser.write()),
                    (3, 7u64.write()),
                ]
                .into(),
            ),
        );
        let stamp = RawValue::Map([(1, 25u64.write()), (2, store::wire_device(DeviceId([1; 32])).write())].into());
        let claim = RawValue::Map([(1, stamp.clone()), (2, true.write())].into());
        for key in [20, 21, 24] {
            raw.insert(key, claim.clone());
        }
        raw.insert(
            17,
            RawValue::Map(
                [
                    (
                        1,
                        RawValue::Map(
                            [
                                (1, raw[&1].clone()),
                                (3, RawValue::from("speakers")),
                                (4, 1u64.write()),
                                (5, 2u64.write()),
                                (6, stamp),
                            ]
                            .into(),
                        ),
                    ),
                    (2, 50u8.write()),
                    (3, false.write()),
                    (4, dto::ExitVolumeStatus::Ready.write()),
                ]
                .into(),
            ),
        );
        raw.insert(22, RawValue::from("native-fence"));
        raw.insert(23, true.write());
        dto::DeviceFacts::from_raw(raw).unwrap()
    }

    #[test]
    fn cache_persists_only_consumed_metadata_and_reconstructs_offline_views() {
        let dir = tempfile::tempdir().unwrap();
        let live = rich_facts();
        let mut cache = FactsCache::load(dir.path()).unwrap();
        cache.observe(live.clone()).unwrap();
        cache.flush().unwrap();
        let disk = store::read(&cache.path, MAX_FACTS_BYTES).unwrap().unwrap();
        let entry = store::entries(&disk).unwrap()[0].as_map().unwrap();
        assert_eq!(
            entry.keys().copied().collect::<Vec<_>>(),
            [1, 5, 6, 7, 8, 9, 10, 12, 18]
        );
        assert!(
            !entry[&1].as_map().unwrap().contains_key(&2),
            "Boot freshness must not be stored"
        );
        let cached = FactsCache::load(dir.path())
            .unwrap()
            .get(GroupId([3; 16]), DeviceId([1; 32]))
            .unwrap();
        for key in [5, 6, 7, 8, 9] {
            assert_eq!(cached.raw()[&key], live.raw()[&key]);
        }
        assert_eq!(cached.boot().unwrap().nonce().unwrap().0, [0; 16]);
        assert_eq!(cached.fact_sequence(), Ok(1));
        assert!(cached.links().unwrap().0.is_empty());
        assert!(cached.resources().unwrap().is_none());
        assert!(cached.exit_volume().unwrap().is_none());
        assert!(cached.pause().unwrap().is_none());
        assert!(cached.audio_exit_claim().unwrap().is_none());
        assert!(cached.audio_claim_watermark().unwrap().is_none());
        assert!(cached.microphone_claim_watermark().unwrap().is_none());
        assert!(cached.native_arrangement_token().unwrap().is_none());
        assert!(cached.accessibility_granted().unwrap().is_none());
        assert_eq!(
            cached.endpoints().unwrap().0[0].address(),
            live.endpoints().unwrap().0[0].address()
        );
        assert_eq!(cached.endpoints().unwrap().0[0].port(), Ok(47470));
        assert_eq!(
            crate::agent_api::display_fact(&cached.displays().unwrap().0[0]),
            crate::agent_api::display_fact(&live.displays().unwrap().0[0])
        );
        let preferences = cached.preferences().unwrap().unwrap().0;
        assert_eq!(
            preferences
                .iter()
                .map(|p| p.option().unwrap().number())
                .collect::<Vec<_>>(),
            [6, 7, 8, 21]
        );
        assert_eq!(
            preferences.iter().map(|p| p.revision().unwrap()).collect::<Vec<_>>(),
            [1, 1, 1, 7]
        );
        assert_eq!(
            super::super::layout_wire::owner_preferences(&preferences),
            super::super::layout_wire::owner_preferences(&live.preferences().unwrap().unwrap().0)
        );
        assert!(preferences.iter().all(|p| !p.raw().contains_key(&99)));
    }

    #[test]
    fn live_fields_unused_extensions_and_unconsumed_revisions_do_not_dirty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let live = rich_facts();
        let mut cache = FactsCache::load(dir.path()).unwrap();
        cache.observe(live.clone()).unwrap();
        cache.flush().unwrap();
        let bytes = std::fs::read(&cache.path).unwrap();
        let mut raw = live.raw().clone();
        let RawValue::Map(boot) = raw.get_mut(&1).unwrap() else {
            unreachable!()
        };
        boot.insert(2, RawValue::Bytes(vec![77; 16]));
        raw.insert(2, 1000u64.write());
        raw.insert(3, RawValue::Map([(1, 2u16.write()), (2, 9u16.write())].into()));
        raw.insert(4, RawValue::from("another-build"));
        raw.insert(15, RawValue::Array(Vec::new()));
        for key in [16, 17, 19, 20, 21, 22, 23, 24] {
            raw.remove(&key);
        }
        raw.insert(99, RawValue::from("unused change"));
        for key in [11, 13, 14] {
            raw.insert(key, facts(4).raw()[&key].clone());
        }
        if let Some(RawValue::Array(items)) = raw.get_mut(&18) {
            for item in items {
                let RawValue::Map(item) = item else { unreachable!() };
                if item[&1] != 21u16.write() {
                    item.insert(2, 999u64.write());
                }
                item.insert(99, false.write());
                if item[&1] == 4u16.write() {
                    item.insert(3, false.write());
                }
            }
        }
        cache.observe(dto::DeviceFacts::from_raw(raw).unwrap()).unwrap();
        assert!(!cache.dirty);
        cache.flush().unwrap();
        assert_eq!(std::fs::read(&cache.path).unwrap(), bytes);
        let mut renamed = live.raw().clone();
        renamed.insert(6, RawValue::from("Renamed"));
        cache.observe(dto::DeviceFacts::from_raw(renamed).unwrap()).unwrap();
        assert!(cache.dirty);
        cache.flush().unwrap();
        assert_eq!(
            FactsCache::load(dir.path())
                .unwrap()
                .get(GroupId([3; 16]), DeviceId([1; 32]))
                .unwrap()
                .name()
                .unwrap()
                .0,
            "Renamed"
        );
    }

    #[test]
    fn loading_full_records_scrubs_live_fields_once_without_a_schema_change() {
        let dir = tempfile::tempdir().unwrap();
        let live = rich_facts();
        let path = dir.path().join(FACTS_FILE);
        store::save(
            &path,
            &[
                (1, 1u8.write()),
                (2, RawValue::Array(vec![live.write()])),
                (99, true.write()),
            ]
            .into(),
            MAX_FACTS_BYTES,
        )
        .unwrap();
        let mut cache = FactsCache::load(dir.path()).unwrap();
        assert!(cache.dirty);
        cache.flush().unwrap();
        let loaded = FactsCache::load(dir.path()).unwrap();
        assert!(!loaded.dirty);
        assert_eq!(loaded.raw, cache.raw);
        assert!(!loaded.raw.contains_key(&99));
        let entry = store::entries(&loaded.raw).unwrap()[0].as_map().unwrap();
        assert!(!entry.contains_key(&2) && !entry.contains_key(&15));
        assert_eq!(
            loaded.get(GroupId([3; 16]), DeviceId([1; 32])).unwrap().name(),
            live.name()
        );
    }
}
