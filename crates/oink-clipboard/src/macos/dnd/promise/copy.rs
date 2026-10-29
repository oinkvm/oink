//! Privacy-safe copy diagnostics. Values are local log codes, not wire enums.
use super::*;
use std::io::{Read, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum CopyReason {
    Success = 0,
    NativeReader = 1,
    InvalidUrl = 2,
    OutsideDestination = 3,
    InvalidName = 4,
    Cancelled = 5,
    Limit = 6,
    SourceMetadata = 7,
    Symlink = 8,
    UnsupportedType = 9,
    CreateDirectory = 10,
    Enumerate = 11,
    OpenSource = 12,
    CreateTarget = 13,
    Read = 14,
    Write = 15,
    SetTimes = 16,
    Sync = 17,
    SizeMismatch = 18,
    SourceChanged = 19,
}
#[derive(Debug)]
pub(super) struct CopyError {
    pub reason: CopyReason,
    pub error: io::Error,
}
impl CopyError {
    pub fn new(reason: CopyReason, error: io::Error) -> Self {
        Self { reason, error }
    }
    pub fn invalid(reason: CopyReason) -> Self {
        Self::new(reason, content::unavailable(Unavailable::InvalidData))
    }
    pub fn unavailable(&self) -> Unavailable {
        content::reason(&self.error)
    }
}

pub(super) fn copy_tree(
    source: &Path,
    target: &Path,
    depth: usize,
    count: &mut usize,
    live: &dyn Fn() -> bool,
) -> Result<(), CopyError> {
    let at = |reason| move |error| CopyError::new(reason, error);
    *count += 1;
    if !live() {
        return Err(CopyError::new(
            CopyReason::Cancelled,
            content::unavailable(Unavailable::Cancelled),
        ));
    }
    if depth > 256 || *count > crate::model::MAX_DISCOVERED_NODES {
        return Err(CopyError::new(
            CopyReason::Limit,
            content::unavailable(Unavailable::LimitExceeded),
        ));
    }
    let before = fs::symlink_metadata(source).map_err(at(CopyReason::SourceMetadata))?;
    if crate::cache::is_link(&before) {
        return Err(CopyError::invalid(CopyReason::Symlink));
    }
    if before.is_dir() {
        fs::create_dir(target).map_err(at(CopyReason::CreateDirectory))?;
        for child in fs::read_dir(source).map_err(at(CopyReason::Enumerate))? {
            let child = child.map_err(at(CopyReason::Enumerate))?;
            copy_tree(&child.path(), &target.join(child.file_name()), depth + 1, count, live)?;
        }
        crate::store::sync_directory(target).map_err(at(CopyReason::Sync))?;
    } else if before.is_file() {
        let mut input = fs::File::open(source).map_err(at(CopyReason::OpenSource))?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .map_err(at(CopyReason::CreateTarget))?;
        let mut buffer = vec![0; 1024 * 1024];
        let mut copied = 0u64;
        loop {
            if !live() {
                return Err(CopyError::new(
                    CopyReason::Cancelled,
                    content::unavailable(Unavailable::Cancelled),
                ));
            }
            let n = input.read(&mut buffer).map_err(at(CopyReason::Read))?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n]).map_err(at(CopyReason::Write))?;
            copied += n as u64;
        }
        output
            .set_times(fs::FileTimes::new().set_modified(before.modified().map_err(at(CopyReason::SourceMetadata))?))
            .map_err(at(CopyReason::SetTimes))?;
        output.sync_all().map_err(at(CopyReason::Sync))?;
        if copied != before.len() {
            return Err(CopyError::invalid(CopyReason::SizeMismatch));
        }
    } else {
        return Err(CopyError::invalid(CopyReason::UnsupportedType));
    }
    let after = fs::symlink_metadata(source).map_err(at(CopyReason::SourceMetadata))?;
    use std::os::unix::fs::MetadataExt;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.modified().map_err(at(CopyReason::SourceMetadata))?
            != after.modified().map_err(at(CopyReason::SourceMetadata))?
    {
        return Err(CopyError::invalid(CopyReason::SourceChanged));
    }
    Ok(())
}
