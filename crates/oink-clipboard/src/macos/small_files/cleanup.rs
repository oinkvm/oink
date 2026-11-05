//! Metadata-only clipboard identity and Oink consumer/source protection.
use super::*;
use crate::cache::{CacheCategory, CacheSize};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct ClipboardIdentity {
    pub name: Option<String>,
    pub key: [u8; 32],
}
impl ClipboardIdentity {
    fn current(&self) -> io::Result<bool> {
        super::super::clipboard_key::current(self.name.as_deref()).map(|key| key == self.key)
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Record {
    pub created_ms: u64,
    pub clipboard: Option<ClipboardIdentity>,
    pub bytes: u64,
}
#[derive(Default)]
pub(super) struct Scan {
    pub bytes: u64,
    pub kept: u64,
    pub removed: u64,
    pub incomplete: bool,
    pub next_expiry: Option<u64>,
}

fn busy(path: &Path, record: &Record) -> io::Result<bool> {
    if record
        .clipboard
        .as_ref()
        .map(ClipboardIdentity::current)
        .transpose()?
        .unwrap_or(false)
    {
        return Ok(true);
    }
    if crate::cache::source_paths()
        .iter()
        .any(|source| source.starts_with(path) || path.starts_with(source))
    {
        return Ok(true);
    }

    Ok(false)
}
pub(super) fn load(path: &Path) -> io::Result<Record> {
    let path = path.join(RECORD);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16384 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}
pub(super) fn scan(root: &Path, clean: bool, clear: bool, now: u64) -> io::Result<Scan> {
    let mut result = Scan::default();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".lock" {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let ours = path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if !ours || !metadata.is_dir() || metadata.file_type().is_symlink() {
            result.incomplete = true;
            continue;
        }
        let bytes = match crate::cache::tree_bytes(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                result.incomplete = true;
                continue;
            }
        };
        let partial = path.extension().is_some_and(|extension| extension == "partial");
        let ready = path.extension().is_some_and(|extension| extension == "ready");
        let lock = match lock_at(&path, false) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound && partial => None,
            Err(_) => {
                result.incomplete = true;
                result.bytes += bytes;
                result.kept += bytes;
                continue;
            }
        };
        // A locked partial is active. A missing lock in a partial is a crashed
        // allocation before creation of its consumer lock, not a live transfer.
        let orphan = partial && !path.join(".lock").exists();
        let mut protected = lock.is_none() && !orphan;
        let mut removable = partial && !protected;
        if ready {
            match load(&path) {
                Ok(record) => {
                    let expires = record.created_ms.saturating_add(RETENTION_MS).saturating_add(1);
                    if expires > now {
                        result.next_expiry = Some(result.next_expiry.map_or(expires, |old| old.min(expires)));
                    }
                    let due = clear || now.saturating_sub(record.created_ms) > RETENTION_MS;
                    // Oink pins and current clipboard identity protect a batch
                    // independently of its age or an explicit clear request.
                    if !protected {
                        protected = match busy(&path, &record) {
                            Ok(busy) => busy,
                            Err(_) => {
                                result.incomplete = true;
                                true
                            }
                        };
                    }
                    removable = due && !protected;
                }
                Err(_) => {
                    result.incomplete = true;
                    protected = true;
                }
            }
        }
        if clean && removable {
            remove_directory(&path)?;
            result.removed += bytes;
            crate::cache::changed(root);
        } else {
            result.bytes += bytes;
            if protected {
                result.kept += bytes;
            }
        }
    }
    Ok(result)
}
pub(super) fn maintain(root: &Path, clear: bool, now: u64) -> io::Result<Scan> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Scan::default()),
        Err(error) => return Err(error),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(_) => {}
    }
    let lock = lock_root(root)?;
    let mut scan = scan(root, lock.is_some(), clear, now)?;
    scan.incomplete |= lock.is_none();
    Ok(scan)
}
pub(crate) fn cache_usage(data: &Path, clear: bool) -> (CacheSize, u64, bool) {
    let scan = maintain(&data.join("ClipboardFiles"), clear, received::now());
    match scan {
        Ok(scan) => (
            CacheSize {
                category: CacheCategory::ClipboardFiles,
                bytes: scan.bytes,
                kept_bytes: scan.kept,
            },
            scan.removed,
            scan.incomplete,
        ),
        Err(_) => (
            CacheSize {
                category: CacheCategory::ClipboardFiles,
                bytes: 0,
                kept_bytes: 0,
            },
            0,
            true,
        ),
    }
}
