//! Retained address hints and display-only facts. Neither store grants current
//! membership, Boot freshness, audio route authority or permission to send input.
use crate::config::store::{self, StoreError};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v2::{
    self, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

pub const ADDRESSES_FILE: &str = "addresses.json";
pub const FACTS_FILE: &str = "metadata-cache.cbor";
const MAX_ADDRESSES_BYTES: usize = 1024 * 1024;
const MAX_FACTS_BYTES: usize = 16 * 1024 * 1024;
/// IPv4 addresses remembered per device (user 2026-11-12). A phone opens a
/// new ephemeral port on every endpoint start and moves between networks;
/// without a bound every port it ever used stayed a dial candidate and the
/// first round spent all dial slots on stale ones (a Mac test machine, 2026-11-12).
pub const MAX_IPS_PER_DEVICE: usize = 8;

/// Schema1 JSON: {schema:1,entries:[{device:{algorithm,public_key},ipv4,port}]}.
/// Newest remembered hints come first. Unknown envelope and entry keys survive.
/// One entry per (device, IPv4) carrying the newest port, at most
/// [`MAX_IPS_PER_DEVICE`] per device: the least recently remembered IP goes
/// first. Applied on every record and on load (an over-full file is
/// rewritten trimmed).
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
        let mut raw = raw;
        let mut store_trimmed = false;
        if let Some(entries) = raw.get_mut("entries").and_then(serde_json::Value::as_array_mut) {
            store_trimmed = trim_addresses(entries);
        }
        let store = Self { path, raw };
        if store_trimmed {
            // Same trimming as every record; written back at once so the
            // file shows what is dialed. Memory is already trimmed: a failed
            // write leaves the next successful record to persist it.
            match serde_json::to_vec_pretty(&store.raw) {
                Ok(bytes) => {
                    if let Err(error) = oink_foundation::storage::atomic_write(&store.path, &bytes) {
                        tracing::warn!(%error, "trimmed address book could not be written back");
                    }
                }
                Err(error) => tracing::warn!(%error, "trimmed address book could not be encoded"),
            }
        }
        Ok(store)
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
        let index = same_ip(entries, device, address);
        if index == Some(0) && parse_address(&entries[0]).is_ok_and(|(_, known)| known.port() == address.port()) {
            return Ok(false);
        }
        let entry = take_or_new(entries, index, device, address)?;
        entries.insert(0, entry);
        trim_addresses(entries);
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
/// Route pins (route-selection design 5.5), stored in the same schema-1 file:
/// `"pinned": true` on the address entry this device dials, or the envelope's
/// `"routes": [{device, keep_incoming: "<this device's IPv4>"}]` when the peer
/// dials. At most one per peer.
impl AddressStore {
    pub fn route_record(&self, device: DeviceId) -> Option<oink_network::link::RouteRecord> {
        let entries = address_entries(&self.raw).expect("validated addresses");
        let pinned = entries.iter().find_map(|entry| {
            let (id, address) = parse_address(entry).ok()?;
            (id == device && entry.get("pinned").and_then(serde_json::Value::as_bool) == Some(true)).then_some(address)
        });
        if let Some(SocketAddr::V4(address)) = pinned {
            return Some(oink_network::link::RouteRecord::Dial(*address.ip()));
        }
        self.raw
            .get("routes")
            .and_then(serde_json::Value::as_array)?
            .iter()
            .find_map(|route| {
                (route_device(route)? == device)
                    .then(|| route.get("keep_incoming")?.as_str()?.parse::<Ipv4Addr>().ok())
                    .flatten()
            })
            .map(oink_network::link::RouteRecord::Incoming)
    }
    /// Only the saved manual endpoint joins the published connected-route
    /// probes. Ordinary historical hints remain exclusive to initial dialing.
    pub fn route_candidates(&self, device: DeviceId, mut published: Vec<SocketAddr>) -> Vec<SocketAddr> {
        if let Some(oink_network::link::RouteRecord::Dial(ip)) = self.route_record(device)
            && !published.iter().any(|a| a.ip() == ip)
            && let Some(address) = self.candidates(device).into_iter().find(|a| a.ip() == ip)
        {
            published.push(address);
        }
        published
    }
    /// Replace this peer's pin. `Dial` needs its target endpoint, from a route
    /// row or authenticated peer facts, even when dialing has not succeeded.
    /// It becomes the newest dial hint too.
    pub fn set_route_record(
        &mut self,
        device: DeviceId,
        record: Option<oink_network::link::RouteRecord>,
        dialed: Option<SocketAddr>,
    ) -> Result<(), StoreError> {
        let mut candidate = self.raw.clone();
        let entries = candidate
            .get_mut("entries")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or(StoreError::Invalid)?;
        for entry in entries.iter_mut() {
            if parse_address(entry).is_ok_and(|(id, _)| id == device)
                && let Some(map) = entry.as_object_mut()
            {
                map.remove("pinned");
            }
        }
        if let Some(object) = candidate.as_object_mut()
            && let Some(routes) = object.get_mut("routes").and_then(serde_json::Value::as_array_mut)
        {
            routes.retain(|route| route_device(route) != Some(device));
        }
        match record {
            None => {}
            Some(oink_network::link::RouteRecord::Dial(ip)) => {
                let Some(SocketAddr::V4(address)) = dialed.filter(|a| a.ip() == std::net::IpAddr::V4(ip)) else {
                    return Err(StoreError::Invalid);
                };
                let entries = candidate
                    .get_mut("entries")
                    .and_then(serde_json::Value::as_array_mut)
                    .ok_or(StoreError::Invalid)?;
                let index = same_ip(entries, device, address);
                let mut entry = take_or_new(entries, index, device, address)?;
                entry
                    .as_object_mut()
                    .ok_or(StoreError::Invalid)?
                    .insert("pinned".into(), serde_json::Value::Bool(true));
                entries.insert(0, entry);
                trim_addresses(entries);
            }
            Some(oink_network::link::RouteRecord::Incoming(ip)) => {
                let object = candidate.as_object_mut().ok_or(StoreError::Invalid)?;
                let routes = object
                    .entry("routes")
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or(StoreError::Invalid)?;
                routes.push(serde_json::json!({
                    "device":{"algorithm":"ed25519","public_key":device.to_hex()},
                    "keep_incoming": ip.to_string(),
                }));
            }
        }
        if candidate == self.raw {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&candidate).map_err(|_| StoreError::Invalid)?;
        if bytes.len() > MAX_ADDRESSES_BYTES {
            return Err(StoreError::LimitExceeded);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        oink_foundation::storage::atomic_write(&self.path, &bytes)?;
        self.raw = candidate;
        Ok(())
    }
}
/// Index of this device's entry for `address`'s IPv4, whatever its port.
fn same_ip(entries: &[serde_json::Value], device: DeviceId, address: std::net::SocketAddrV4) -> Option<usize> {
    entries.iter().position(|entry| {
        parse_address(entry).is_ok_and(|(id, known)| id == device && known.ip() == IpAddr::V4(*address.ip()))
    })
}
/// Removes the entry at `index` and gives it `address`'s port (unknown keys
/// and a pin survive), or makes a new entry.
fn take_or_new(
    entries: &mut Vec<serde_json::Value>,
    index: Option<usize>,
    device: DeviceId,
    address: std::net::SocketAddrV4,
) -> Result<serde_json::Value, StoreError> {
    Ok(match index {
        Some(index) => {
            let mut entry = entries.remove(index);
            entry
                .as_object_mut()
                .ok_or(StoreError::Invalid)?
                .insert("port".into(), address.port().into());
            entry
        }
        None => {
            serde_json::json!({"device":{"algorithm":"ed25519","public_key":device.to_hex()},"ipv4":address.ip().to_string(),"port":address.port()})
        }
    })
}
fn pinned(entry: &serde_json::Value) -> bool {
    entry.get("pinned").and_then(serde_json::Value::as_bool) == Some(true)
}
/// Entries are newest first. Keeps the newest entry of every (device, IPv4)
/// and the [`MAX_IPS_PER_DEVICE`] most recent IPs of every device; true when
/// something was dropped. A route pin (one per peer) moves to the kept entry
/// of its IP and is never dropped: it is the address the user chose.
fn trim_addresses(entries: &mut Vec<serde_json::Value>) -> bool {
    let before = entries.len();
    let mut kept: Vec<(DeviceId, IpAddr, serde_json::Value)> = Vec::with_capacity(before);
    for entry in entries.drain(..) {
        let (device, address) = parse_address(&entry).expect("validated address");
        if let Some((_, _, newer)) = kept.iter_mut().find(|(id, ip, _)| *id == device && *ip == address.ip()) {
            if pinned(&entry)
                && let Some(map) = newer.as_object_mut()
            {
                map.insert("pinned".into(), serde_json::Value::Bool(true));
            }
            continue;
        }
        kept.push((device, address.ip(), entry));
    }
    let mut per_device: BTreeMap<DeviceId, usize> = BTreeMap::new();
    for (device, _, _) in &kept {
        *per_device.entry(*device).or_default() += 1;
    }
    // Oldest first: drop least recently used IPs until each device fits.
    let mut index = kept.len();
    while index > 0 {
        index -= 1;
        let (device, _, entry) = &kept[index];
        let count = per_device.get_mut(device).expect("counted device");
        if *count > MAX_IPS_PER_DEVICE && !pinned(entry) {
            *count -= 1;
            kept.remove(index);
        }
    }
    entries.extend(kept.into_iter().map(|(_, _, entry)| entry));
    entries.len() != before
}
fn route_device(route: &serde_json::Value) -> Option<DeviceId> {
    let device = route.get("device")?;
    if device.get("algorithm")?.as_str()? != "ed25519" {
        return None;
    }
    let key = device.get("public_key")?.as_str()?;
    if key.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(key.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(DeviceId(bytes))
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
            facts.encode(v2::limits::CONFIG_FRAME)?;
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
    /// compare Boot freshness or carry the group audio route setting.
    pub fn observe(&mut self, facts: dto::DeviceFacts) -> Result<(), StoreError> {
        facts.encode(v2::limits::CONFIG_FRAME)?;
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
        v2::cbor::encode_map(&candidate, MAX_FACTS_BYTES)?;
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
// offline readers in agent_api, runtime_state, layout_profiles and device_cards; authentication,
// the audio configuration, permissions and runtime controls use live facts, never this record.
fn subset(raw: &RawMap, keys: &[u32]) -> RawMap {
    raw.iter()
        .filter(|(key, _)| keys.contains(key))
        .map(|(key, value)| (*key, value.clone()))
        .collect()
}
fn cache_record(raw: &RawMap) -> Result<RawMap, StoreError> {
    let boot = store::required(raw, 1)?.as_map()?;
    let device = store::device(store::required(boot, 1)?)?;
    // 31 (form_factor) feeds the iPhone/iPad badge of offline device cards.
    // 28..=30 are retired DeviceFacts keys (protocol-v2 registry); not stored.
    let mut record = subset(raw, &[5, 6, 7, 8, 9, 25, 26, 27, 31]);
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
                    | dto::LocalOption::DisableHiddenDisplays
            ) {
                continue;
            }
            let mut preference = subset(fields, &[1, 3]);
            if matches!(
                option,
                dto::LocalOption::SyncSystemArrangement | dto::LocalOption::DisableHiddenDisplays
            ) {
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
    raw.entry(25).or_insert_with(|| dto::DeviceKind::Desktop.write());
    raw.entry(26).or_insert_with(|| v2::MAJOR.write());
    raw.entry(27).or_insert_with(|| v2::MINOR.write());
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
                (25, dto::DeviceKind::Desktop.write()),
                (26, v2::MAJOR.write()),
                (27, v2::MINOR.write()),
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
    fn one_entry_per_ip_keeps_the_newest_port() {
        let dir = tempfile::tempdir().unwrap();
        let phone = DeviceId([3; 32]);
        let other = DeviceId([4; 32]);
        let pc: SocketAddr = "192.0.2.234:47470".parse().unwrap();
        let mut addresses = AddressStore::load(dir.path()).unwrap();
        addresses.remember(other, pc).unwrap();
        for port in 50_000..50_012 {
            assert!(
                addresses
                    .remember(phone, SocketAddr::from(([192, 168, 8, 232], port)))
                    .unwrap()
            );
        }
        let newest = SocketAddr::from(([192, 168, 8, 232], 50_011));
        assert_eq!(addresses.candidates(phone), [newest]);
        assert!(!addresses.remember(phone, newest).unwrap(), "already the newest entry");
        let loaded = AddressStore::load(dir.path()).unwrap();
        assert_eq!(loaded.candidates(phone), [newest]);
        assert_eq!(loaded.candidates(other), [pc]);
        assert_eq!(loaded.raw["entries"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn at_most_eight_ips_per_device_least_recently_used_dropped_first() {
        let dir = tempfile::tempdir().unwrap();
        let peer = DeviceId([5; 32]);
        let other = DeviceId([6; 32]);
        let ip = |n: u8| SocketAddr::from(([10, 0, 0, n], 47470));
        let mut addresses = AddressStore::load(dir.path()).unwrap();
        for n in 1..=4 {
            addresses.remember(other, ip(n)).unwrap();
        }
        for n in 1..=7 {
            addresses.remember(peer, ip(n)).unwrap();
        }
        // .1 is used again: it becomes the most recent and survives.
        addresses.remember(peer, ip(1)).unwrap();
        for n in 8..=10 {
            addresses.remember(peer, ip(n)).unwrap();
        }
        // Newest first; .2 and .3 were least recently used.
        let expected: Vec<_> = [10, 9, 8, 1, 7, 6, 5, 4].into_iter().map(ip).collect();
        assert_eq!(addresses.candidates(peer), expected);
        assert_eq!(MAX_IPS_PER_DEVICE, 8);
        // Other devices keep their own entries and order.
        let others: Vec<_> = [4, 3, 2, 1].into_iter().map(ip).collect();
        assert_eq!(addresses.candidates(other), others);
        let loaded = AddressStore::load(dir.path()).unwrap();
        assert_eq!(loaded.candidates(peer), expected);
        assert_eq!(loaded.candidates(other), others);
    }
    #[test]
    fn an_over_full_file_is_trimmed_on_load_and_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let peer = DeviceId([7; 32]);
        let entry = |ip: &str, port: u16| serde_json::json!({"device":{"algorithm":"ed25519","public_key":peer.to_hex()},"ipv4":ip,"port":port});
        // Newest first, as written by earlier builds: three ports on .232
        // (the middle one pinned) and ten other IPs.
        let mut entries = vec![
            entry("192.0.2.232", 3),
            entry("192.0.2.232", 2),
            entry("192.0.2.232", 1),
        ];
        entries[1]["pinned"] = true.into();
        entries[1]["future"] = true.into();
        for n in 1..=10 {
            entries.push(entry(&format!("10.0.0.{n}"), 47470));
        }
        std::fs::write(
            dir.path().join(ADDRESSES_FILE),
            serde_json::to_vec(&serde_json::json!({"schema":1,"entries":entries,"future":1})).unwrap(),
        )
        .unwrap();
        let loaded = AddressStore::load(dir.path()).unwrap();
        let mut expected = vec![SocketAddr::from(([192, 168, 8, 232], 3))];
        expected.extend((1..=7).map(|n| SocketAddr::from(([10, 0, 0, n], 47470))));
        assert_eq!(loaded.candidates(peer), expected);
        // The pin follows its IP to the kept (newest) entry.
        assert_eq!(
            loaded.route_record(peer),
            Some(oink_network::link::RouteRecord::Dial("192.0.2.232".parse().unwrap()))
        );
        assert_eq!(loaded.raw["future"], 1);
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(ADDRESSES_FILE)).unwrap()).unwrap();
        assert_eq!(on_disk, loaded.raw);
        assert_eq!(on_disk["schema"], 1);
        assert_eq!(AddressStore::load(dir.path()).unwrap().candidates(peer), expected);
    }
    #[test]
    fn route_probes_retain_a_remembered_nat_endpoint_outside_published_facts() {
        use oink_network::link::RouteRecord;
        let dir = tempfile::tempdir().unwrap();
        let peer = DeviceId([1; 32]);
        let published: SocketAddr = "192.0.2.2:47470".parse().unwrap();
        let manual: SocketAddr = "198.51.100.2:47480".parse().unwrap();
        let mut store = AddressStore::load(dir.path()).unwrap();
        store.remember(peer, manual).unwrap();
        assert_eq!(store.route_candidates(peer, vec![published]), [published]);
        let std::net::IpAddr::V4(ip) = manual.ip() else {
            unreachable!()
        };
        store
            .set_route_record(peer, Some(RouteRecord::Dial(ip)), Some(manual))
            .unwrap();
        let loaded = AddressStore::load(dir.path()).unwrap();
        assert_eq!(loaded.route_candidates(peer, vec![published]), [published, manual]);
        assert_eq!(loaded.route_candidates(peer, vec![manual]), [manual]);
        assert_eq!(loaded.route_candidates(peer, vec![]), [manual]);
        store.set_route_record(peer, None, None).unwrap();
        assert_eq!(store.route_candidates(peer, vec![published]), [published]);
        assert_eq!(
            store.candidates(peer),
            [manual],
            "automatic preserves ordinary dial hints"
        );
    }

    #[test]
    fn route_pins_persist_one_per_peer_and_survive_reload() {
        use oink_network::link::RouteRecord;
        let dir = tempfile::tempdir().unwrap();
        let peer = DeviceId([1; 32]);
        let other = DeviceId([2; 32]);
        let wifi: SocketAddr = "192.168.1.2:47470".parse().unwrap();
        let wired: SocketAddr = "10.77.0.2:47470".parse().unwrap();
        let mut addresses = AddressStore::load(dir.path()).unwrap();
        addresses.remember(peer, wifi).unwrap();
        addresses.remember(other, wifi).unwrap();
        assert_eq!(addresses.route_record(peer), None);
        // Dial pins mark the dialed entry and move it first.
        addresses
            .set_route_record(peer, Some(RouteRecord::Dial(ip(wired))), Some(wired))
            .unwrap();
        assert_eq!(addresses.route_record(peer), Some(RouteRecord::Dial(ip(wired))));
        assert_eq!(addresses.candidates(peer)[0], wired);
        // A keep-incoming record replaces it: still one record per peer.
        addresses
            .set_route_record(peer, Some(RouteRecord::Incoming("10.77.0.1".parse().unwrap())), None)
            .unwrap();
        let loaded = AddressStore::load(dir.path()).unwrap();
        assert_eq!(
            loaded.route_record(peer),
            Some(RouteRecord::Incoming("10.77.0.1".parse().unwrap()))
        );
        assert_eq!(loaded.route_record(other), None);
        assert!(
            loaded.raw["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e.get("pinned").is_none())
        );
        assert_eq!(loaded.raw["schema"], 1);
        // Automatic clears it; the address hints stay.
        addresses.set_route_record(peer, None, None).unwrap();
        let loaded = AddressStore::load(dir.path()).unwrap();
        assert_eq!(loaded.route_record(peer), None);
        assert_eq!(loaded.candidates(peer), [wired, wifi]);
        // A dial pin needs the dialed socket of the same address.
        assert!(
            addresses
                .set_route_record(peer, Some(RouteRecord::Dial(ip(wired))), Some(wifi))
                .is_err()
        );
        fn ip(address: SocketAddr) -> std::net::Ipv4Addr {
            let SocketAddr::V4(address) = address else {
                unreachable!()
            };
            *address.ip()
        }
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
        let future = v2::cbor::encode_map(&[(1, 2u8.write()), (2, RawValue::Array(vec![]))].into(), 1024).unwrap();
        std::fs::write(&path, &future).unwrap();
        assert!(matches!(
            FactsCache::load(dir.path()),
            Err(StoreError::UnsupportedSchema)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), future);
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
        let route = RawValue::Map([(1, stamp.clone()), (2, store::wire_device(DeviceId([1; 32])).write())].into());
        raw.insert(33, RawValue::Map([(1, route)].into()));
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
            [1, 5, 6, 7, 8, 9, 10, 12, 18, 25, 26, 27]
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
        assert!(cached.audio_config().unwrap().is_none());
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
    fn cache_keeps_the_mobile_form_factor_for_offline_badges() {
        let dir = tempfile::tempdir().unwrap();
        let mut raw = rich_facts().raw().clone();
        raw.insert(25, dto::DeviceKind::Mobile.write());
        raw.insert(31, dto::FormFactor::Tablet.write());
        let mut cache = FactsCache::load(dir.path()).unwrap();
        cache.observe(dto::DeviceFacts::from_raw(raw).unwrap()).unwrap();
        cache.flush().unwrap();
        let cached = FactsCache::load(dir.path())
            .unwrap()
            .get(GroupId([3; 16]), DeviceId([1; 32]))
            .unwrap();
        assert!(matches!(cached.form_factor(), Ok(Some(dto::FormFactor::Tablet))));
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
