//! Durable native namespace; does not retain transfer-cache leases.
use super::*;
use crate::received::{self, Basis, Item, Reader, Usage};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Node {
    pub path: PathBuf,
    pub life: Item,
    pub directory: bool,
    pub deleted: bool,
    pub handed_off: bool,
    pub length: u64,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub deliveries: BTreeMap<u64, Vec<Node>>,
    #[serde(skip)]
    pub opens: HashMap<(u64, usize), i64>,
    #[serde(skip)]
    pub pending_gaps: HashSet<u64>,
}
impl Journal {
    pub fn load(root: &Path) -> io::Result<Self> {
        let mut state: Self = match std::fs::read(root.with_extension("received")) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e),
        };
        for nodes in state.deliveries.values_mut() {
            for node in nodes {
                if node.path.is_absolute()
                    || node
                        .path
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                node.life.gap(received::now());
            }
        }
        Ok(state)
    }
    fn merge_discovered(&mut self, id: u64, found: BTreeMap<usize, Node>) {
        let Some(last) = found.keys().last().copied() else {
            return;
        };
        let existing = self.deliveries.entry(id).or_default();
        let previous_len = existing.len();
        let first = found.values().next().unwrap();
        existing.resize_with(existing.len().max(last + 1), || Node {
            path: first.path.components().take(2).collect(),
            life: Item::new(received::now()),
            directory: true,
            deleted: true,
            handed_off: false,
            length: 0,
        });
        for (index, node) in found {
            if index >= previous_len {
                existing[index] = node;
                continue;
            }
            let old = &mut existing[index];
            if old.deleted {
                continue;
            } // no late discovery can revive a tombstone
            old.path = node.path;
            old.directory = node.directory;
            old.length = node.length;
            old.life.materialized |= node.life.materialized;
            old.handed_off |= node.handed_off;
            old.life.gap(received::now());
        }
    }
    pub fn save(&self, root: &Path) -> io::Result<()> {
        oink_foundation::storage::atomic_write(&root.with_extension("received"), &serde_json::to_vec(self)?)
    }
}
impl Service {
    pub(super) fn native_complete(&self, id: u64, index: usize, length: u64) {
        let mut journal = self.received.lock().unwrap();
        if let Some(node) = journal.deliveries.get_mut(&id).and_then(|v| v.get_mut(index)) {
            node.handed_off = true;
            node.length = length;
        }
        if journal.save(&self.root).is_err() {
            for node in journal.deliveries.values_mut().flatten() {
                node.life.gap(received::now());
            }
        }
        let path = journal
            .deliveries
            .get(&id)
            .and_then(|nodes| nodes.get(index))
            .map(|node| self.root.join(&node.path));
        drop(journal);
        if let Some(path) = path
            && self.full_and_pin(&path, length).unwrap_or(false)
        {
            let mut journal = self.received.lock().unwrap();
            if let Some(node) = journal.deliveries.get_mut(&id).and_then(|v| v.get_mut(index)) {
                node.life.materialized = true;
            }
            let _ = journal.save(&self.root);
        }
    }
    pub(super) fn received_sweep(&self) {
        let snapshot = self.received.lock().unwrap().deliveries.clone();
        for (id, nodes) in snapshot {
            for (index, node) in nodes.iter().enumerate() {
                if node.deleted || node.directory {
                    continue;
                }
                let path = self.root.join(&node.path);
                let identity = self.disk_identity(&path);
                let owned_incomplete = identity
                    .as_ref()
                    .is_ok_and(|(owner, slot, full)| *owner == id && *slot == index && !full);
                let missing = std::fs::symlink_metadata(&path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
                let owned = identity
                    .as_ref()
                    .is_ok_and(|(owner, slot, _)| *owner == id && *slot == index);
                let complete = owned && self.full_and_pin(&path, node.length).unwrap_or(false);
                let live = self.deliveries.lock().unwrap().get(&id).cloned();
                let mut journal = self.received.lock().unwrap();
                let busy = journal.opens.get(&(id, index)).copied().unwrap_or(0) > 0
                    || live.as_ref().is_some_and(|d| d.readers.load(Ordering::Acquire) > 0);
                let Some(current) = journal.deliveries.get_mut(&id).and_then(|v| v.get_mut(index)) else {
                    continue;
                };
                if current.path != node.path {
                    continue;
                }
                if complete {
                    current.life.materialized = true;
                }
                // Recovery only removes provably never-complete placeholders. A
                // successful handoff or prior materialization is protected on errors.
                let stale = !current.life.materialized && !current.handed_off && live.is_none() && owned_incomplete;
                if missing {
                    current.deleted = true;
                    continue;
                }
                if !owned {
                    continue;
                }
                let reason = current
                    .life
                    .cleanup(received::now(), received::setting(), Basis::LastUsed, busy);
                if !busy && (stale || reason.is_some()) {
                    // OS sharing denies deletion while an incompatible data handle
                    // exists; a failed deletion is retried, never counted as freed.
                    drop(journal);
                    let allocated = std::fs::symlink_metadata(&path)
                        .and_then(|m| crate::cache::allocated(&path, &m))
                        .unwrap_or(0);
                    let result = exclusive_delete(&path, || {
                        let journal = self.received.lock().unwrap();
                        let Some(current) = journal.deliveries.get(&id).and_then(|v| v.get(index)) else {
                            return false;
                        };
                        let busy = journal.opens.get(&(id, index)).copied().unwrap_or(0) > 0;
                        current.path == node.path
                            && !current.deleted
                            && !busy
                            && (stale && !current.life.materialized && !current.handed_off
                                || current
                                    .life
                                    .cleanup(received::now(), received::setting(), Basis::LastUsed, false)
                                    .is_some())
                    });
                    let mut journal = self.received.lock().unwrap();
                    let Some(current) = journal.deliveries.get_mut(&id).and_then(|v| v.get_mut(index)) else {
                        continue;
                    };
                    match result {
                        Ok(()) => {
                            current.deleted = true;
                            tracing::info!(
                                cleanup_reason = if stale {
                                    "stale"
                                } else {
                                    match reason.unwrap() {
                                        received::Reason::Shell24h => "shell_24h",
                                        received::Reason::Age7 => "age_7d",
                                        received::Reason::Age30 => "age_30d",
                                    }
                                },
                                count = 1_u64,
                                removed_bytes = allocated,
                                "received file removed"
                            );
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => current.deleted = true,
                        Err(_) => {
                            tracing::debug!(
                                cleanup_reason = if stale { "stale" } else { "policy" },
                                outcome = "deferred",
                                count = 1_u64,
                                "received cleanup deferred"
                            );
                        }
                    }
                }
            }
        }
        self.expire_legacy();
        self.prune_empty_received();
        let journal = self.received.lock().unwrap();
        if journal.save(&self.root).is_err() {
            tracing::warn!(reason = "journal_write", "received-file state not saved");
        }
    }
    fn full_and_pin(&self, path: &Path, length: u64) -> io::Result<bool> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | 0x40000)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut buffer = vec![0u64; 1024];
        let mut written = 0;
        // SAFETY: cfapi fills an aligned buffer whose size covers standard info.
        hr(unsafe {
            (self.api.info)(
                file.as_raw_handle(),
                CF_PLACEHOLDER_INFO_STANDARD,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut written,
            )
        })?;
        // SAFETY: the successful call initialized CF_PLACEHOLDER_STANDARD_INFO.
        let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
        if info.OnDiskDataSize < length as i64 {
            return Ok(false);
        }
        if info.PinState != CF_PIN_STATE_PINNED {
            // SAFETY: metadata-only handle has WRITE_DAC and stays live.
            hr(unsafe {
                (self.api.pin)(
                    file.as_raw_handle(),
                    CF_PIN_STATE_PINNED,
                    CF_SET_PIN_FLAG_NONE,
                    std::ptr::null_mut(),
                )
            })?;
        }
        Ok(true)
    }
    pub(super) fn received_observe(&self, id: u64, index: usize, reader: Reader, open: bool, deleted: bool) {
        let mut journal = self.received.lock().unwrap();
        let unbalanced = if !deleted {
            let balance = journal.opens.entry((id, index)).or_default();
            *balance += if open { 1 } else { -1 };
            *balance < 0
        } else {
            false
        };
        if !journal.deliveries.contains_key(&id) {
            journal.pending_gaps.insert(id);
        }
        if let Some(nodes) = journal.deliveries.get_mut(&id) {
            // HDROP can be requested during hover. Without a successful drop
            // notification, start the shell-copy deadline no earlier than the
            // first non-system content demand for this batch.
            if reader != Reader::System && nodes.iter().all(|n| !n.life.shell && !n.life.app && !n.life.unknown) {
                let delivered = received::now();
                for node in &mut *nodes {
                    node.life.received_at = node.life.received_at.max(delivered);
                    node.life.last_used = node.life.last_used.max(delivered);
                }
            }
            if reader == Reader::Unknown || unbalanced {
                for node in &mut *nodes {
                    node.life.gap(received::now());
                }
            }
            if let Some(node) = nodes.get_mut(index) {
                node.life.observe(reader, received::now());
                if deleted {
                    let path = node.path.clone();
                    for node in &mut *nodes {
                        if node.path.starts_with(&path) {
                            node.deleted = true;
                        }
                    }
                }
            }
        }
        // No disk, signature verification or native calls on cfapi threads.
        // A crash before the next checkpoint disqualifies the whole batch on load.
        tracing::debug!(
            reader = match reader {
                Reader::System => 0_u8,
                Reader::Shell => 1,
                Reader::App => 2,
                Reader::Unknown => 3,
            },
            count = 1_u64,
            "received reader observed"
        );
        crate::cache::changed(&self.root);
    }
}
pub(crate) fn received_usage() -> io::Result<Usage> {
    let service = CURRENT.lock().unwrap().upgrade().ok_or(io::ErrorKind::NotConnected)?;
    service.received_sweep();
    let deliveries = service.received.lock().unwrap().deliveries.clone();
    let mut usage = Usage {
        time_basis: Basis::LastUsed,
        cleanup: received::setting(),
        ..Default::default()
    };
    for (id, nodes) in &deliveries {
        for (index, node) in nodes.iter().enumerate().filter(|(_, n)| !n.deleted && !n.directory) {
            let path = service.root.join(&node.path);
            let identity = service.disk_identity(&path);
            if !identity
                .as_ref()
                .is_ok_and(|(owner, slot, _)| owner == id && *slot == index)
            {
                usage.incomplete = true;
                continue;
            }
            if !node.life.materialized {
                usage.incomplete |= identity.is_ok_and(|(_, _, full)| full);
                continue;
            }
            match std::fs::symlink_metadata(&path).and_then(|m| crate::cache::allocated(&path, &m)) {
                Ok(size) => usage.materialized_bytes += size,
                Err(_) => usage.incomplete = true,
            }
        }
    }
    tracing::debug!(
        materialized_bytes = usage.materialized_bytes,
        incomplete = usage.incomplete,
        "received usage"
    );
    Ok(usage)
}
pub(crate) fn open_received() -> io::Result<()> {
    let service = CURRENT.lock().unwrap().upgrade().ok_or(io::ErrorKind::NotConnected)?;
    std::process::Command::new("explorer.exe")
        .arg(&service.root)
        .spawn()
        .map(|_| ())
}
/// Fixed protected OS locations only; unknown helpers are retained, never
/// mistaken for Explorer. Preverification happens off native callback threads.
pub(super) fn reader_classes() -> HashMap<String, Reader> {
    let mut classes = HashMap::new();
    let windows = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let mut paths = vec![(windows.join("explorer.exe"), Reader::Shell)];
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        for exe in ["MsMpEng.exe", "MpDefenderCoreService.exe"] {
            paths.push((
                PathBuf::from(&programs).join("Windows Defender").join(exe),
                Reader::System,
            ));
        }
    }
    if let Some(data) = std::env::var_os("ProgramData")
        && let Ok(versions) = std::fs::read_dir(PathBuf::from(data).join(r"Microsoft\Windows Defender\Platform"))
    {
        for entry in versions.flatten() {
            if entry.metadata().is_ok_and(|m| m.is_dir() && !crate::cache::is_link(&m)) {
                for exe in ["MsMpEng.exe", "MpDefenderCoreService.exe"] {
                    paths.push((entry.path().join(exe), Reader::System));
                }
            }
        }
    }
    for name in ["SearchIndexer.exe", "SearchProtocolHost.exe", "SearchFilterHost.exe"] {
        paths.push((windows.join("System32").join(name), Reader::System));
    }
    for path in paths {
        if trusted(&path.0) {
            if let Some(native) = nt_path(&path.0) {
                classes.insert(native.to_lowercase(), path.1);
            }
            classes.insert(path.0.to_string_lossy().to_lowercase(), path.1);
        }
    }
    classes
}
fn nt_path(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    let drive = value.get(..2)?;
    let mut target = vec![0u16; 32768];
    // SAFETY: QueryDosDevice writes into the supplied live UTF-16 buffer.
    let length = unsafe { QueryDosDeviceW(wide(drive).as_ptr(), target.as_mut_ptr(), target.len() as u32) };
    if length == 0 {
        return None;
    }
    let end = target.iter().position(|c| *c == 0)?;
    Some(format!("{}{}", String::from_utf16_lossy(&target[..end]), &value[2..]))
}
fn trusted(path: &Path) -> bool {
    use windows_sys::Win32::Security::WinTrust::*;
    if std::fs::symlink_metadata(path).map_or(true, |m| crate::cache::is_link(&m)) {
        return false;
    }
    let name = wide(path);
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: name.as_ptr(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: initialized trust structures and path live throughout verification;
    // UI/network retrieval are disabled and the trust state is always closed.
    unsafe {
        let result = WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
        let microsoft = if result == 0 {
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            if provider.is_null() {
                false
            } else {
                let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
                if signer.is_null() || (*signer).csCertChain == 0 || (*signer).pasCertChain.is_null() {
                    false
                } else {
                    let certificate = (*(*signer).pasCertChain).pCert;
                    let mut name = [0u16; 256];
                    let count = windows_sys::Win32::Security::Cryptography::CertGetNameStringW(
                        certificate,
                        windows_sys::Win32::Security::Cryptography::CERT_NAME_SIMPLE_DISPLAY_TYPE,
                        0,
                        std::ptr::null(),
                        name.as_mut_ptr(),
                        name.len() as u32,
                    );
                    count > 0
                        && count as usize <= name.len()
                        && matches!(
                            String::from_utf16_lossy(&name[..count.saturating_sub(1) as usize]).as_str(),
                            "Microsoft Windows" | "Microsoft Corporation" | "Microsoft Windows Publisher"
                        )
                }
            }
        } else {
            false
        };
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
        microsoft
    }
}

/// Windows native names can differ from the configured root only by case.
/// Preserve the actual tail; textual strip_prefix would deny legitimate edits.
fn relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    let mut tail = path.components();
    for component in root.components() {
        let other = tail.next()?;
        let a = wide(component.as_os_str());
        let b = wide(other.as_os_str());
        // SAFETY: counted UTF-16 components remain live during ordinal comparison.
        if unsafe { CompareStringOrdinal(a.as_ptr(), (a.len() - 1) as i32, b.as_ptr(), (b.len() - 1) as i32, 1) }
            != CSTR_EQUAL
        {
            return None;
        }
    }
    Some(tail.collect())
}
/// Native callback paths are volume-rooted, not DOS absolute paths.
unsafe fn callback_path(info: &CF_CALLBACK_INFO, pointer: *const u16) -> Option<PathBuf> {
    unsafe fn string(pointer: *const u16) -> Option<String> {
        if pointer.is_null() {
            return None;
        }
        let mut n = 0;
        // SAFETY: the callback owns a terminated UTF-16 string for this call.
        unsafe {
            while n < 32768 && *pointer.add(n) != 0 {
                n += 1;
            }
            if n == 32768 {
                None
            } else {
                Some(String::from_utf16_lossy(std::slice::from_raw_parts(pointer, n)))
            }
        }
    }
    // SAFETY: cfapi supplies these two live strings.
    unsafe {
        let p = string(pointer)?;
        if p.starts_with(r"\Device\") {
            let native = nt_path(Path::new(&format!("{}\\", string(info.VolumeDosName)?)))?;
            return p
                .strip_prefix(native.trim_end_matches('\\'))
                .map(|tail| PathBuf::from(format!("{}{tail}", string(info.VolumeDosName).unwrap_or_default())));
        }
        Some(PathBuf::from(format!("{}{}", string(info.VolumeDosName)?, p)))
    }
}
pub(super) unsafe extern "system" fn renaming(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: cfapi owns callback/context/path pointers for this call.
    unsafe {
        let i = &*info;
        let Some(service) = (&*i.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let old = callback_path(i, i.NormalizedPath);
        let new = callback_path(i, (*params).Anonymous.Rename.TargetPath);
        let allowed = old.zip(new).is_some_and(|(old, new)| {
            let Some(old) = relative_path(&service.root, &old) else {
                return false;
            };
            let Some(new) = relative_path(&service.root, &new) else {
                return false;
            };
            old.components().count() > 2
                && new.components().count() > 2
                && old.components().take(2).eq(new.components().take(2))
                && new.components().all(|c| matches!(c, std::path::Component::Normal(_)))
        });
        let mut op = operation(i.ConnectionKey, i.TransferKey, i.RequestKey);
        op.Type = CF_OPERATION_TYPE_ACK_RENAME;
        let mut p = CF_OPERATION_PARAMETERS {
            ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_6>())
                as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                AckRename: CF_OPERATION_PARAMETERS_0_6 {
                    Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                    CompletionStatus: if allowed {
                        STATUS_SUCCESS
                    } else {
                        STATUS_CLOUD_FILE_ACCESS_DENIED
                    },
                },
            },
        };
        let _ = (service.api.execute)(&op, &mut p);
    }
}
pub(super) unsafe extern "system" fn renamed(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: both native paths are valid for this completion.
    unsafe {
        let i = &*info;
        let Some(service) = (&*i.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let Some((old, new)) =
            callback_path(i, (*params).Anonymous.RenameCompletion.SourcePath).zip(callback_path(i, i.NormalizedPath))
        else {
            return;
        };
        let Some(old) = relative_path(&service.root, &old) else {
            return;
        };
        let Some(new) = relative_path(&service.root, &new) else {
            return;
        };
        let mut journal = service.received.lock().unwrap();
        for node in journal.deliveries.values_mut().flatten() {
            if let Ok(tail) = node.path.strip_prefix(&old) {
                node.path = new.join(tail);
                node.life.observe(Reader::Unknown, received::now());
            }
        }
        crate::cache::changed(&service.root);
    }
}

fn exclusive_delete(path: &Path, eligible: impl FnOnce() -> bool) -> io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // Re-evaluate after obtaining the exclusive handle: no reader can sneak
    // between the last-used/classification decision and native deletion.
    if !eligible() {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the exclusive DELETE handle and fixed-size disposition are live.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    drop(file);
    Ok(())
}

impl Service {
    pub(super) fn create_container(&self, path: &Path, id: u64) -> io::Result<()> {
        let name = wide(path.file_name().ok_or(io::ErrorKind::InvalidInput)?);
        let mut identity = [0u8; 16];
        identity[..8].copy_from_slice(&id.to_le_bytes());
        identity[8..].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut info = CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: name.as_ptr(),
            FsMetadata: CF_FS_METADATA {
                BasicInfo: FILE_BASIC_INFO {
                    FileAttributes: FILE_ATTRIBUTE_DIRECTORY,
                    ..Default::default()
                },
                FileSize: 0,
            },
            FileIdentity: identity.as_ptr().cast(),
            FileIdentityLength: 16,
            Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
            ..Default::default()
        };
        let mut count = 0;
        // SAFETY: filename, identity and the single metadata entry live through create.
        hr(unsafe {
            (self.api.create)(
                wide(path.parent().ok_or(io::ErrorKind::InvalidInput)?).as_ptr(),
                &mut info,
                1,
                CF_CREATE_FLAG_STOP_ON_ERROR,
                &mut count,
            )
        })?;
        hr(info.Result)?;
        if count != 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
    pub(super) fn owned_container(&self, path: &Path, id: u64) -> io::Result<bool> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let mut buffer = [0u64; 1024];
        let mut returned = 0;
        // SAFETY: an aligned output buffer covers the standard-info header and identity.
        if unsafe {
            (self.api.info)(
                file.as_raw_handle(),
                CF_PLACEHOLDER_INFO_STANDARD,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut returned,
            )
        } < 0
        {
            return Ok(false);
        }
        // SAFETY: the successful standard-info call initialized these fields.
        let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
        if info.FileIdentityLength != 16 {
            return Ok(false);
        }
        // SAFETY: info reports a 16-byte identity inside our larger buffer.
        let identity = unsafe { std::slice::from_raw_parts(info.FileIdentity.as_ptr(), 16) };
        Ok(identity[..8] == id.to_le_bytes() && identity[8..] == u64::MAX.to_le_bytes())
    }
}
pub(super) unsafe fn deleted_path(info: *const CF_CALLBACK_INFO) {
    // SAFETY: all pointers refer to this cfapi notification.
    unsafe {
        let info = &*info;
        let Some(service) = (&*info.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let Some(path) = callback_path(info, info.NormalizedPath) else {
            return;
        };
        let Some(relative) = relative_path(&service.root, &path) else {
            return;
        };
        let mut changed = Vec::new();
        {
            let mut journal = service.received.lock().unwrap();
            for (id, nodes) in &mut journal.deliveries {
                for (index, node) in nodes.iter_mut().enumerate() {
                    if node.path.starts_with(&relative) {
                        node.deleted = true;
                        changed.push((*id, index));
                    }
                }
            }
        }
        let deliveries = service.deliveries.lock().unwrap();
        for (id, index) in changed {
            if let Some(delivery) = deliveries.get(&id)
                && let Some(flag) = delivery.deleted.get(index)
            {
                flag.store(true, Ordering::Release);
            }
        }
        tracing::info!(
            cleanup_reason = "user_delete",
            count = 1_u64,
            "received deletion observed"
        );
        crate::cache::changed(&service.root);
    }
}

impl Service {
    pub(super) fn native_identity_matches(&self, path: &Path, id: u64, index: usize) -> bool {
        self.disk_identity(path)
            .is_ok_and(|(owner, slot, _)| owner == id && slot == index)
    }
    fn disk_identity(&self, path: &Path) -> io::Result<(u64, usize, bool)> {
        let meta = std::fs::symlink_metadata(path)?;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let mut buffer = [0u64; 1024];
        let mut returned = 0;
        // SAFETY: aligned output covers the header and our fixed 16-byte identity.
        hr(unsafe {
            (self.api.info)(
                file.as_raw_handle(),
                CF_PLACEHOLDER_INFO_STANDARD,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut returned,
            )
        })?;
        // SAFETY: info was initialized by the successful call above.
        let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
        if info.FileIdentityLength != 16 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the identity is inside the larger native output allocation.
        let bytes = unsafe { std::slice::from_raw_parts(info.FileIdentity.as_ptr(), 16) };
        Ok((
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            u64::from_le_bytes(bytes[8..].try_into().unwrap()) as usize,
            info.OnDiskDataSize >= meta.len() as i64,
        ))
    }
    /// Runs once before publishing CURRENT. Only an owned batch identity permits
    /// recovery; arbitrary user files and reparse targets are never traversed.
    pub(super) fn recover_received(&self) -> io::Result<()> {
        for source in std::fs::read_dir(&self.root)?.flatten() {
            if !source.file_type()?.is_dir() && !crate::cache::is_link(&source.metadata()?) {
                continue;
            }
            let source_path = source.path();
            if !self
                .disk_identity(&source_path)
                .is_ok_and(|(_, index, _)| index == usize::MAX)
            {
                continue;
            }
            for batch in std::fs::read_dir(&source_path)?.flatten() {
                let batch_path = batch.path();
                let Ok((id, index, _)) = self.disk_identity(&batch_path) else {
                    continue;
                };
                if index != usize::MAX {
                    continue;
                }
                let mut found = BTreeMap::new();
                let mut queue = vec![batch_path.clone()];
                while let Some(parent) = queue.pop() {
                    for child in std::fs::read_dir(parent)?.flatten() {
                        let path = child.path();
                        let meta = std::fs::symlink_metadata(&path)?;
                        let Ok((owner, index, full)) = self.disk_identity(&path) else {
                            continue;
                        };
                        if owner != id || index >= crate::model::MAX_DISCOVERED_NODES {
                            continue;
                        }
                        let directory = meta.is_dir();
                        if directory {
                            queue.push(path.clone());
                        }
                        let mut life = Item::new(received::now());
                        life.gap(received::now());
                        life.materialized = full && !directory;
                        found.insert(
                            index,
                            Node {
                                path: path.strip_prefix(&self.root).map_err(io::Error::other)?.into(),
                                life,
                                directory,
                                deleted: false,
                                handed_off: full && !directory,
                                length: meta.len(),
                            },
                        );
                    }
                }
                if found.is_empty() {
                    // No native children: safe empty-only orphan rollback.
                    if !self.received.lock().unwrap().deliveries.contains_key(&id) {
                        let _ = std::fs::remove_dir(&batch_path);
                    }
                } else {
                    self.received.lock().unwrap().merge_discovered(id, found);
                }
            }
        }
        self.received.lock().unwrap().save(&self.root)?;
        self.received_sweep();
        Ok(())
    }
    /// Pre-release random batches keep their old 24h deadline. Only matching
    /// native Cloud identities are removable; foreign files prevent rmdir.
    fn expire_legacy(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.len() != 16 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let Ok(id) = u64::from_str_radix(name, 16) else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.is_dir()
                || crate::cache::is_link(&meta)
                || !meta
                    .created()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() >= cloud_files::RETENTION_SECS)
            {
                continue;
            }
            let mut queue = vec![entry.path()];
            let mut dirs = Vec::new();
            while let Some(parent) = queue.pop() {
                dirs.push(parent.clone());
                let Ok(children) = std::fs::read_dir(parent) else {
                    continue;
                };
                for child in children.flatten() {
                    let path = child.path();
                    let Ok((owner, index, _)) = self.disk_identity(&path) else {
                        continue;
                    };
                    if owner != id || index >= crate::model::MAX_DISCOVERED_NODES {
                        continue;
                    }
                    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                        queue.push(path);
                    } else if exclusive_delete(&path, || true).is_ok() {
                        tracing::info!(cleanup_reason = "old_layout", count = 1_u64, "received file removed");
                    }
                }
            }
            dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for path in dirs {
                let _ = std::fs::remove_dir(path);
            }
        }
    }
    fn prune_empty_received(&self) {
        let deliveries = self.received.lock().unwrap().deliveries.clone();
        for nodes in deliveries.values() {
            // Empty directory deliveries are received items too; preserve them
            // by default and apply the explicit age policy without recursion.
            if nodes.iter().all(|n| n.directory)
                && nodes.iter().any(|n| {
                    !n.deleted && received::setting().days() == 0
                        || !n.deleted
                            && received::now().saturating_sub(n.life.last_used)
                                < received::setting().days() * received::DAY
                })
            {
                continue;
            }
            if nodes.iter().any(|n| !n.directory && !n.deleted) {
                continue;
            }
            let mut dirs: Vec<_> = nodes
                .iter()
                .filter(|n| n.directory)
                .map(|n| self.root.join(&n.path))
                .collect();
            if let Some(node) = nodes.first() {
                let batch: PathBuf = node.path.components().take(2).collect();
                dirs.push(self.root.join(batch));
            }
            dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            dirs.dedup();
            for path in dirs {
                let _ = std::fs::remove_dir(path);
            }
        }
    }
}

