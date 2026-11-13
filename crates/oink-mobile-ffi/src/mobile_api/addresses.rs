//! Member addresses the phone dials before discovery has answered:
//! user-edited ones ("编辑 IP", one manual IPv4 endpoint per group member)
//! and the endpoints of earlier successful direct links (as the PC's
//! `addresses.json`: most recent first, one per IPv4 with its newest port,
//! at most eight IPs per member, a failure never removes one). Both
//! are persisted with the group in the device-only directory (not backed up
//! or migrated). A dial tries the manual address, then the remembered ones,
//! then discovery.

use super::*;
use oink_foundation::DeviceId;
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

const FILE: &str = "member-addresses.json";
const REMEMBERED_FILE: &str = "member-last-addresses.json";
/// Remembered IPv4 addresses per member (most recent first), each with the
/// newest port it answered on; the least recently used IP is dropped first.
/// Same bound as the PC address book (user 2026-11-12).
const REMEMBERED_IPS_PER_MEMBER: usize = 8;
/// Same default as manual pairing (`query_pairing_address`).
const DEFAULT_PORT: u16 = 47470;

#[derive(Debug)]
pub(super) struct AddressBook {
    path: PathBuf,
    entries: BTreeMap<DeviceId, SocketAddr>,
    remembered_path: PathBuf,
    remembered: BTreeMap<DeviceId, Vec<SocketAddr>>,
}

impl AddressBook {
    /// A corrupt file is a start failure, not an empty book: dropping a
    /// user-entered address would silently stop reconnecting that member.
    pub(super) fn load(data_dir: &std::path::Path) -> Result<Self, MobileError> {
        let remembered_path = crate::device_only::dir(data_dir).join(REMEMBERED_FILE);
        let remembered = match std::fs::read(&remembered_path) {
            Ok(bytes) => serde_json::from_slice::<BTreeMap<String, Vec<String>>>(&bytes)
                .ok()
                .and_then(|raw| {
                    raw.into_iter()
                        .map(|(device, addresses)| {
                            let addresses = addresses
                                .iter()
                                .map(|address| address.parse().ok())
                                .collect::<Option<Vec<SocketAddr>>>()?;
                            Some((parse_device(&device)?, addresses))
                        })
                        .collect::<Option<BTreeMap<_, _>>>()
                })
                .ok_or_else(|| MobileError::Startup {
                    detail: "remembered member addresses are corrupt".into(),
                })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(MobileError::Startup {
                    detail: format!("remembered member addresses: {error}"),
                });
            }
        };
        let mut remembered = remembered;
        let mut trimmed = false;
        for list in remembered.values_mut() {
            trimmed |= trim_remembered(list);
        }
        if trimmed {
            // An over-full file from an earlier build is rewritten at once;
            // memory is trimmed either way and the next success persists it.
            if let Err(error) = write_remembered(&remembered_path, &remembered) {
                tracing::warn!(%error, "trimmed remembered member addresses could not be written back");
            }
        }
        let path = crate::device_only::dir(data_dir).join(FILE);
        let entries = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<BTreeMap<String, String>>(&bytes)
                .ok()
                .and_then(|raw| {
                    raw.into_iter()
                        .map(|(device, address)| Some((parse_device(&device)?, address.parse().ok()?)))
                        .collect::<Option<BTreeMap<_, _>>>()
                })
                .ok_or_else(|| MobileError::Startup {
                    detail: "member address book is corrupt".into(),
                })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(MobileError::Startup {
                    detail: format!("member address book: {error}"),
                });
            }
        };
        Ok(Self {
            path,
            entries,
            remembered_path,
            remembered,
        })
    }

    pub(super) fn get(&self, device: DeviceId) -> Option<SocketAddr> {
        self.entries.get(&device).copied()
    }

    /// Endpoints of this member's earlier successful direct links, most recent first.
    pub(super) fn remembered(&self, device: DeviceId) -> &[SocketAddr] {
        self.remembered.get(&device).map_or(&[], Vec::as_slice)
    }

    /// `address` reached `device` on an admitted direct link. Moves it to the
    /// front; true when the file changed. A failed write keeps the old list.
    pub(super) fn remember(&mut self, device: DeviceId, address: SocketAddr) -> Result<bool, MobileError> {
        if !address.is_ipv4() || address.port() == 0 {
            return Ok(false);
        }
        if self.remembered(device).first() == Some(&address) {
            return Ok(false);
        }
        let mut next = self.remembered.clone();
        let list = next.entry(device).or_default();
        // The newest port of this IP replaces the older ones.
        list.retain(|known| known.ip() != address.ip());
        list.insert(0, address);
        trim_remembered(list);
        write_remembered(&self.remembered_path, &next)?;
        self.remembered = next;
        Ok(true)
    }

    fn set(&mut self, device: DeviceId, address: Option<SocketAddr>) -> Result<(), MobileError> {
        let mut next = self.entries.clone();
        match address {
            Some(address) => next.insert(device, address),
            None => next.remove(&device),
        };
        let raw: BTreeMap<String, String> = next
            .iter()
            .map(|(device, address)| (device.to_hex(), address.to_string()))
            .collect();
        let bytes = serde_json::to_vec(&raw).map_err(|error| persistence(error.to_string()))?;
        oink_foundation::storage::atomic_write(&self.path, &bytes).map_err(|error| persistence(error.to_string()))?;
        self.entries = next;
        Ok(())
    }
}

