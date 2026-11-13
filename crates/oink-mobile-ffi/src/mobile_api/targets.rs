//! Where the transfer page and the share picker can send: the devices online
//! now, the group's size for the composer rules, and the last online list
//! the share picker shows before the first connect round ends.

use super::*;
use std::path::Path;

/// What kind of device a target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TransferTargetKind {
    /// A PC: attachments go there and it announces them to the group.
    Computer,
    /// A phone or tablet: attachments go to it alone (phone to phone).
    Mobile,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct TransferTarget {
    /// 64-hex device id.
    pub device: String,
    pub name: String,
    pub kind: TransferTargetKind,
    /// The platform badge.
    pub os: CardOs,
    /// The crowned Android badge (as on the device cards).
    pub android_privileged: bool,
    /// Order in which the links came up (smaller is earlier).
    pub connection_order: u64,
}

/// The one other device of a group of two, online or not.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct TransferOnlyDevice {
    pub device: String,
    pub name: String,
    pub kind: TransferTargetKind,
    pub os: CardOs,
    pub online: bool,
}

#[derive(Debug, Clone, Default, PartialEq, uniffi::Record)]
pub struct TransferTargetsView {
    pub revision: u64,
    /// Other devices online now: computers first, then mobile devices, each
    /// in connection order.
    pub online: Vec<TransferTarget>,
    /// Group members other than this device, online or not.
    pub other_devices: u32,
    /// Set when `other_devices == 1`: that device.
    pub only_device: Option<TransferOnlyDevice>,
    /// The session's first connect round is still running
    /// ("正在查找设备…").
    pub searching: bool,
    /// Current active target: usable focus PC, otherwise first usable direct device.
    #[uniffi(default = None)]
    pub active_device: Option<String>,
}

impl TransferTargetsView {
    pub(super) fn active_pc(&self) -> Option<String> {
        self.online
            .iter()
            .find(|target| {
                target.kind == TransferTargetKind::Computer && Some(&target.device) == self.active_device.as_ref()
            })
            .or_else(|| {
                self.online
                    .iter()
                    .find(|target| target.kind == TransferTargetKind::Computer)
            })
            .map(|target| target.device.clone())
    }
}

/// The last non-empty online list a session of this phone saw (the app or a
/// share session; both write it). Readable without a session, so the share
/// picker shows it at once. Unreadable or missing: empty.
#[uniffi::export]
pub fn cached_transfer_targets(data_dir: String) -> Vec<TransferTarget> {
    read_cache(&crate::device_only::transfer_targets(Path::new(&data_dir)))
}

fn os_name(os: CardOs) -> &'static str {
    match os {
        CardOs::Android => "android",
        CardOs::Ios => "ios",
        CardOs::Macos => "macos",
        CardOs::Windows => "windows",
        CardOs::Linux => "linux",
        CardOs::Other => "other",
    }
}

fn read_cache(path: &Path) -> Vec<TransferTarget> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_slice(&bytes) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            Some(TransferTarget {
                device: entry.get("device")?.as_str()?.to_owned(),
                name: entry.get("name")?.as_str()?.to_owned(),
                kind: match entry.get("kind")?.as_str()? {
                    "computer" => TransferTargetKind::Computer,
                    "mobile" => TransferTargetKind::Mobile,
                    _ => return None,
                },
                os: presence::card_os(entry.get("os")?.as_str()?),
                android_privileged: entry.get("android_privileged")?.as_bool()?,
                connection_order: entry.get("connection_order")?.as_u64()?,
            })
        })
        .collect()
}

pub(super) fn write_cache(path: &Path, targets: &[TransferTarget]) -> std::io::Result<()> {
    let entries: Vec<serde_json::Value> = targets
        .iter()
        .map(|target| {
            serde_json::json!({
                "device": target.device,
                "name": target.name,
                "kind": match target.kind {
                    TransferTargetKind::Computer => "computer",
                    TransferTargetKind::Mobile => "mobile",
                },
                "os": os_name(target.os),
                "android_privileged": target.android_privileged,
                "connection_order": target.connection_order,
            })
        })
        .collect();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Replaced atomically: the app and a share session may read it any time.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&entries).map_err(std::io::Error::other)?)?;
    std::fs::rename(temporary, path)
}

impl MobileSession {
    /// The current targets (also pushed as `MobileEvent::TransferTargets`).
    pub(super) fn targets_view(&self) -> TransferTargetsView {
        lock_presence(self).targets.clone()
    }

    /// Keeps the cached list: the online list when there is one, else the
    /// previous cache without devices that left the group.
    pub(super) fn persist_targets(&self, online: &[TransferTarget]) {
        let path = &self.targets_cache;
        let previous = read_cache(path);
        let next = if online.is_empty() {
            let members: Vec<String> = self
                .network
                .members()
                .iter()
                .map(|member| member.device.to_hex())
                .collect();
            previous
                .iter()
                .filter(|target| members.contains(&target.device))
                .cloned()
                .collect()
        } else {
            online.to_vec()
        };
        if next != previous
            && let Err(error) = write_cache(path, &next)
        {
            tracing::warn!(kind = ?error.kind(), "transfer target cache not written");
        }
    }

    fn forget_cached_target(&self, device: &str) {
        let path = &self.targets_cache;
        let mut cached = read_cache(path);
        let before = cached.len();
        cached.retain(|target| target.device != device);
        if cached.len() != before
            && let Err(error) = write_cache(path, &cached)
        {
            tracing::warn!(kind = ?error.kind(), "transfer target cache not written");
        }
    }
}

fn lock_presence(session: &MobileSession) -> std::sync::MutexGuard<'_, presence::Presence> {
    session.presence.lock().unwrap_or_else(|error| error.into_inner())
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileSession {
    /// The devices a transfer can go to now (see `TransferTargetsView`).
    pub fn transfer_targets(&self) -> TransferTargetsView {
        self.targets_view()
    }

    /// Reaches `device` now for the share picker: true once it is online
    /// (at once when it already is), false when this attempt ends without
    /// it (every address and the PC relays failed, it is incompatible, or
    /// it is no longer in the group). The dial's own budget is the only
    /// limit. A device found offline is dropped from the cached list.
    pub async fn transfer_confirm_online(&self, device: String) -> Result<bool, MobileError> {
        let id = control::parse_device_id(&device)?;
        let candidates = self.member_candidates(id);
        let online = self.network.confirm_reachable(id, candidates).await;
        if !online {
            self.forget_cached_target(&device);
        }
        self.refresh_presence();
        Ok(online)
    }
}
