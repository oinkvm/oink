//! Local-selection authority and the logical paths of one materialized batch.
//! Only local enumeration supplies paths; no remote request can construct these.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::content) struct Followed {
    source: PathBuf,
    target: PathBuf,
    identity: [u64; 2],
    created: Option<SystemTime>,
}
impl Followed {
    pub fn verify(&self) -> io::Result<()> {
        let verify = || {
            let path = self.source.canonicalize()?;
            let current = inspect(&path)?;
            if path != self.target || current.identity != self.identity || current.created != self.created {
                return Err(unavailable(Unavailable::Superseded));
            }
            Ok(())
        };
        verify().map_err(|_| unavailable(Unavailable::Superseded))
    }
    pub fn target(&self) -> &Path {
        &self.target
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mapping {
    path: PathBuf,
    logical: Vec<String>,
    fingerprint: Fingerprint,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::content) struct Batch {
    mappings: Vec<Mapping>,
    followed: Vec<Followed>,
}
pub(in crate::content) struct Captured {
    pub path: PathBuf,
    pub fingerprint: Fingerprint,
    pub link: Option<String>,
    pub followed: Vec<Followed>,
}
impl Batch {
    pub fn include(&mut self, path: &Path, logical: Vec<String>, fingerprint: &Fingerprint) {
        self.mappings.retain(|m| m.logical != logical);
        self.mappings.push(Mapping {
            path: path.into(),
            logical,
            fingerprint: fingerprint.clone(),
        });
    }

    fn inside(&self, path: &Path, fingerprint: &Fingerprint) -> io::Result<Option<Vec<String>>> {
        // Exact identities handle directory cycles and file aliases. Longest
        // directory prefix gives a selected descendant its own root identity.
        let mut candidates: Vec<_> = self.mappings.iter().collect();
        candidates.sort_by_key(|m| std::cmp::Reverse(m.path.components().count()));
        for mapping in candidates {
            let Ok(base) = inspect(&mapping.path) else { continue };
            if base.identity != mapping.fingerprint.identity || base.created != mapping.fingerprint.created {
                continue;
            }
            if fingerprint.identity == base.identity && fingerprint.created == base.created {
                return Ok(Some(mapping.logical.clone()));
            }
            if !base.directory {
                continue;
            }
            let Ok(rest) = path.strip_prefix(&mapping.path) else {
                continue;
            };
            let mut physical = mapping.path.clone();
            let mut logical = mapping.logical.clone();
            for component in rest.components() {
                let name = component.as_os_str();
                let children = children(&physical)?;
                let (display, next) = children
                    .into_iter()
                    .find(|(_, p)| p.file_name() == Some(name))
                    .ok_or_else(|| unavailable(Unavailable::Superseded))?;
                logical.push(display);
                physical = next;
            }
            if inspect(&physical.canonicalize()?)?.identity == fingerprint.identity {
                return Ok(Some(logical));
            }
        }
        Ok(None)
    }
    pub fn capture(
        &mut self,
        path: &Path,
        logical: Vec<String>,
        inherited: &[Followed],
    ) -> io::Result<Option<Captured>> {
        for guard in inherited {
            guard.verify()?;
        }
        let metadata = fs::symlink_metadata(path)?;
        let is_link = crate::cache::is_link(&metadata);
        #[cfg(windows)]
        if is_link && !windows_link(path)? {
            return Ok(None);
        }
        if !is_link {
            let fingerprint = inspect(path)?;
            let path = path.canonicalize()?;
            readable(&path, &fingerprint)?;
            if fingerprint.directory {
                self.include(&path, logical, &fingerprint);
            }
            return Ok(Some(Captured {
                path,
                fingerprint,
                link: None,
                followed: inherited.to_vec(),
            }));
        }
        for guard in self.followed.iter().filter(|guard| guard.source == path) {
            guard.verify()?;
        }
        let target = path.canonicalize()?;
        let fingerprint = inspect(&target)?;
        readable(&target, &fingerprint)?;
        let internal = self.inside(&target, &fingerprint)?;
        if cfg!(windows) && internal.is_some() {
            return Ok(None);
        }
        if let Some(destination) = internal {
            // Keep an in-batch link chain (e.g. Versions/Current/Resources),
            // after canonical identity proves it is also physically in-batch.
            // Only collapse to the confirmed destination when the lexical path
            // has no representation in this materialized batch.
            #[cfg(target_os = "macos")]
            let destination = lexical_target(path)
                .and_then(|lexical| self.inside(&lexical, &fingerprint))
                .ok()
                .flatten()
                .unwrap_or(destination);
            let parent = &logical[..logical.len().saturating_sub(1)];
            let common = parent.iter().zip(&destination).take_while(|(a, b)| a == b).count();
            let mut relative = vec!["..".to_owned(); parent.len() - common];
            relative.extend_from_slice(&destination[common..]);
            let relative = if relative.is_empty() {
                ".".into()
            } else {
                relative.join("/")
            };
            return Ok(Some(Captured {
                path: path.into(),
                fingerprint: inspect_leaf(path)?,
                link: Some(relative),
                followed: inherited.to_vec(),
            }));
        }
        let mut followed = inherited.to_vec();
        followed.push(Followed {
            source: path.into(),
            target: target.clone(),
            identity: fingerprint.identity,
            created: fingerprint.created,
        });
        for guard in &followed {
            guard.verify()?;
        }
        self.followed.retain(|guard| guard.source != path);
        self.followed.push(followed.last().unwrap().clone());
        self.include(&target, logical, &fingerprint);
        Ok(Some(Captured {
            path: target,
            fingerprint,
            link: None,
            followed,
        }))
    }
}

pub(in crate::content) fn component(name: &str, used: &mut HashSet<String>) -> String {
    let target = crate::sync::files::TargetOs::MacOs;
    let base = crate::sync::files::sanitize_component(name, target);
    let mut display = base.clone();
    let mut number = 1;
    while !used.insert(display.to_lowercase()) {
        number += 1;
        display = crate::sync::files::names::with_suffix(&base, number, target);
    }
    display
}
pub(in crate::content) fn children(parent: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut paths = Vec::new();
    for child in fs::read_dir(parent)? {
        match child {
            Ok(child) => {
                if let Some(name) = child.file_name().to_str() {
                    paths.push((name.to_owned(), child.path()));
                }
            }
            Err(error) => tracing::warn!(reason=?error.kind(), "skipping unavailable directory item"),
        }
    }
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    let mut used = HashSet::new();
    Ok(paths
        .into_iter()
        .map(|(name, path)| (component(&name, &mut used), path))
        .collect())
}

/// Reparse is a broad storage mechanism, not permission to traverse. Only
/// actual symlinks and mount-point junctions derive authority from selection.
#[cfg(windows)]
fn windows_link(path: &Path) -> io::Result<bool> {
    use std::{
        mem::size_of,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::*;
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: borrowed handle and fixed-size writable output survive the call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(matches!(info.ReparseTag, 0xa0000003 | 0xa000000c))
}

fn readable(_path: &Path, _fingerprint: &Fingerprint) -> io::Result<()> {
    // Readability is a preflight only, not authority: identity and scope are
    // rechecked when the file is opened/read. access does not hydrate bytes.
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let name = CString::new(_path.as_os_str().as_bytes()).map_err(|_| unavailable(Unavailable::InvalidData))?;
        let mode = libc::R_OK | if _fingerprint.directory { libc::X_OK } else { 0 };
        // SAFETY: live NUL-terminated local path; no filesystem mutation.
        if unsafe { libc::access(name.as_ptr(), mode) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn lexical_target(link: &Path) -> io::Result<PathBuf> {
    let target = fs::read_link(link)?;
    let path = if target.is_absolute() {
        target
    } else {
        link.parent()
            .ok_or_else(|| unavailable(Unavailable::InvalidData))?
            .canonicalize()?
            .join(target)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            part => normalized.push(part.as_os_str()),
        }
    }
    Ok(normalized)
}