/// Most recent first: keeps the first (newest) port of every IP and the
/// [`REMEMBERED_IPS_PER_MEMBER`] most recent IPs. True when it dropped one.
fn trim_remembered(list: &mut Vec<SocketAddr>) -> bool {
    let before = list.len();
    let mut seen = Vec::with_capacity(before);
    list.retain(|address| {
        if seen.contains(&address.ip()) {
            false
        } else {
            seen.push(address.ip());
            true
        }
    });
    list.truncate(REMEMBERED_IPS_PER_MEMBER);
    list.len() != before
}

fn write_remembered(
    path: &std::path::Path,
    remembered: &BTreeMap<DeviceId, Vec<SocketAddr>>,
) -> Result<(), MobileError> {
    let raw: BTreeMap<String, Vec<String>> = remembered
        .iter()
        .map(|(device, addresses)| (device.to_hex(), addresses.iter().map(ToString::to_string).collect()))
        .collect();
    let bytes = serde_json::to_vec(&raw).map_err(|error| persistence(error.to_string()))?;
    oink_foundation::storage::atomic_write(path, &bytes).map_err(|error| persistence(error.to_string()))
}

fn persistence(detail: String) -> MobileError {
    MobileError::Command {
        code: UserFacingCode::PersistenceFailed,
        detail,
    }
}

fn parse_device(value: &str) -> Option<DeviceId> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(value.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(DeviceId(bytes))
}

/// `a.b.c.d` or `a.b.c.d:port`; IPv4 only (the mobile transport is IPv4).
pub(super) fn parse_member_address(value: &str) -> Option<SocketAddr> {
    let value = value.trim();
    let address = value
        .parse::<SocketAddr>()
        .ok()
        .or_else(|| Some(SocketAddr::new(value.parse::<std::net::IpAddr>().ok()?, DEFAULT_PORT)))?;
    let std::net::IpAddr::V4(ip) = address.ip() else {
        return None;
    };
    (address.port() != 0 && !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast()).then_some(address)
}

impl MobileSession {
    fn member(&self, device: &str) -> Result<DeviceId, MobileError> {
        let peer = parse_device(device).ok_or_else(|| MobileError::Command {
            code: UserFacingCode::InvalidValue,
            detail: "invalid device ID".into(),
        })?;
        if peer == self.network.boot().device_id || !self.network.members().iter().any(|m| m.device == peer) {
            return Err(MobileError::Command {
                code: UserFacingCode::NotMember,
                detail: "device is not another member of this group".into(),
            });
        }
        Ok(peer)
    }

