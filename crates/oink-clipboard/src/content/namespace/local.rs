use super::*;
pub(in crate::content) mod batch;
mod proxy;
use crate::model::{ContentSize, INVENTORY_BYTES, MAX_DISCOVERED_NODES, NodeKind};
use std::{
    collections::HashMap,
    fs::{self, File, Metadata},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Local {
    roots: Vec<FileNode>,
    state: Arc<Mutex<State>>,
    cancelled: Arc<AtomicBool>,
    clipboard: bool,
    origin: Option<oink_foundation::DeviceId>,
}
struct State {
    nodes: HashMap<NodeId, Node>,
    next: u64,
    limit: usize,
    bytes: usize,
    retention: Option<super::super::transfer::FileRetention>,
    batch: batch::Batch,
}
struct Node {
    entry: FileNode,
    location: Location,
    selected: Option<Fingerprint>,
    view: Option<View>,
}
// Before registration the current capture owns its references. Registration
// transfers that ownership to the 1024-entry FIFO. Eviction drops the path and
// identity record even when an old native owner still holds its namespace.
pub(crate) struct Reference {
    path: PathBuf,
    offered: Fingerprint,
    followed: Vec<batch::Followed>,
    proxy: Option<proxy::Location>,
    _source: Mutex<Option<crate::cache::SourceLease>>,
}
impl Reference {
    fn path(&self) -> io::Result<PathBuf> {
        self.proxy
            .as_ref()
            .map_or_else(|| Ok(self.path.clone()), proxy::Location::path)
    }
    fn inspect(&self) -> io::Result<Fingerprint> {
        let current = inspect(&self.path()?)?;
        self.normalize(current)
    }
    fn normalize(&self, current: Fingerprint) -> io::Result<Fingerprint> {
        match &self.proxy {
            Some(proxy) => proxy.normalize(current, &self.offered),
            None => Ok(current),
        }
    }
}
enum Location {
    Owned(Arc<Reference>),
    Offered(std::sync::Weak<Reference>),
}
impl Location {
    fn new(path: PathBuf, offered: Fingerprint, followed: Vec<batch::Followed>) -> Self {
        let source = crate::cache::SourceLease::new(path.clone());
        Self::Owned(Arc::new(Reference {
            path,
            offered,
            followed,
            proxy: None,
            _source: Mutex::new(Some(source)),
        }))
    }
    fn get(&self) -> io::Result<Arc<Reference>> {
        match self {
            Self::Owned(reference) => Ok(reference.clone()),
            Self::Offered(reference) => reference.upgrade().ok_or_else(|| unavailable(Unavailable::NoSuchItem)),
        }
    }
    fn retain(&mut self, retention: &super::super::transfer::FileRetention) {
        if let Self::Owned(reference) = self {
            *self = Self::Offered(retention.insert(reference.clone()));
        }
    }
}
struct View {
    fingerprint: Fingerprint,
    entries: Vec<FileNode>,
}
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(in crate::content) struct Fingerprint {
    pub(in crate::content) identity: [u64; 2],
    pub(in crate::content) size: u64,
    pub(in crate::content) modified: SystemTime,
    pub(in crate::content) created: Option<SystemTime>,
    pub(in crate::content) directory: bool,
    pub(in crate::content) exec_bits: Option<u16>,
}
impl Fingerprint {
    fn new(metadata: &Metadata, identity: [u64; 2]) -> io::Result<Self> {
        Ok(Self {
            identity,
            size: metadata.len(),
            modified: metadata.modified()?,
            created: metadata.created().ok(),
            directory: metadata.is_dir(),
            exec_bits: {
                #[cfg(target_os = "macos")]
                {
                    use std::os::unix::fs::PermissionsExt;
                    metadata
                        .is_file()
                        .then(|| (metadata.permissions().mode() & 0o111) as u16)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    None
                }
            },
        })
    }
    pub(in crate::content) fn version(&self) -> u64 {
        let mut hash = blake3::Hasher::new();
        for number in self.identity.into_iter().chain([self.size]) {
            hash.update(&number.to_le_bytes());
        }
        // ctime/ChangeTime also change for access grants, xattrs, rw permissions and
        // renames. They are not content versions: an unchanged drag source must
        // remain readable through those metadata-only events. Same-size rewrites
        // with restored mtime are outside this metadata fingerprint's precision.
        for time in [Some(self.modified), self.created].into_iter().flatten() {
            match time.duration_since(UNIX_EPOCH) {
                Ok(value) => {
                    hash.update(&[0]);
                    hash.update(&value.as_nanos().to_le_bytes());
                }
                Err(value) => {
                    hash.update(&[1]);
                    hash.update(&value.duration().as_nanos().to_le_bytes());
                }
            }
        }
        // Executability is transfer metadata, so a chmod changing x bits must
        // supersede a selected version even with identical bytes and mtime.
        if let Some(bits) = self.exec_bits.filter(|bits| *bits != 0) {
            hash.update(b"exec");
            hash.update(&bits.to_le_bytes());
        }
        u64::from_le_bytes(hash.finalize().as_bytes()[..8].try_into().unwrap()).max(1)
    }
}
impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalNamespace")
            .field("roots", &self.roots.len())
            .finish()
    }
}
impl Local {
    pub fn new(roots: Vec<(NodeId, PathBuf)>) -> io::Result<Arc<Self>> {
        Self::with_limit(roots, MAX_DISCOVERED_NODES)
    }
    pub fn clipboard(roots: Vec<(NodeId, PathBuf)>) -> io::Result<Arc<Self>> {
        let mut local = Self::new(roots)?;
        Arc::get_mut(&mut local).unwrap().clipboard = true;
        Ok(local)
    }
    /// Files this device holds on behalf of another device (a phone's upload
    /// kept by its proxy PC). `origin` names that device, so a native receive
    /// of these files lands in its own received-files folder.
    pub fn received_from(roots: Vec<(NodeId, PathBuf)>, origin: oink_foundation::DeviceId) -> io::Result<Arc<Self>> {
        let mut used = std::collections::HashSet::new();
        let mut names = HashMap::new();
        for (id, path) in &roots {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(io::ErrorKind::InvalidInput)?;
            names.insert(*id, batch::component(name, &mut used));
        }
        let tracked: Vec<_> = roots
            .into_iter()
            .map(|(id, path)| Ok((id, proxy::Root::load(path)?)))
            .collect::<io::Result<_>>()?;
        let roots = tracked
            .iter()
            .map(|(id, root)| Ok((*id, root.path()?)))
            .collect::<io::Result<_>>()?;
        let mut local = Self::new(roots)?;
        {
            let mut state = local.state.lock().unwrap();
            for (id, root) in tracked {
                if let Some(node) = state.nodes.get_mut(&id) {
                    node.entry.name = names[&id].clone();
                    if let Location::Owned(reference) = &mut node.location {
                        let reference = Arc::get_mut(reference).unwrap();
                        reference.offered = root.offered();
                        reference.proxy = Some(root.location(PathBuf::new()));
                        if !reference.offered.directory {
                            node.entry.version = reference.offered.version();
                        }
                    }
                }
            }
        }
        let inner = Arc::get_mut(&mut local).unwrap();
        inner.origin = Some(origin);
        {
            let state = inner.state.lock().unwrap();
            for root in &mut inner.roots {
                *root = state.nodes[&root.id].entry.clone();
            }
        }
        Ok(local)
    }
    fn with_limit(roots: Vec<(NodeId, PathBuf)>, limit: usize) -> io::Result<Arc<Self>> {
        if roots.len() > limit {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        let had_roots = !roots.is_empty();
        let mut nodes = HashMap::new();
        let mut entries = Vec::new();
        let mut batch = batch::Batch::default();
        let mut prepared = Vec::new();
        let mut used = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for (id, path) in roots {
            if !ids.insert(id) {
                return Err(unavailable(Unavailable::InvalidData));
            }
            let result: io::Result<_> = (|| {
                let raw = path
                    .file_name()
                    .and_then(|v| v.to_str())
                    .ok_or_else(|| unavailable(Unavailable::InvalidData))?;
                let name = batch::component(raw, &mut used);
                let metadata = fs::symlink_metadata(&path)?;
                // Register ALL ordinary selected roots before classifying a link,
                // including a link selected before its target in native order.
                if !crate::cache::is_link(&metadata) {
                    let fingerprint = inspect(&path)?;
                    batch.include(&path.canonicalize()?, vec![name.clone()], &fingerprint);
                }
                Ok((id, path, name))
            })();
            match result {
                Ok(root) => prepared.push(root),
                Err(error) => tracing::warn!(reason=?error.kind(), "skipping unavailable selected root"),
            }
        }
        // Followed top-level directories also become members of this batch.
        for (id, path, name) in prepared {
            let captured = match batch.capture(&path, vec![name.clone()], &[]) {
                Ok(Some(captured)) => captured,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(reason=?error.kind(), "skipping unavailable selected link");
                    continue;
                }
            };
            let mut entry = entry(id, None, name, &captured.fingerprint)?;
            if let Some(target) = captured.link {
                set_link(&mut entry, target);
            }
            nodes.insert(
                id,
                Node {
                    entry: entry.clone(),
                    location: Location::new(captured.path, captured.fingerprint, captured.followed),
                    selected: None,
                    view: None,
                },
            );
            entries.push(entry);
        }
        if had_roots && entries.is_empty() {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        if crate::wire::nodes_size(&entries).map_err(|_| unavailable(Unavailable::InvalidData))? > INVENTORY_BYTES {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        let next = nodes
            .keys()
            .map(|id| id.0)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        Ok(Arc::new(Self {
            roots: entries,
            state: Arc::new(Mutex::new(State {
                nodes,
                next,
                limit,
                bytes: INVENTORY_BYTES,
                retention: None,
                batch,
            })),
            cancelled: Arc::new(AtomicBool::new(false)),
            clipboard: false,
            origin: None,
        }))
    }
    fn valid(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(unavailable(Unavailable::Cancelled))
        } else {
            Ok(())
        }
    }
}
impl Namespace for Local {
    fn release_fulfilled(&self, roots: &[NodeId]) {
        let state = self.state.lock().unwrap();
        for node in state.nodes.values() {
            let mut ancestor = &node.entry;
            for _ in 0..256 {
                if roots.contains(&ancestor.id) {
                    if let Ok(reference) = node.location.get() {
                        // Existing readers have independent path leases and
                        // keep validating this unchanged identity record.
                        reference._source.lock().unwrap().take();
                    }
                    break;
                }
                let Some(parent) = ancestor.parent.and_then(|id| state.nodes.get(&id)) else {
                    break;
                };
                ancestor = &parent.entry;
            }
        }
    }
    fn source_roots(&self) -> io::Result<Vec<super::super::grants::SourceRoot>> {
        self.valid()?;
        let state = self.state.lock().unwrap();
        let mut captured = Vec::new();
        for entry in &self.roots {
            let result = (|| {
                verify_chain(&state, entry.id, self.clipboard)?;
                let reference = state.nodes[&entry.id].location.get()?;
                let path = reference.path()?;
                let expected = if reference.proxy.is_some() {
                    inspect(&path)?
                } else {
                    reference.offered.clone()
                };
                super::super::grants::SourceRoot::capture_followed(
                    state.nodes[&entry.id].entry.clone(),
                    &path,
                    &expected,
                    reference.followed.clone(),
                )
                .map(|root| root.with_tracked(reference.proxy.as_ref().and_then(proxy::Location::tracked)))
            })();
            match result {
                Ok(root) => captured.push(root),
                Err(error) => tracing::warn!(reason=?error.kind(), "skipping unavailable source root"),
            }
        }
        Ok(captured)
    }