/// CF sync roots are reparse points too. Reject redirection tags, not Cloud tags.
pub(super) fn valid_root(path: &Path) -> io::Result<bool> {
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the handle and fixed-size native output remain live.
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
    // IO_REPARSE_TAG_CLOUD and its versioned variants share all but bits 12..15.
    Ok(info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
        && (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 || info.ReparseTag & !0xf000 == 0x9000001a))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_paths_preserve_leaf_case_and_reject_neighboring_roots() {
        let root = Path::new(r"C:\fixture\received");
        assert_eq!(
            relative_path(root, Path::new(r"c:\FIXTURE\Received\source\batch\Leaf")),
            Some(PathBuf::from(r"source\batch\Leaf"))
        );
        assert!(relative_path(root, Path::new(r"C:\fixture\received-other\file")).is_none());
        let volume = wide("C:");
        let normalized = wide(r"\fixture\received\source\batch\Leaf");
        let info = CF_CALLBACK_INFO {
            VolumeDosName: volume.as_ptr(),
            ..Default::default()
        };
        // SAFETY: both counted/terminated native strings live through the helper.
        let actual = unsafe { callback_path(&info, normalized.as_ptr()) };
        assert_eq!(actual, Some(PathBuf::from(r"C:\fixture\received\source\batch\Leaf")));
    }
    #[test]
    fn journal_restart_discards_open_balances_but_preserves_items_and_disqualifies_shell_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("CloudFiles");
        let mut life = Item::new(1);
        life.materialized = true;
        life.observe(Reader::Shell, 2);
        let node = Node {
            path: PathBuf::from(r"device\batch\file"),
            life,
            directory: false,
            deleted: false,
            handed_off: true,
            length: 4,
        };
        let mut journal = Journal::default();
        journal.deliveries.insert(7, vec![node]);
        journal.opens.insert((7, 0), 1);
        journal.save(&root).unwrap();
        let recovered = Journal::load(&root).unwrap();
        assert!(recovered.opens.is_empty());
        let item = &recovered.deliveries[&7][0].life;
        assert!(item.materialized && item.observation_gap);
        assert!(!item.shell_only());
        assert!(item.last_used >= 2);
        journal.deliveries.get_mut(&7).unwrap()[0].path = PathBuf::from(r"..\foreign");
        journal.save(&root).unwrap();
        assert_eq!(Journal::load(&root).err().unwrap().kind(), io::ErrorKind::InvalidData);
    }
    #[test]
    fn explorer_identity_is_verified_without_a_desktop_or_shell_registration() {
        let classes = reader_classes();
        let explorer = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("explorer.exe");
        assert_eq!(
            classes.get(&explorer.to_string_lossy().to_lowercase()),
            Some(&Reader::Shell)
        );
        let temp = tempfile::tempdir().unwrap();
        let imposter = temp.path().join("explorer.exe");
        std::fs::write(&imposter, b"not a signed executable").unwrap();
        assert!(!trusted(&imposter));
    }
}
