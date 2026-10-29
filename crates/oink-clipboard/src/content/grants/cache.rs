//! Fulfilments live beside the authority journal, outside the transient store.
//! Only the journal lifecycle may remove them; cache clearing cannot revoke a
//! current clipboard or an admitted receiver, including before startup recovery.
use super::*;
use crate::cache::{CacheCategory, CacheSize};

pub fn usage(directory: &Path) -> (CacheSize, bool) {
    let mut size = CacheSize {
        category: CacheCategory::SourceSnapshots,
        bytes: 0,
        kept_bytes: 0,
    };
    let root = directory.join(fulfilled::DIRECTORY);
    match crate::cache::tree_bytes(&root) {
        Ok(bytes) => {
            size.bytes = bytes;
            size.kept_bytes = bytes;
            (size, false)
        }
        Err(error) => (size, error.kind() != io::ErrorKind::NotFound),
    }
}
