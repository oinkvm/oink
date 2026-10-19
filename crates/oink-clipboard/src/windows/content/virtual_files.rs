//! Source virtual files stay in the source COM apartment. At crossing, missing
//! sizes query FileContents.GetData + IStream.Stat without Read/Seek; third-party
//! GetData may itself render or block. Real Drop snapshots every FileContents
//! before releasing IDataObject. No COM pointer enters a worker.
use super::*;
use crate::content::namespace::{self, Namespace};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::{Condvar, Mutex},
};

#[derive(Clone, PartialEq, Eq)]
struct Descriptor {
    path: String,
    directory: bool,
    size: ContentSize,
    modified_ms: Option<i64>,
}
type StoredFiles = (Batch, Arc<Vec<FileNode>>);
pub(crate) struct Virtual {
    descriptors: Vec<Descriptor>,
    nodes: Vec<FileNode>,
    indices: Vec<(NodeId, usize)>,
    ready: Condvar,
    state: Mutex<Option<Result<StoredFiles, Unavailable>>>,
}
impl Virtual {
    pub fn metadata(data: &IDataObject) -> io::Result<Option<Arc<Self>>> {
        let Some(descriptors) = descriptors(data)? else {
            return Ok(None);
        };
        let mut nodes: Vec<FileNode> = Vec::new();
        let mut paths = HashMap::<String, usize>::new();
        let mut indices = Vec::new();
        let mut explicit = std::collections::HashSet::new();
        for (index, descriptor) in descriptors.iter().enumerate() {
            let components: Vec<_> = descriptor.path.split('/').collect();
            if components.len() > 256 {
                return Err(content::unavailable(Unavailable::LimitExceeded));
            }
            let mut parent = None;
            let mut path = String::new();
            for (position, component) in components.iter().enumerate() {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(component);
                let last = position + 1 == components.len();
                let kind = if last && !descriptor.directory {
                    NodeKind::File
                } else {
                    NodeKind::Directory
                };
                let at = if let Some(at) = paths.get(&path).copied() {
                    if nodes[at].kind != kind || (last && !explicit.insert(path.clone())) {
                        return Err(content::unavailable(Unavailable::InvalidData));
                    }
                    at
                } else {
                    if nodes.len() >= MAX_DISCOVERED_NODES {
                        return Err(content::unavailable(Unavailable::LimitExceeded));
                    }
                    let at = nodes.len();
                    let entry = FileNode {
                        id: NodeId(at as u64 + 1),
                        parent,
                        name: (*component).to_owned(),
                        kind,
                        size: ContentSize::Unknown,
                        modified_ms: None,
                        version: 0,
                    };
                    if !entry.valid() {
                        return Err(content::unavailable(Unavailable::InvalidData));
                    }
                    nodes.push(entry);
                    paths.insert(path.clone(), at);
                    if last {
                        explicit.insert(path.clone());
                    }
                    at
                };
                if last {
                    nodes[at].size = if kind == NodeKind::File && descriptor.size == ContentSize::Unknown {
                        stream_size(data, index).map_or(ContentSize::Unknown, ContentSize::Known)
                    } else {
                        descriptor.size
                    };
                    nodes[at].modified_ms = descriptor.modified_ms;
                    if kind == NodeKind::File {
                        indices.push((nodes[at].id, index));
                    }
                }
                parent = Some(nodes[at].id);
            }
        }
        if crate::wire::nodes_size(&nodes).map_err(|_| content::unavailable(Unavailable::InvalidData))?
            > 64 * 1024 * 1024
        {
            return Err(content::unavailable(Unavailable::LimitExceeded));
        }
        let value = Arc::new(Self {
            descriptors,
            nodes,
            indices,
            ready: Condvar::new(),
            state: Mutex::new(None),
        });
        if crate::wire::nodes_size(&value.roots()).map_err(|_| content::unavailable(Unavailable::InvalidData))?
            > INVENTORY_BYTES
        {
            return Err(content::unavailable(Unavailable::LimitExceeded));
        }
        Ok(Some(value))
    }
    /// Clipboard virtual files retain descriptors only. Unlike a drag's real Drop,
    /// every content request reacquires the current IDataObject on a reader STA.
    pub fn current(
        self: Arc<Self>,
        worker: Arc<super::super::worker::Worker>,
        owner: Arc<super::super::worker::Worker>,
        identity: Arc<current::Identity>,
        inventory: Inventory,
    ) -> Arc<dyn Namespace> {
        Arc::new(Current {
            files: self,
            worker,
            owner,
            identity,
            inventory,
            sizes: Mutex::new(HashMap::new()),
        })
    }
    pub fn snapshot(&self, data: &IDataObject, root: &Path) -> io::Result<()> {
        let result = self.capture(data, root);
        let mut state = self.state.lock().unwrap();
        if state.is_some() {
            return Err(content::unavailable(Unavailable::Cancelled));
        }
        *state = Some(result.map_err(|error| content::reason(&error)));
        self.ready.notify_all();
        state
            .as_ref()
            .unwrap()
            .as_ref()
            .map(|_| ())
            .map_err(|r| content::unavailable(*r))
    }
    fn capture(&self, data: &IDataObject, root: &Path) -> io::Result<(Batch, Arc<Vec<FileNode>>)> {
        if descriptors(data)?.as_ref() != Some(&self.descriptors) {
            return Err(content::unavailable(Unavailable::Superseded));
        }
        let batch = Batch::create(root, StoreKind::SourceSnapshot, BatchId::random()?)?;
        let mut nodes = self.nodes.clone();
        let cf = format_id(&NativeFormat::WindowsRegistered("FileContents".into())).ok_or_else(failed)?;
        for (node, index) in &self.indices {
            self.live()?;
            // Match the advertised length, including Stat-discovered metadata;
            // it also excludes allocator padding when GetData returns HGLOBAL.
            let expected = self.nodes[node.0 as usize - 1].size;
            let format = FORMATETC {
                cfFormat: cf,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: *index as i32,
                tymed: (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0 | TYMED_FILE.0 | TYMED_ISTORAGE.0) as u32,
                ..Default::default()
            };
            // SAFETY: IDataObject and every returned medium stay on this apartment.
            // SAFETY: the caller owns data on this apartment; medium is released below.
            let mut medium = unsafe { data.GetData(&format) }.map_err(|_| failed())?;
            let result = (|| {
                let path = batch.path().join(format!("f-{}.part", node.0));
                // SAFETY: the returned STGMEDIUM stays live on this apartment;
                // each adapter checks tymed before accessing its union member.
                let size = unsafe {
                    if medium.tymed == TYMED_ISTORAGE.0 as u32 {
                        self.copy_storage(&medium, &path)?
                    } else {
                        let mut file = File::create(&path)?;
                        let size = self.copy_medium(&medium, &mut file, expected)?;
                        file.sync_all()?;
                        size
                    }
                };
                if let ContentSize::Known(expected) = expected
                    && expected != size
                {
                    return Err(content::unavailable(Unavailable::Superseded));
                }
                batch.commit_file(*node)?;
                let entry = &mut nodes[node.0 as usize - 1];
                entry.size = ContentSize::Known(size);
                entry.version = 1;
                Ok(())
            })();
            // SAFETY: release exactly once after every success/error of the adapter.
            unsafe {
                ReleaseStgMedium(&mut medium);
            }
            if let Err(error) = result {
                let _ = batch.abandon();
                return Err(error);
            }
        }
        Ok((batch, Arc::new(nodes)))
    }
    fn live(&self) -> io::Result<()> {
        if matches!(*self.state.lock().unwrap(), Some(Err(_))) {
            Err(content::unavailable(Unavailable::Cancelled))
        } else {
            Ok(())
        }
    }
    unsafe fn copy_storage(&self, medium: &STGMEDIUM, path: &Path) -> io::Result<u64> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::System::Com::StructuredStorage::StgCreateDocfile;
        let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: caller established TYMED_ISTORAGE, retains it through CopyTo;
        // this fresh batch path and its wide string live through synchronous I/O.
        unsafe {
            self.live()?;
            let source = medium.u.pstg.as_ref().ok_or_else(failed)?;
            let target = StgCreateDocfile(
                PCWSTR(wide.as_ptr()),
                STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
                None,
            )
            .map_err(|_| failed())?;
            source.CopyTo(None, None, &target).map_err(|_| failed())?;
            target.Commit(STGC_DEFAULT.0 as u32).map_err(|_| failed())?;
            drop(target);
            self.live()?;
        }
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        file.sync_all()?;
        Ok(file.metadata()?.len())
    }
    unsafe fn copy_medium(&self, medium: &STGMEDIUM, output: &mut dyn Write, expected: ContentSize) -> io::Result<u64> {
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        // SAFETY: caller owns the native medium on this apartment. Union access
        // follows its tymed, stream buffers are bounded, and global locks balance.
        unsafe {
            match medium.tymed {
                value if value == TYMED_ISTREAM.0 as u32 => {
                    let stream = medium.u.pstm.as_ref().ok_or_else(failed)?;
                    let mut bytes = vec![0; 1024 * 1024];
                    let mut total = 0u64;
                    loop {
                        self.live()?;
                        let mut count = 0;
                        stream
                            .Read(bytes.as_mut_ptr().cast(), bytes.len() as u32, Some(&mut count))
                            .ok()
                            .map_err(|_| failed())?;
                        if count as usize > bytes.len() {
                            return Err(failed());
                        }
                        if count == 0 {
                            return Ok(total);
                        }
                        output.write_all(&bytes[..count as usize])?;
                        total = total.checked_add(count as u64).ok_or_else(failed)?;
                    }
                }
                value if value == TYMED_HGLOBAL.0 as u32 => {
                    let handle = medium.u.hGlobal;
                    let allocated = GlobalSize(handle);
                    let size = match expected {
                        ContentSize::Known(n) => {
                            usize::try_from(n).ok().filter(|n| *n <= allocated).ok_or_else(failed)?
                        }
                        ContentSize::Unknown => allocated,
                    };
                    if size == 0 {
                        return Ok(0);
                    }
                    let pointer = GlobalLock(handle);
                    if pointer.is_null() {
                        return Err(failed());
                    }
                    let result = (|| {
                        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size);
                        for chunk in bytes.chunks(1024 * 1024) {
                            self.live()?;
                            output.write_all(chunk)?;
                        }
                        Ok(size as u64)
                    })();
                    let _ = GlobalUnlock(handle);
                    result
                }
                value if value == TYMED_FILE.0 as u32 => {
                    let path = medium.u.lpszFileName.to_string().map_err(|_| failed())?;
                    let mut source = File::open(path)?;
                    let mut bytes = vec![0; 1024 * 1024];
                    let mut total = 0u64;
                    loop {
                        self.live()?;
                        let n = source.read(&mut bytes)?;
                        if n == 0 {
                            return Ok(total);
                        }
                        output.write_all(&bytes[..n])?;
                        total = total.checked_add(n as u64).ok_or_else(failed)?;
                    }
                }
                _ => Err(content::unavailable(Unavailable::Unsupported)),
            }
        }
    }
    fn stored(&self) -> io::Result<(Batch, Arc<Vec<FileNode>>)> {
        let mut state = self.state.lock().unwrap();
        while state.is_none() {
            state = self.ready.wait(state).unwrap();
        }
        state.as_ref().unwrap().clone().map_err(content::unavailable)
    }
}
impl Namespace for Virtual {
    fn roots(&self) -> Vec<FileNode> {
        self.nodes.iter().filter(|n| n.parent.is_none()).cloned().collect()
    }
    fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.live()?;
        if self
            .nodes
            .iter()
            .find(|n| n.id == id)
            .is_none_or(|n| n.kind != NodeKind::Directory)
        {
            return Err(content::unavailable(Unavailable::NoSuchItem));
        }
        let entries = self
            .nodes
            .iter()
            .filter(|n| n.parent == Some(id))
            .cloned()
            .collect::<Vec<_>>();
        namespace::page(id, 1, &entries, cursor)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        let (_, nodes) = self.stored()?;
        nodes
            .get(
                id.0.checked_sub(1)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(failed)?,
            )
            .filter(|n| n.id == id && n.kind == NodeKind::File)
            .cloned()
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        let (batch, nodes) = self.stored()?;
        let node = nodes
            .get(
                id.0.checked_sub(1)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(failed)?,
            )
            .filter(|n| n.id == id && n.kind == NodeKind::File && n.version == version)
            .ok_or_else(|| content::unavailable(Unavailable::Superseded))?;
        let ContentSize::Known(size) = node.size else {
            return Err(failed());
        };
        if offset > size {
            return Err(failed());
        }
        let mut file = File::open(batch.path().join(format!("f-{}", id.0)))?;
        if file.metadata()?.len() != size {
            return Err(content::unavailable(Unavailable::Superseded));
        }
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(Reader { file, _batch: batch }))
    }
    fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        *state = Some(Err(Unavailable::Cancelled));
        self.ready.notify_all();
    }
}
struct Current {
    files: Arc<Virtual>,
    worker: Arc<super::super::worker::Worker>,
    owner: Arc<super::super::worker::Worker>,
    identity: Arc<current::Identity>,
    inventory: Inventory,
    sizes: Mutex<HashMap<NodeId, u64>>,
}
impl Current {
    fn entry(&self, id: NodeId) -> io::Result<FileNode> {
        self.files
            .nodes
            .iter()
            .find(|n| n.id == id && n.kind == NodeKind::File)
            .cloned()
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))
    }
}
impl Namespace for Current {
    fn roots(&self) -> Vec<FileNode> {
        self.files.roots()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.identity.check()?;
        self.files.list(node, cursor)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        self.identity.check()?;
        let mut node = self.entry(id)?;
        let cached = self.sizes.lock().unwrap().get(&id).copied();
        let size = if let Some(size) = cached {
            size
        } else {
            let files = self.files.clone();
            let identity = self.identity.clone();
            let inventory = self.inventory.clone();
            let owner = self.owner.clone();
            let size = self
                .worker
                .run(move |_| {
                    let result = identity.read(&inventory, |data| {
                        files.with_medium(data, id, |value, expected| {
                            if let ContentSize::Known(size) = expected {
                                return Ok(size);
                            }
                            // SAFETY: with_medium owns this STGMEDIUM on its STA. Stat can
                            // discover a lazy stream length without consuming/storing it.
                            unsafe {
                                if value.tymed == TYMED_ISTREAM.0 as u32 {
                                    let mut stat = STATSTG::default();
                                    if value
                                        .u
                                        .pstm
                                        .as_ref()
                                        .ok_or_else(failed)?
                                        .Stat(&mut stat, STATFLAG_NONAME)
                                        .is_ok()
                                    {
                                        return Ok(stat.cbSize);
                                    }
                                }
                            }
                            files.copy_current(value, &mut io::sink(), expected)
                        })
                    });
                    owner.notify();
                    result
                })
                .map_err(|_| failed())??;
            self.sizes.lock().unwrap().insert(id, size);
            size
        };
        node.size = ContentSize::Known(size);
        node.version = self.identity_version();
        Ok(node)
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        self.identity.check()?;
        let node = self.select(id)?;
        let ContentSize::Known(size) = node.size else {
            return Err(failed());
        };
        if version != node.version || offset > size {
            return Err(content::clipboard::changed());
        }
        let (mut output, input) = current::pipe(offset);
        let files = self.files.clone();
        let identity = self.identity.clone();
        let inventory = self.inventory.clone();
        let owner = self.owner.clone();
        self.worker
            .post(move |_| {
                let acquired = identity.read(&inventory, |data| files.get_medium(data, id));
                // Validate/advance the render identity before streaming. Otherwise
                // the reader would mistake this GetData bump for a new copy.
                owner.notify();
                let result = acquired.and_then(|value| {
                    let copied = files.copy_current(&value.value, &mut output, value.expected)?;
                    identity.check()?;
                    if copied != size {
                        return Err(content::clipboard::changed());
                    }
                    Ok(())
                });
                output.finish(result);
            })
            .map_err(|_| failed())?;
        Ok(input)
    }
}
impl Current {
    fn identity_version(&self) -> u64 {
        self.inventory.id.generation.max(1)
    }
}
impl Virtual {
    fn with_medium<T>(
        &self,
        data: &IDataObject,
        id: NodeId,
        use_medium: impl FnOnce(&STGMEDIUM, ContentSize) -> io::Result<T>,
    ) -> io::Result<T> {
        let value = self.get_medium(data, id)?;
        use_medium(&value.value, value.expected)
    }
    fn get_medium(&self, data: &IDataObject, id: NodeId) -> io::Result<CurrentMedium> {
        if descriptors(data)?.as_ref() != Some(&self.descriptors) {
            return Err(content::clipboard::changed());
        }
        let index = self
            .indices
            .iter()
            .find(|(node, _)| *node == id)
            .map(|(_, index)| *index)
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
        let format = FORMATETC {
            cfFormat: format_id(&NativeFormat::WindowsRegistered("FileContents".into())).ok_or_else(failed)?,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: index as i32,
            tymed: (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0 | TYMED_FILE.0 | TYMED_ISTORAGE.0) as u32,
            ..Default::default()
        };
        // SAFETY: acquired and released on one STA, including all failure paths.
        let value = unsafe { data.GetData(&format) }.map_err(|_| failed())?;
        Ok(CurrentMedium {
            value,
            expected: self.descriptors[index].size,
        })
    }