    fn real_clipboard_files(&self) -> bool {
        self.clipboard
    }
    fn tracked_file(&self, id: NodeId) -> io::Result<Option<crate::tracked_file::TrackedFile>> {
        let state = self.state.lock().unwrap();
        let reference = state
            .nodes
            .get(&id)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?
            .location
            .get()?;
        Ok(reference.proxy.as_ref().and_then(proxy::Location::tracked))
    }
    fn proxy_file_moved(&self, id: NodeId, target: &crate::tracked_file::TrackedFile) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        let reference = state
            .nodes
            .get(&id)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?
            .location
            .get()?;
        if let Some(proxy) = &reference.proxy {
            proxy.relocate(target)?;
        }
        Ok(())
    }
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        self.origin
    }
    fn announced_roots(&self) -> Vec<(NodeId, PathBuf, [u64; 2])> {
        if !self.clipboard {
            return Vec::new();
        }
        let state = self.state.lock().unwrap();
        self.roots
            .iter()
            .filter_map(|root| {
                let node = state.nodes.get(&root.id)?;
                // A link (or a root reached through one) is bound to the
                // batch that followed it; only plain paths are recorded.
                if node.entry.kind.symlink_target().is_some() {
                    return None;
                }
                let reference = node.location.get().ok()?;
                reference
                    .followed
                    .is_empty()
                    .then(|| (root.id, reference.path.clone(), reference.offered.identity))
            })
            .collect()
    }
    fn retain_files(&self, retention: super::super::transfer::FileRetention) {
        if !self.clipboard {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.retention.is_some() {
            return;
        }
        let mut ids: Vec<_> = state.nodes.keys().copied().collect();
        ids.sort_by_key(|id| id.0);
        for id in ids {
            state.nodes.get_mut(&id).unwrap().location.retain(&retention);
        }
        state.retention = Some(retention);
    }
    fn roots(&self) -> Vec<FileNode> {
        self.roots
            .iter()
            .cloned()
            .map(|mut node| {
                node.exec_bits = None;
                node
            })
            .collect()
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.valid()?;
        let mut state = self.state.lock().unwrap();
        verify_chain(&state, id, self.clipboard)?;
        let node = state
            .nodes
            .get(&id)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if node.entry.kind != NodeKind::Directory {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let reference = node.location.get()?;
        let fingerprint = reference.inspect()?;
        if let Some(view) = &node.view {
            if view.fingerprint != fingerprint {
                return Err(unavailable(Unavailable::Superseded));
            }
        } else {
            if cursor.is_some() {
                return Err(unavailable(Unavailable::Superseded));
            }
            let mut ancestor = Some(id);
            let mut depth = 0;
            while let Some(id) = ancestor {
                depth += 1;
                if depth >= 256 {
                    return Err(unavailable(Unavailable::LimitExceeded));
                }
                ancestor = state.nodes[&id].entry.parent;
            }
            let parent_path = reference.path()?;
            let mut logical = Vec::new();
            let mut ancestor = Some(id);
            while let Some(current) = ancestor {
                logical.push(state.nodes[&current].entry.name.clone());
                ancestor = state.nodes[&current].entry.parent;
            }
            logical.reverse();
            let available = state.limit.saturating_sub(state.nodes.len());
            let mut children = Vec::new();
            let mut batch = state.batch.clone();
            for (name, path) in batch::children(&parent_path)? {
                self.valid()?;
                let mut child_path = logical.clone();
                child_path.push(name.clone());
                let captured = match batch.capture(&path, child_path, &reference.followed) {
                    Ok(Some(captured)) => captured,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(reason=?error.kind(), "skipping unavailable linked item");
                        continue;
                    }
                };
                if children.len() >= available {
                    return Err(unavailable(Unavailable::LimitExceeded));
                }
                children.push((name, captured));
            }
            if reference.inspect()? != fingerprint {
                return Err(unavailable(Unavailable::Superseded));
            }
            children.sort_by(|a, b| a.0.cmp(&b.0));
            let mut entries = Vec::with_capacity(children.len());
            let mut staged = Vec::with_capacity(children.len());
            let mut next = state.next;
            let mut metadata_bytes = state.bytes;
            for (name, captured) in children {
                let batch::Captured {
                    path,
                    fingerprint,
                    link,
                    followed,
                } = captured;
                let child = NodeId(next);
                next = next
                    .checked_add(1)
                    .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
                let mut entry = entry(child, Some(id), name, &fingerprint)?;
                if let Some(target) = link {
                    set_link(&mut entry, target);
                }
                metadata_bytes = metadata_bytes.saturating_add(
                    crate::wire::node(&entry)
                        .and_then(|value| oink_protocol::v2::schema::WireDto::encode(&value, INVENTORY_BYTES))
                        .map_err(|_| unavailable(Unavailable::InvalidData))?
                        .len(),
                );
                if metadata_bytes > 64 * 1024 * 1024 {
                    return Err(unavailable(Unavailable::LimitExceeded));
                }
                let tail = path.strip_prefix(&parent_path).ok().map(Path::to_path_buf);
                let mut location = Location::new(path, fingerprint, followed);
                if let (Some(parent), Some(tail), Location::Owned(child)) = (&reference.proxy, tail, &mut location) {
                    Arc::get_mut(child).unwrap().proxy = Some(parent.child(&tail));
                }
                staged.push((
                    child,
                    Node {
                        entry: entry.clone(),
                        location,
                        selected: None,
                        view: None,
                    },
                ));
                entries.push(entry);
            }
            if let Some(retention) = &state.retention {
                for (_, node) in &mut staged {
                    node.location.retain(retention);
                }
            }
            state.batch = batch;
            state.nodes.extend(staged);
            state.next = next;
            state.bytes = metadata_bytes;
            state.nodes.get_mut(&id).unwrap().view = Some(View {
                fingerprint: fingerprint.clone(),
                entries,
            });
        }
        self.valid()?;
        let view = state.nodes[&id].view.as_ref().unwrap();
        let version = view.fingerprint.version();
        super::page(id, version, &view.entries, cursor)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        self.valid()?;
        let mut state = self.state.lock().unwrap();
        verify_chain(&state, id, self.clipboard)?;
        let node = state
            .nodes
            .get_mut(&id)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if node.entry.kind != NodeKind::File {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let reference = node.location.get()?;
        let current = reference.inspect()?;
        if !self.clipboard && node.selected.as_ref().is_some_and(|old| *old != current) {
            return Err(unavailable(Unavailable::Superseded));
        }
        node.selected = Some(current.clone());
        node.entry.size = ContentSize::Known(current.size);
        node.entry.version = current.version();
        node.entry.modified_ms = modified_ms(current.modified);
        node.entry.exec_bits = current.exec_bits;
        Ok(node.entry.clone())
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        let entry = self.select(id)?;
        if entry.version != version {
            return Err(unavailable(Unavailable::Superseded));
        }
        let state = self.state.lock().unwrap();
        let node = &state.nodes[&id];
        let selected = node.selected.as_ref().unwrap().clone();
        if selected.version() != version {
            return Err(unavailable(Unavailable::Superseded));
        }
        if offset > selected.size {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let reference = node.location.get()?;
        let path = reference.path()?;
        let read = crate::cache::SourceLease::reading(path.clone());
        let mut file = File::open(&path)?;
        if reference.normalize(file_fingerprint(&file)?)? != selected {
            return Err(unavailable(Unavailable::Superseded));
        }
        file.seek(SeekFrom::Start(offset))?;
        Ok(super::super::terminal_reader(Box::new(Reader {
            file,
            proxy: reference.proxy.clone(),
            _read: read,
            selected,
            cancelled: self.cancelled.clone(),
            state: self.state.clone(),
            id,
            clipboard: self.clipboard,
        })))
    }
}
struct Reader {
    file: File,
    proxy: Option<proxy::Location>,
    _read: crate::cache::SourceLease,
    selected: Fingerprint,
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    id: NodeId,
    clipboard: bool,
}
impl Reader {
    fn fingerprint(&self) -> io::Result<Fingerprint> {
        let current = file_fingerprint(&self.file)?;
        match &self.proxy {
            Some(proxy) => proxy.normalize(current, &self.selected),
            None => Ok(current),
        }
    }
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(unavailable(Unavailable::Cancelled));
        }
        verify_chain(&self.state.lock().unwrap(), self.id, self.clipboard)?;
        if self.fingerprint()? != self.selected {
            return Err(unavailable(Unavailable::Superseded));
        }
        let count = self.file.read(bytes)?;
        // Includes EOF and truncation; never acknowledge bytes from two versions.
        verify_chain(&self.state.lock().unwrap(), self.id, self.clipboard)?;
        if self.fingerprint()? != self.selected {
            return Err(unavailable(Unavailable::Superseded));
        }
        Ok(count)
    }
}
fn verify_chain(state: &State, mut id: NodeId, clipboard: bool) -> io::Result<()> {
    loop {
        let node = state
            .nodes
            .get(&id)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        let reference = node.location.get()?;
        for guard in &reference.followed {
            guard.verify()?;
        }
        let current = (if node.entry.kind.symlink_target().is_some() {
            inspect_leaf(&reference.path)
        } else {
            reference.inspect()
        })
        .map_err(|error| {
            if clipboard
                && matches!(
                    super::super::reason(&error),
                    Unavailable::Unsupported | Unavailable::NoSuchItem
                )
            {
                unavailable(Unavailable::NoSuchItem)
            } else {
                error
            }
        })?;
        if current.identity != reference.offered.identity {
            return Err(unavailable(if clipboard && reference.followed.is_empty() {
                Unavailable::NoSuchItem
            } else {
                Unavailable::Superseded
            }));
        }
        // Retained files own their absolute path and disk identity independently
        // of parent metadata. Evicting a directory record cannot revoke a child
        // whose own record remains in the FIFO. Drop keeps its ancestor checks.
        if clipboard {
            return Ok(());
        }
        match node.entry.parent {
            Some(parent) => id = parent,
            None => return Ok(()),
        }
    }
}
pub(in crate::content) fn entry(
    id: NodeId,
    parent: Option<NodeId>,
    name: String,
    fingerprint: &Fingerprint,
) -> io::Result<FileNode> {
    let entry = FileNode {
        exec_bits: fingerprint.exec_bits,
        id,
        parent,
        name,
        kind: if fingerprint.directory {
            NodeKind::Directory
        } else {
            NodeKind::File
        },
        size: if fingerprint.directory {
            ContentSize::Unknown
        } else {
            ContentSize::Known(fingerprint.size)
        },
        // Metadata versions support unchanged single-file copy equivalence. select
        // still chooses the current version independently of this initial hint.
        version: if fingerprint.directory {
            0
        } else {
            fingerprint.version()
        },
        modified_ms: modified_ms(fingerprint.modified),
    };
    if !entry.valid() {
        return Err(unavailable(Unavailable::InvalidData));
    }
    Ok(entry)
}
fn modified_ms(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(e) => i64::try_from(e.duration().as_millis()).ok().map(|n| -n),
    }
}
pub(in crate::content) fn inspect(path: &Path) -> io::Result<Fingerprint> {
    let metadata = fs::symlink_metadata(path)?;
    if crate::cache::is_link(&metadata) || !(metadata.is_file() || metadata.is_dir()) {
        return Err(unavailable(Unavailable::Unsupported));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Fingerprint::new(&metadata, [metadata.dev(), metadata.ino()])
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new().read(true).custom_flags(0x02000000).open(path)?;
        file_fingerprint(&file)
    }
}
pub(in crate::content) fn file_fingerprint(file: &File) -> io::Result<Fingerprint> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        [metadata.dev(), metadata.ino()]
    };
    #[cfg(windows)]
    let identity = {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: this borrowed live File handle and writable record survive the call.
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }.map_err(io::Error::other)?;
        [
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ]
    };
    Fingerprint::new(&metadata, identity)
}

#[cfg(all(test, debug_assertions))]
mod tests;

/// Only the link itself is inspected; no canonicalize/stat/open of its target.
/// Windows does not publish reparse leaves; batch capture binds followed targets.
pub(in crate::content) fn inspect_leaf(path: &Path) -> io::Result<Fingerprint> {
    #[cfg(target_os = "macos")]
    {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            use std::os::unix::fs::MetadataExt;
            return Fingerprint::new(&metadata, [metadata.dev(), metadata.ino()]);
        }
    }
    inspect(path)
}
pub(in crate::content) fn set_link(entry: &mut FileNode, target: String) {
    let mut hash = blake3::Hasher::new();
    hash.update(&entry.version.to_le_bytes());
    hash.update(target.as_bytes());
    entry.version = u64::from_le_bytes(hash.finalize().as_bytes()[..8].try_into().unwrap()).max(1);
    entry.size = ContentSize::Known(target.len() as u64);
    entry.kind = NodeKind::Symlink(target);
    entry.exec_bits = None;
}