    pub(super) fn known_member_addresses(&self, device: &str) -> Result<Vec<String>, MobileError> {
        let peer = self.member(device)?;
        let mut addresses: Vec<SocketAddr> = lock(&self.addresses).get(peer).into_iter().collect();
        for address in self.nearby_addresses(peer).unwrap_or_default() {
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        Ok(addresses.into_iter().map(|address| address.to_string()).collect())
    }

    /// Save (or clear, with an empty string) the member's manual address.
    /// Works for online members too: the live link stays on its path and the
    /// address is used for every later reconnect; an offline member is dialed
    /// at once.
    pub(super) fn edit_member_address(&self, device: &str, address: &str) -> Result<(), MobileError> {
        let peer = self.member(device)?;
        let address = if address.trim().is_empty() {
            None
        } else {
            Some(parse_member_address(address).ok_or_else(|| MobileError::Command {
                code: UserFacingCode::InvalidAddress,
                detail: "IPv4 address with an optional port required".into(),
            })?)
        };
        lock(&self.addresses).set(peer, address)?;
        let mut candidates: Vec<SocketAddr> = address.into_iter().collect();
        for discovered in self.nearby_addresses(peer).unwrap_or_default() {
            if !candidates.contains(&discovered) {
                candidates.push(discovered);
            }
        }
        self.network.update_addresses(peer, candidates.clone());
        if self.network.links().by_peer(peer).is_none() && !candidates.is_empty() {
            // A dial already in flight is fine; the next presence pass retries.
            let _ = self
                .network
                .dial_member(peer, candidates, oink_network::link::DialOrigin::Initial);
        }
        self.refresh_presence();
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_addresses_accept_ipv4_with_default_port_and_persist() {
        assert_eq!(
            parse_member_address(" 192.168.1.20 "),
            Some("192.168.1.20:47470".parse().unwrap())
        );
        assert_eq!(
            parse_member_address("10.0.0.2:5000"),
            Some("10.0.0.2:5000".parse().unwrap())
        );
        for bad in [
            "",
            "fe80::1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "10.0.0.2:0",
            "host",
        ] {
            assert_eq!(parse_member_address(bad), None, "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        // A running session always has it: the group state lives there.
        std::fs::create_dir(crate::device_only::dir(dir.path())).unwrap();
        let device = DeviceId([9; 32]);
        let mut book = AddressBook::load(dir.path()).unwrap();
        book.set(device, parse_member_address("10.0.0.2")).unwrap();
        assert_eq!(
            AddressBook::load(dir.path()).unwrap().get(device),
            Some("10.0.0.2:47470".parse().unwrap())
        );
        book.set(device, None).unwrap();
        assert_eq!(AddressBook::load(dir.path()).unwrap().get(device), None);
        std::fs::write(crate::device_only::dir(dir.path()).join(FILE), b"[").unwrap();
        assert!(AddressBook::load(dir.path()).is_err());
    }

    #[test]
    fn successful_addresses_are_remembered_most_recent_first_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(crate::device_only::dir(dir.path())).unwrap();
        let device = DeviceId([9; 32]);
        let other = DeviceId([8; 32]);
        let (home, office): (SocketAddr, SocketAddr) =
            ("192.168.1.20:47470".parse().unwrap(), "10.0.0.2:5000".parse().unwrap());
        let mut book = AddressBook::load(dir.path()).unwrap();
        assert!(book.remembered(device).is_empty());
        assert!(book.remember(device, home).unwrap());
        assert!(!book.remember(device, home).unwrap(), "already the most recent");
        assert!(book.remember(device, office).unwrap());
        assert!(book.remember(device, home).unwrap());
        assert!(!book.remember(device, "[fe80::1]:47470".parse().unwrap()).unwrap());
        assert_eq!(book.remembered(device), [home, office]);
        assert!(book.remembered(other).is_empty());
        // Twelve ports on one IP: one entry with the newest port.
        for port in 1..=12 {
            assert!(book.remember(other, SocketAddr::from(([10, 0, 0, 9], port))).unwrap());
        }
        assert_eq!(book.remembered(other), [SocketAddr::from(([10, 0, 0, 9], 12))]);
        let loaded = AddressBook::load(dir.path()).unwrap();
        assert_eq!(loaded.remembered(device), [home, office]);
        assert_eq!(loaded.remembered(other), [SocketAddr::from(([10, 0, 0, 9], 12))]);
        // Ten IPs: the eight most recently used stay, newest first.
        let ip = |n: u8| SocketAddr::from(([172, 16, 0, n], 47470));
        for n in 1..=10 {
            book.remember(device, ip(n)).unwrap();
        }
        let expected: Vec<_> = (3..=10).rev().map(ip).collect();
        assert_eq!(book.remembered(device), expected);
        assert_eq!(AddressBook::load(dir.path()).unwrap().remembered(device), expected);
        std::fs::write(crate::device_only::dir(dir.path()).join(REMEMBERED_FILE), b"[").unwrap();
        assert!(AddressBook::load(dir.path()).is_err());
    }

    #[test]
    fn an_over_full_remembered_file_is_trimmed_on_load_and_written_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(crate::device_only::dir(dir.path())).unwrap();
        let device = DeviceId([7; 32]);
        // Newest first, as an earlier build wrote it: three ports of one IP.
        let mut raw = vec![
            "192.0.2.234:3".to_string(),
            "192.0.2.234:2".into(),
            "192.0.2.234:1".into(),
        ];
        raw.extend((1..=10).map(|n| format!("10.0.0.{n}:47470")));
        let path = crate::device_only::dir(dir.path()).join(REMEMBERED_FILE);
        std::fs::write(
            &path,
            serde_json::to_vec(&BTreeMap::from([(device.to_hex(), raw)])).unwrap(),
        )
        .unwrap();
        let book = AddressBook::load(dir.path()).unwrap();
        let mut expected: Vec<SocketAddr> = vec!["192.0.2.234:3".parse().unwrap()];
        expected.extend((1..=7).map(|n| SocketAddr::from(([10, 0, 0, n], 47470))));
        assert_eq!(book.remembered(device), expected);
        let on_disk: BTreeMap<String, Vec<String>> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk[&device.to_hex()].len(), REMEMBERED_IPS_PER_MEMBER);
        assert_eq!(on_disk[&device.to_hex()][0], "192.0.2.234:3");
    }
}