    fn copy_current(&self, value: &STGMEDIUM, output: &mut dyn Write, expected: ContentSize) -> io::Result<u64> {
        // SAFETY: called only within with_medium; the medium remains apartment-local.
        unsafe {
            if value.tymed == TYMED_ISTORAGE.0 as u32 {
                let bytes = medium::serialize(value, Medium::Storage)?;
                output.write_all(&bytes)?;
                Ok(bytes.len() as u64)
            } else {
                self.copy_medium(value, output, expected)
            }
        }
    }
}

struct CurrentMedium {
    value: STGMEDIUM,
    expected: ContentSize,
}
impl Drop for CurrentMedium {
    fn drop(&mut self) {
        // SAFETY: never leaves its reader STA; every acquired medium releases once.
        unsafe {
            ReleaseStgMedium(&mut self.value);
        }
    }
}

struct Reader {
    file: File,
    _batch: Batch,
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}
fn failed() -> io::Error {
    content::unavailable(Unavailable::ReadFailed)
}
fn stream_size(data: &IDataObject, index: usize) -> Option<u64> {
    // FD_FILESIZE is optional: a virtual source can omit it even when its
    // FileContents stream already knows the length (observed on Windows 11).
    // Publish that length before macOS creates a File Provider URL: stat-first
    // readers treat the unknown-size placeholder's reported zero as an empty
    // file and never read it to trigger hydration (observed on macOS 26).
    // Stat does not consume/seek the stream; failure stays Unknown. GlobalSize
    // cannot substitute here because HGLOBAL allocation may include padding.
    let format = FORMATETC {
        cfFormat: format_id(&NativeFormat::WindowsRegistered("FileContents".into()))?,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index as i32,
        tymed: TYMED_ISTREAM.0 as u32,
        ..Default::default()
    };
    let medium = CurrentMedium {
        // SAFETY: this IDataObject and the acquired medium stay on the caller's
        // COM apartment; CurrentMedium releases once on every return path.
        value: unsafe { data.GetData(&format) }.ok()?,
        expected: ContentSize::Unknown,
    };
    if medium.value.tymed != TYMED_ISTREAM.0 as u32 {
        return None;
    }
    // SAFETY: the checked stream medium remains owned until Stat returns.
    // NONAME avoids allocating a name; no COM pointer leaves this apartment.
    unsafe {
        let mut stat = STATSTG::default();
        medium.value.u.pstm.as_ref()?.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        Some(stat.cbSize)
    }
}
fn descriptors(data: &IDataObject) -> io::Result<Option<Vec<Descriptor>>> {
    for (name, wide, stride) in [("FileGroupDescriptorW", true, 592), ("FileGroupDescriptor", false, 332)] {
        let cf = format_id(&NativeFormat::WindowsRegistered(name.into())).ok_or_else(failed)?;
        let format = FORMATETC {
            cfFormat: cf,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        // SAFETY: apartment-owned IDataObject and a live descriptor FORMATETC.
        if unsafe { data.QueryGetData(&format) }.is_err() {
            continue;
        }
        // SAFETY: the caller owns data on this apartment; medium is released below.
        let mut medium = unsafe { data.GetData(&format) }.map_err(|_| failed())?;
        let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            // SAFETY: tymed has been checked and the medium is still owned here.
            unsafe { medium::global(medium.u.hGlobal) }
        } else {
            Err(failed())
        };
        // SAFETY: GetData transferred this medium to us; release once on both paths.
        unsafe {
            ReleaseStgMedium(&mut medium);
        }
        let bytes = bytes?;
        if bytes.len() < 4 || bytes.len() > INVENTORY_BYTES {
            return Err(content::unavailable(Unavailable::LimitExceeded));
        }
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        if count > 65535 || 4 + count * stride > bytes.len() {
            return Err(content::unavailable(Unavailable::InvalidData));
        }
        let mut descriptors = Vec::with_capacity(count);
        for entry in bytes[4..4 + count * stride].chunks_exact(stride) {
            let u32_at = |offset| u32::from_le_bytes(entry[offset..offset + 4].try_into().unwrap());
            let flags = u32_at(0);
            let name = if wide {
                let units: Vec<_> = entry[72..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect();
                let end = units.iter().position(|u| *u == 0).ok_or_else(failed)?;
                String::from_utf16(&units[..end]).map_err(|_| failed())?
            } else {
                let end = entry[72..].iter().position(|b| *b == 0).ok_or_else(failed)?;
                super::super::ansi_to_string(&entry[72..72 + end])
            }
            .replace('\\', "/");
            let directory = flags & 4 != 0 && u32_at(36) & 0x10 != 0;
            let size = if !directory && flags & 0x40 != 0 {
                ContentSize::Known(((u32_at(64) as u64) << 32) | u32_at(68) as u64)
            } else {
                ContentSize::Unknown
            };
            let modified_ms = (flags & 0x20 != 0).then(|| {
                let ticks = ((u32_at(60) as u64) << 32) | u32_at(56) as u64;
                ((ticks as i128 / 10_000) - 11_644_473_600_000).clamp(i64::MIN as i128, i64::MAX as i128) as i64
            });
            descriptors.push(Descriptor {
                path: name,
                directory,
                size,
                modified_ms,
            });
        }
        return Ok(Some(descriptors));
    }
    Ok(None)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::{FileContents, FileMetadata};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Measured {
        stat_size: Mutex<Option<u64>>,
        actual_size: usize,
        opens: AtomicUsize,
        stats: AtomicUsize,
    }
    impl FileContents for Measured {
        fn open(&self, _: usize) -> io::Result<crate::FileReader> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(io::Cursor::new(vec![7; self.actual_size])))
        }
        fn metadata(&self, _: usize) -> io::Result<Option<FileMetadata>> {
            self.stats.fetch_add(1, Ordering::SeqCst);
            Ok(Some(FileMetadata {
                size: self
                    .stat_size
                    .lock()
                    .unwrap()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?,
                modified_ms: None,
            }))
        }
    }
    // The fixture exposes a real apartment-local IDataObject/IStream. Descriptor
    // flags and Stat are independent, just as they are in a foreign OLE source.
    fn measured_data(
        descriptor_size: Option<u64>,
        stat_size: Option<u64>,
        actual_size: usize,
    ) -> (IDataObject, Arc<Measured>) {
        let contents = Arc::new(Measured {
            stat_size: Mutex::new(stat_size),
            actual_size,
            opens: AtomicUsize::new(0),
            stats: AtomicUsize::new(0),
        });
        let entries = vec![crate::FileEntry {
            path: "sample.png".into(),
            is_dir: false,
            size: descriptor_size.unwrap_or(0),
            modified_ms: None,
            local_path: None,
        }];
        let mut descriptors = super::super::super::files::descriptors(&entries);
        if descriptor_size.is_none() {
            let flags = u32::from_le_bytes(descriptors.bytes[4..8].try_into().unwrap()) & !0x40;
            descriptors.bytes[4..8].copy_from_slice(&flags.to_le_bytes());
        }
        let (data, _) = super::super::super::vfile::build(
            super::super::super::vfile::Spec {
                cloud: None,
                descriptors,
                fixed: Vec::new(),
                contents: contents.clone(),
                cf_descriptor: format_id(&NativeFormat::WindowsRegistered("FileGroupDescriptorW".into())).unwrap()
                    as u32,
                cf_contents: format_id(&NativeFormat::WindowsRegistered("FileContents".into())).unwrap() as u32,
            },
            true,
        );
        (data, contents)
    }
    #[test]
    fn missing_descriptor_size_uses_stream_stat_without_reading_contents() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: isolated STA; the data object and its media are released before uninitializing.
            unsafe { OleInitialize(None).unwrap() };
            for size in [2088, 0, u32::MAX as u64 + 9] {
                let (data, contents) = measured_data(None, Some(size), 2088);
                assert_eq!(descriptors(&data).unwrap().unwrap()[0].size, ContentSize::Unknown);
                let source = Virtual::metadata(&data).unwrap().unwrap();
                assert_eq!(source.roots()[0].size, ContentSize::Known(size));
                assert_eq!(
                    contents.opens.load(Ordering::SeqCst),
                    0,
                    "metadata consumed file contents"
                );
                assert_eq!(contents.stats.load(Ordering::SeqCst), 1);
            }
            // SAFETY: all apartment-owned COM objects have been released.
            unsafe { OleUninitialize() };
        })
        .join()
        .unwrap();
    }
    #[test]
    fn descriptor_size_is_authoritative_and_failed_stat_stays_unknown() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: isolated STA; all test COM objects drop before OleUninitialize.
            unsafe { OleInitialize(None).unwrap() };
            for size in [0, 41] {
                let (data, contents) = measured_data(Some(size), Some(2088), 2088);
                let source = Virtual::metadata(&data).unwrap().unwrap();
                assert_eq!(source.roots()[0].size, ContentSize::Known(size));
                assert_eq!(contents.stats.load(Ordering::SeqCst), 0);
                assert_eq!(contents.opens.load(Ordering::SeqCst), 0);
            }
            let (data, contents) = measured_data(None, None, 17);
            let source = Virtual::metadata(&data).unwrap().unwrap();
            assert_eq!(source.roots()[0].size, ContentSize::Unknown);
            assert_eq!(contents.opens.load(Ordering::SeqCst), 0);
            *contents.stat_size.lock().unwrap() = Some(17);
            let root = tempfile::tempdir().unwrap();
            source.snapshot(&data, root.path()).unwrap();
            drop(data);
            assert_eq!(source.select(NodeId(1)).unwrap().size, ContentSize::Known(17));
            // SAFETY: all apartment-owned COM objects have been released.
            unsafe { OleUninitialize() };
        })
        .join()
        .unwrap();
    }
    #[test]
    fn snapshot_rejects_contents_that_disagree_with_stream_stat() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: isolated STA; all test COM objects drop before OleUninitialize.
            unsafe { OleInitialize(None).unwrap() };
            let (data, contents) = measured_data(None, Some(2088), 2089);
            let source = Virtual::metadata(&data).unwrap().unwrap();
            *contents.stat_size.lock().unwrap() = Some(2089);
            let root = tempfile::tempdir().unwrap();
            assert_eq!(
                content::reason(&source.snapshot(&data, root.path()).unwrap_err()),
                Unavailable::Superseded
            );
            drop(data);
            // SAFETY: all apartment-owned COM objects have been released.
            unsafe { OleUninitialize() };
        })
        .join()
        .unwrap();
    }
    struct Memory;
    impl FileContents for Memory {
        fn descriptor_size_known(&self, _: usize) -> bool {
            false
        }
        fn open(&self, index: usize) -> io::Result<crate::FileReader> {
            Ok(Box::new(io::Cursor::new(vec![index as u8; index + 3])))
        }
        fn metadata(&self, index: usize) -> io::Result<Option<FileMetadata>> {
            Ok(Some(FileMetadata {
                size: index as u64 + 3,
                modified_ms: None,
            }))
        }
    }
    #[test]
    fn structured_virtual_file_is_serialized_to_an_owned_compound_file() {
        std::thread::spawn(|| {
            use std::mem::ManuallyDrop;
            use std::os::windows::ffi::OsStrExt;
            use windows::Win32::System::{
                Com::StructuredStorage::*,
                Ole::{OleInitialize, OleUninitialize},
            };
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("compound");
            let source = Virtual {
                descriptors: Vec::new(),
                nodes: Vec::new(),
                indices: Vec::new(),
                ready: Condvar::new(),
                state: Mutex::new(None),
            };
            // SAFETY: fresh apartment, locally owned storage and correctly tagged
            // medium. All COM objects are released before OleUninitialize.
            unsafe {
                OleInitialize(None).unwrap();
                let lock = CreateILockBytesOnHGlobal(None, true).unwrap();
                let storage =
                    StgCreateDocfileOnILockBytes(&lock, STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE, 0)
                        .unwrap();
                let stream = storage
                    .CreateStream(
                        windows::core::w!("payload"),
                        STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
                        0,
                        0,
                    )
                    .unwrap();
                let bytes = [0u8, 255, 0, 9];
                stream.Write(bytes.as_ptr().cast(), 4, None).ok().unwrap();
                drop(stream);
                storage.Commit(0).unwrap();
                let mut medium = STGMEDIUM {
                    tymed: TYMED_ISTORAGE.0 as u32,
                    u: STGMEDIUM_0 {
                        pstg: ManuallyDrop::new(Some(storage)),
                    },
                    pUnkForRelease: ManuallyDrop::new(None),
                };
                assert!(source.copy_storage(&medium, &path).unwrap() > 4);
                ReleaseStgMedium(&mut medium);
                drop(lock);
                let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                let local =
                    StgOpenStorage(PCWSTR(path.as_ptr()), None, STGM_READ | STGM_SHARE_EXCLUSIVE, None, 0).unwrap();
                let stream = local
                    .OpenStream(windows::core::w!("payload"), None, STGM_READ | STGM_SHARE_EXCLUSIVE, 0)
                    .unwrap();
                let mut read = [0u8; 4];
                stream.Read(read.as_mut_ptr().cast(), 4, None).ok().unwrap();
                assert_eq!(read, bytes);
                drop((stream, local));
                OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
    #[test]
    fn native_virtual_source_captures_stream_sized_files_and_keeps_folders_after_com_release() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: isolated test STA with all COM objects released before exit.
            unsafe {
                OleInitialize(None).unwrap();
            }
            let root = tempfile::tempdir().unwrap();
            let entries: Vec<_> = [
                ("folder", true),
                ("folder/first", false),
                ("second", false),
                ("empty", true),
            ]
            .into_iter()
            .map(|(path, is_dir)| crate::FileEntry {
                path: path.into(),
                is_dir,
                size: 0,
                modified_ms: None,
                local_path: None,
            })
            .collect();
            let (data, shared) = super::super::super::vfile::build(
                super::super::super::vfile::Spec {
                    cloud: None,
                    descriptors: super::super::super::files::descriptors(&entries),
                    fixed: Vec::new(),
                    contents: Arc::new(Memory),
                    cf_descriptor: format_id(&NativeFormat::WindowsRegistered("FileGroupDescriptorW".into())).unwrap()
                        as u32,
                    cf_contents: format_id(&NativeFormat::WindowsRegistered("FileContents".into())).unwrap() as u32,
                },
                true,
            );
            let source = Virtual::metadata(&data).unwrap().unwrap();
            assert_eq!(source.roots().len(), 3);
            assert_eq!(source.nodes[0].size, ContentSize::Unknown);
            assert_eq!(source.nodes[1].size, ContentSize::Known(4));
            assert_eq!(source.nodes[2].size, ContentSize::Known(5));
            assert_eq!(source.nodes[3].size, ContentSize::Unknown);
            source.snapshot(&data, root.path()).unwrap();
            drop((data, shared));
            // SAFETY: all test COM objects are released on their owning STA.
            unsafe {
                OleUninitialize();
            }
            let children = source.list(NodeId(1), None).unwrap();
            let file = source.select(children.entries[0].id).unwrap();
            assert_eq!(file.size, ContentSize::Known(4));
            let mut bytes = Vec::new();
            source
                .open(file.id, file.version, 0)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, [1; 4]);
            assert!(source.list(NodeId(4), None).unwrap().entries.is_empty());
            source.cancel();
            assert!(source.select(file.id).is_err());
        })
        .join()
        .unwrap();
    }
}
