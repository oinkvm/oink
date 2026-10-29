//! Numeric local diagnostic evidence. This storage has no upload entry point and
//! cannot confer report consent. Instance correlation stays on this machine.
use oink_telemetry::*;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub root: PathBuf,
    pub instance: String,
}
impl Target {
    pub fn valid(&self) -> bool {
        #[cfg(windows)]
        if !local_path(&self.root) {
            return false;
        }
        self.instance.len() == 24 && self.instance.bytes().all(|b| b.is_ascii_hexdigit())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub instance: String,
    pub collected_at: u64,
    pub faulting_thread: u32,
    pub crash: SafeCrash,
}
pub fn write(target: &Target, crash: SafeCrash, collected_at: u64, faulting_thread: u32) -> Result<()> {
    if !target.valid() {
        return Err(Error::Invalid);
    }
    std::fs::create_dir_all(&target.root).map_err(|_| Error::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target.root, std::fs::Permissions::from_mode(0o700)).map_err(|_| Error::Io)?;
    }
    #[cfg(windows)]
    crate::collector::restrict_directory(&target.root)?;
    let summary = Summary {
        instance: target.instance.clone(),
        collected_at,
        faulting_thread,
        crash,
    };
    let bytes = serde_json::to_vec(&summary).map_err(|_| Error::Invalid)?;
    if bytes.len() > 32768 {
        return Err(Error::Limit);
    }
    oink_telemetry::store::atomic_write(&target.root.join("crash.json"), &bytes)
}
pub fn read(root: &Path) -> Option<Summary> {
    let mut bytes = Vec::new();
    std::fs::File::open(root.join("crash.json"))
        .ok()?
        .take(32769)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 32768 {
        return None;
    }
    let summary: Summary = serde_json::from_slice(&bytes).ok()?;
    if summary.instance.len() != 24
        || !summary.instance.bytes().all(|b| b.is_ascii_hexdigit())
        || summary.collected_at > now()
        || now().saturating_sub(summary.collected_at) > 7 * 86400
        || Report::SafeCrash(summary.crash.clone()).validate().is_err()
    {
        return None;
    }
    Some(summary)
}

pub fn instance_bytes(instance: &str) -> Option<[u8; 12]> {
    if instance.len() != 24 || !instance.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 12];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&instance[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}
pub fn instance_hex(instance: [u8; 12]) -> String {
    instance.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(windows)]
pub(crate) fn local_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
}
