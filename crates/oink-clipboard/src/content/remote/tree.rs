use super::*;
use crate::content::namespace::Namespace;
use crate::model::{
    DIRECTORY_PAGE_BYTES, DirectoryCursor, DirectoryPage, FileNode, MAX_DISCOVERED_NODES, NodeId, NodeKind,
};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
};

const BLOCK: u64 = 1024 * 1024;
const METADATA_MAX: usize = 64 * 1024 * 1024;
pub(super) struct Tree {
    pub nodes: HashMap<NodeId, FileNode>,
    pub directories: HashMap<NodeId, Vec<DirectoryPage>>,
    pub files: HashMap<NodeId, Arc<FileSlot>>,
    locks: HashMap<NodeId, Arc<tokio::sync::Mutex<()>>>,
    bytes: usize,
}
pub(super) struct FileSlot {
    state: Mutex<FileState>,
    ready: Condvar,
    changed: tokio::sync::Notify,
}
struct FileCompletion(Arc<FileSlot>);
impl Drop for FileCompletion {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        if state.complete.is_none() {
            state.complete = Some(Err(Unavailable::Cancelled));
        }
        self.0.ready.notify_all();
        self.0.changed.notify_waiters();
    }
}
struct FileState {
    entry: Option<FileNode>,
    file: Option<Arc<File>>,
    verified: u64,
    published: u64,
    last: Option<[u8; 32]>,
    complete: Option<Result<FileRecord, Unavailable>>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileRecord {
    pub node: NodeId,
    pub version: u64,
    pub size: u64,
    pub hash: [u8; 32],
}
impl Tree {
    pub fn new(roots: &[FileNode]) -> Self {
        Self {
            nodes: roots.iter().map(|n| (n.id, n.clone())).collect(),
            directories: HashMap::new(),
            files: HashMap::new(),
            locks: HashMap::new(),
            bytes: 0,
        }
    }
    fn page(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> Option<DirectoryPage> {
        let pages = self.directories.get(&id)?;
        let mut previous = None;
        for page in pages {
            if cursor == previous {
                return Some(page.clone());
            }
            previous = page.next;
        }
        None
    }
}
impl Remote {
    pub(super) async fn directory(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.valid()?;
        let promised = self
            .0
            .inventory
            .items
            .iter()
            .flat_map(|item| &item.representations)
            .any(|rep| matches!(rep.data, RepresentationData::Promise(node, _) if node == id));
        if promised {
            if matches!(*self.0.demand.lock().unwrap(), Demand::Hover(_)) {
                return Err(unavailable(Unavailable::Permission));
            }
            self.start_delivery();
        }
        let lock = {
            let mut tree = self.0.tree.lock().unwrap();
            if tree.nodes.get(&id).is_none_or(|n| n.kind != NodeKind::Directory) {
                return Err(unavailable(Unavailable::NoSuchItem));
            }
            if let Some(page) = tree.page(id, cursor) {
                return Ok(page);
            }
            tree.locks.entry(id).or_default().clone()
        };
        let _guard = lock.lock().await;
        {
            let tree = self.0.tree.lock().unwrap();
            if let Some(page) = tree.page(id, cursor) {
                return Ok(page);
            }
            let expected = tree
                .directories
                .get(&id)
                .and_then(|pages| pages.last())
                .and_then(|page| page.next);
            if expected != cursor {
                return Err(unavailable(Unavailable::Superseded));
            }
        }
        if self.receipt().is_some() {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let request = ContentMsg::ListDirectory {
            offer: self.0.inventory.id,
            node: id,
            cursor,
            demand: *self.0.demand.lock().unwrap(),
        };
        let response = self
            .metadata_request(request, DIRECTORY_PAGE_BYTES as u64 + 65536)
            .await?;
        let ContentMsg::Directory { offer, page } = response else {
            return Err(unavailable(Unavailable::InvalidData));
        };
        if offer != self.0.inventory.id || page.directory != id {
            return Err(unavailable(Unavailable::InvalidData));
        }
        page.validate(cursor).map_err(unavailable)?;
        let mut tree = self.0.tree.lock().unwrap();
        if tree.nodes.len().saturating_add(page.entries.len()) > MAX_DISCOVERED_NODES {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        if page.entries.iter().any(|node| tree.nodes.contains_key(&node.id)) {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let page_bytes = crate::wire::page(&page)
            .and_then(|page| oink_protocol::v1::schema::WireDto::encode(&page, METADATA_MAX))
            .map_err(|_| unavailable(Unavailable::InvalidData))?
            .len();
        if tree.bytes.saturating_add(page_bytes) > METADATA_MAX {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        tree.bytes += page_bytes;
        tree.nodes.extend(page.entries.iter().map(|n| (n.id, n.clone())));
        tree.directories.entry(id).or_default().push(page.clone());
        drop(tree);
        self.update_progress(true, |p| {
            p.discovered += page.entries.len() as u64;
        });
        Ok(page)
    }
    pub(super) async fn metadata_request(&self, request: ContentMsg, receive_max: u64) -> io::Result<ContentMsg> {
        let mut backoff = Duration::from_millis(500);
        loop {
            self.valid()?;
            let result = async {
                let mut channel = self.open_channel(receive_max).await?;
                channel
                    .send(&request.clone())
                    .await?;
                loop {
                    match self.receive_content(channel.as_mut()).await? {
                        ContentMsg::Preparing { offer, node }
                            if offer == self.0.inventory.id
                                && matches!(request, ContentMsg::ListDirectory { node: id, .. } if id == node) => {}
                        ContentMsg::DirectoryFailed { offer, node, reason } | ContentMsg::FileFailed { offer, node, reason }
                            if offer == self.0.inventory.id && matches!(request, ContentMsg::ListDirectory { node: id, .. } | ContentMsg::SelectFile { node: id, .. } if id == node) => {
                            return Err(unavailable(reason));
                        }
                        ContentMsg::Failed { delivery, reason }
                            if matches!(request, ContentMsg::BeginDelivery { delivery: id, .. } if id == delivery) => {
                            return Err(unavailable(reason));
                        }
                        response => {
                            self.end_channel(channel.as_mut()).await?;
                            return Ok(response);
                        }
                    }
                }
            }
            .await;
            match result {
                Ok(response) => return Ok(response),
                Err(error) if transient(&error) => self.retry(&mut backoff).await?,
                Err(error) => return Err(error),
            }
        }
    }
    pub(super) async fn open_channel(&self, receive_max: u64) -> io::Result<Box<dyn BulkChannel>> {
        self.valid()?;
        let options = oink_foundation::transfer::LowOptions {
            send_max: 4096,
            receive_max,
            operation_max: receive_max.saturating_add(4096),
        };
        tokio::select! {
            _ = self.0.changed.notified() => Err(unavailable(Unavailable::Cancelled)),
            _ = self.0.link.cancelled() => Err(unavailable(Unavailable::Cancelled)),
            result = tokio::time::timeout_at(self.deadline(), self.0.link.open_with_options(self.0.inventory.id.clip.origin, options)) => result.map_err(|_| unavailable(Unavailable::NoProgress))?,
        }
    }
    pub(super) async fn receive_content(&self, channel: &mut dyn BulkChannel) -> io::Result<ContentMsg> {
        loop {
            self.valid()?;
            let deadline = self.deadline();
            let result = tokio::select! {
                _ = self.0.changed.notified() => return Err(unavailable(Unavailable::Cancelled)),
                _ = self.0.link.cancelled() => return Err(unavailable(Unavailable::Cancelled)),
                result = tokio::time::timeout_at(deadline, channel.recv()) => result,
            };
            if !channel.effect_validity()() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            match result {
                Ok(Ok(Some(message))) => return Ok(message),
                Ok(Ok(None)) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(Err(error)) => return Err(error),
                // Other members of this delivery can make progress while this
                // directory or stream is waiting. Only the shared deadline fails.
                Err(_) if tokio::time::Instant::now() < self.deadline() => continue,
                Err(_) => return Err(unavailable(Unavailable::NoProgress)),
            }
        }
    }
    pub(super) async fn end_channel(&self, channel: &mut dyn BulkChannel) -> io::Result<()> {
        channel.finish();
        loop {
            match tokio::time::timeout_at(self.deadline(), channel.recv()).await {
                Ok(Ok(None)) if channel.effect_validity()() => return Ok(()),
                Ok(Ok(_)) => return Err(unavailable(Unavailable::InvalidData)),
                Ok(Err(error)) => return Err(error),
                Err(_) if tokio::time::Instant::now() < self.deadline() => continue,
                Err(_) => return Err(unavailable(Unavailable::NoProgress)),
            }
        }
    }
    async fn retry(&self, backoff: &mut Duration) -> io::Result<()> {
        self.update_progress(false, |p| p.phase = super::super::TransferPhase::Reconnecting);
        tokio::select! {
            _ = self.0.changed.notified() => return Err(unavailable(Unavailable::Cancelled)),
            _ = self.0.link.cancelled() => return Err(unavailable(Unavailable::Cancelled)),
            _ = tokio::time::sleep_until(self.deadline().min(tokio::time::Instant::now() + *backoff)) => {},
        }
        self.valid()?;
        if tokio::time::Instant::now() >= self.deadline() {
            return Err(unavailable(Unavailable::NoProgress));
        }
        *backoff = (*backoff * 2).min(Duration::from_secs(5));
        Ok(())
    }
    pub(super) fn file_request(&self, id: NodeId) -> io::Result<Arc<FileSlot>> {
        self.valid()?;
        if matches!(*self.0.demand.lock().unwrap(), Demand::Hover(_)) {
            return Err(unavailable(Unavailable::Permission));
        }
        let mut tree = self.0.tree.lock().unwrap();
        if let Some(slot) = tree.files.get(&id) {
            return Ok(slot.clone());
        }
        if tree.nodes.get(&id).is_none_or(|n| n.kind != NodeKind::File) {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let slot = Arc::new(FileSlot {
            state: Mutex::new(FileState {
                entry: None,
                file: None,
                verified: 0,
                published: 0,
                last: None,
                complete: None,
            }),
            ready: Condvar::new(),
            changed: tokio::sync::Notify::new(),
        });
        tree.files.insert(id, slot.clone());
        let remote = self.clone();
        let worker = slot.clone();
        let completion = FileCompletion(worker.clone());
        self.0.runtime.spawn(async move {
            let _completion = completion;
            let result = remote.fetch_file(id, &worker).await.map_err(|e| reason(&e));
            let succeeded = result.is_ok();
            let mut state = worker.state.lock().unwrap();
            if state.complete.is_none() {
                state.complete = Some(result);
                // A dormant Remote must not retain an open payload handle after
                // TTL eviction. Actual readers hold a read lease and reopen the
                // committed file; in-progress reads still use the writer handle.
                if succeeded {
                    state.file = None;
                }
            }
            drop(state);
            worker.ready.notify_all();
            worker.changed.notify_waiters();
            if succeeded {
                remote.update_progress(true, |p| p.files_done += 1);
            }
        });
        Ok(slot)
    }
    async fn fetch_file(&self, id: NodeId, slot: &Arc<FileSlot>) -> io::Result<FileRecord> {
        let _permit = self
            .0
            .parallel
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| unavailable(Unavailable::Cancelled))?;
        let demand = *self.0.demand.lock().unwrap();
        tracing::info!(target: "oink_file_diagnostics", file_stage = 32_u8,
            file_offer = self.0.inventory.id.clip.seq, file_generation = self.0.inventory.id.generation,
            file_node = id.0, "file selection requested");
        let selected = self
            .metadata_request(
                ContentMsg::SelectFile {
                    offer: self.0.inventory.id,
                    node: id,
                    demand,
                },
                65536,
            )
            .await?;
        tracing::info!(target: "oink_file_diagnostics", file_stage = 33_u8,
            file_offer = self.0.inventory.id.clip.seq, file_generation = self.0.inventory.id.generation,
            file_node = id.0, "file selection response received");
        let (offer, entry, latest) = match selected {
            ContentMsg::FileSelected { offer, entry } => (offer, entry, false),
            ContentMsg::FileVersion { offer, entry } if matches!(demand, Demand::Clipboard) => (offer, entry, true),
            _ => return Err(unavailable(Unavailable::InvalidData)),
        };
        if offer != self.0.inventory.id
            || entry.id != id
            || entry.kind != NodeKind::File
            || !entry.valid()
            || entry.version == 0
        {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let ContentSize::Known(size) = entry.size else {
            return Err(unavailable(Unavailable::InvalidData));
        };
        {
            let mut tree = self.0.tree.lock().unwrap();
            let old = &tree.nodes[&id];
            if old.parent != entry.parent || old.name != entry.name {
                return Err(unavailable(Unavailable::Superseded));
            }
            // OLE descriptors publish known lengths before content selection.
            // Reject changes even when a target skips an advertised empty file;
            // the independent whole-drop receiver must not acknowledge new bytes
            // as the inventory that the native target accepted.
            if self.0.target == ClipPlatform::Windows
                && !latest
                && (matches!(old.size, ContentSize::Known(published) if published != size)
                    || (old.version != 0 && old.version != entry.version))
            {
                return Err(unavailable(Unavailable::Superseded));
            }
            tree.nodes.insert(id, entry.clone());
            let total = tree
                .nodes
                .values()
                .filter_map(|n| match n.size {
                    ContentSize::Known(size) if n.kind == NodeKind::File => Some(size),
                    _ => None,
                })
                .try_fold(0u64, u64::checked_add);
            if total.is_none_or(|n| n > self.0.max_files.load(Ordering::Acquire)) {
                return Err(unavailable(Unavailable::LimitExceeded));
            }
        }
        let path = self.0.batch.path().join(format!("f-{}.part", id.0));
        let file = Arc::new(
            tokio::task::spawn_blocking(move || {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)
            })
            .await
            .map_err(|_| unavailable(Unavailable::ReadFailed))??,
        );
        {
            let mut state = slot.state.lock().unwrap();
            state.entry = Some(entry.clone());
            state.file = Some(file.clone());
        }
        slot.ready.notify_all();
        let mut backoff = Duration::from_millis(500);
        let mut hash = blake3::Hasher::new();
        loop {
            self.valid()?;
            let result = self.file_attempt(id, &entry, slot, &file, &mut hash).await;
            match result {
                Ok(()) => break,
                Err(error) if transient(&error) => self.retry(&mut backoff).await?,
                Err(error) => return Err(error),
            }
        }
        let batch = self.0.batch.clone();
        tokio::task::spawn_blocking(move || {
            file.sync_all()?;
            batch.commit_file(id)
        })
        .await
        .map_err(|_| unavailable(Unavailable::ReadFailed))??;
        self.valid()?;
        slot.state.lock().unwrap().published = size;
        slot.ready.notify_all();
        Ok(FileRecord {
            node: id,
            version: entry.version,
            size,
            hash: *hash.finalize().as_bytes(),
        })
    }
    async fn file_attempt(
        &self,
        id: NodeId,
        entry: &FileNode,
        slot: &FileSlot,
        file: &Arc<File>,
        hash: &mut blake3::Hasher,
    ) -> io::Result<()> {
        let ContentSize::Known(size) = entry.size else {
            unreachable!()
        };
        let (offset, check) = {
            let state = slot.state.lock().unwrap();
            (state.verified, state.last)
        };
        let remaining = size - offset;
        let budget = remaining
            .checked_add(remaining.div_ceil(BLOCK).saturating_add(2).saturating_mul(512))
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        let mut channel = self.open_channel(budget).await?;
        let request = ContentMsg::ReadFile {
            offer: self.0.inventory.id,
            node: id,
            version: entry.version,
            offset,
            check,
            demand: *self.0.demand.lock().unwrap(),
        };
        channel.send(&request).await?;
        let mut offset = offset;
        loop {
            match self.receive_content(channel.as_mut()).await? {
                ContentMsg::FileBlock {
                    offer,
                    node,
                    version,
                    offset: at,
                    hash: digest,
                    bytes,
                } if offer == self.0.inventory.id
                    && node == id
                    && version == entry.version
                    && at == offset
                    && bytes.len() as u64 == BLOCK.min(size - offset)
                    && !bytes.is_empty() =>
                {
                    if blake3::hash(&bytes).as_bytes() != &digest {
                        return Err(unavailable(Unavailable::InvalidData));
                    }
                    let length = bytes.len() as u64;
                    hash.update(&bytes);
                    let file = file.clone();
                    tokio::task::spawn_blocking(move || crate::sync::files::store::write_all_at(&file, &bytes, at))
                        .await
                        .map_err(|_| unavailable(Unavailable::ReadFailed))??;
                    offset += length;
                    {
                        let mut state = slot.state.lock().unwrap();
                        state.verified = offset;
                        state.last = Some(digest);
                        // Hold the last block until explicit EOF and durable file
                        // commit. A reader must not infer success from length alone.
                        if offset < size {
                            state.published = offset;
                        }
                    }
                    slot.ready.notify_all();
                    self.update_progress(true, |p| {
                        p.bytes += length;
                        p.phase = super::super::TransferPhase::Receiving;
                    });
                }
                ContentMsg::FileEnd {
                    offer,
                    node,
                    version,
                    size: length,
                } if offer == self.0.inventory.id
                    && node == id
                    && version == entry.version
                    && length == size
                    && offset == size =>
                {
                    return self.end_channel(channel.as_mut()).await;
                }
                ContentMsg::FileFailed { offer, node, reason } if offer == self.0.inventory.id && node == id => {
                    return Err(unavailable(reason));
                }
                _ => return Err(unavailable(Unavailable::InvalidData)),
            }
        }
    }
    pub(super) fn cancel_files(&self, failure: Unavailable) {
        for slot in self.0.tree.lock().unwrap().files.values() {
            let mut state = slot.state.lock().unwrap();
            if state.complete.is_none() {
                state.complete = Some(Err(failure));
            }
            drop(state);
            slot.ready.notify_all();
            slot.changed.notify_waiters();
        }
    }
    pub(super) async fn wait_file(slot: &FileSlot) -> io::Result<FileRecord> {
        loop {
            let changed = slot.changed.notified();
            if let Some(result) = slot.state.lock().unwrap().complete.clone() {
                return result.map_err(unavailable);
            }
            changed.await;
        }
    }
    /// Full stable descriptor view, requested by OLE only at first GetData. Mac
    /// enumerators use directory() directly and never call this to begin a drag.
    pub async fn expand(&self) -> io::Result<Vec<FileNode>> {
        let mut pending: std::collections::VecDeque<_> = self
            .0
            .inventory
            .namespace_roots_for(self.0.target)
            .iter()
            .map(|root| (root.clone(), 0usize))
            .collect();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.update_progress(false, |p| {
            p.phase = super::super::TransferPhase::Preparing;
            p.enumerating = true;
        });
        while let Some((node, depth)) = pending.pop_front() {
            if depth > 256 || !seen.insert(node.id) {
                return Err(unavailable(Unavailable::LimitExceeded));
            }
            if node.kind == NodeKind::Directory {
                let mut cursor = None;
                loop {
                    let page = self.directory(node.id, cursor).await?;
                    pending.extend(page.entries.iter().cloned().map(|child| (child, depth + 1)));
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
            }
            if node.kind == NodeKind::File && self.0.delivery_started.load(Ordering::Acquire) {
                self.file_request(node.id)?;
            }
            out.push(node);
        }
        self.update_progress(true, |p| {
            p.total_files = Some(out.iter().filter(|n| n.kind == NodeKind::File).count() as u64);
            p.enumerating = false;
        });
        Ok(out)
    }
}
impl Namespace for Remote {
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        Some(self.0.inventory.id.clip.origin)
    }
    fn roots(&self) -> Vec<FileNode> {
        self.0.inventory.roots.clone()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        if matches!(*self.0.demand.lock().unwrap(), Demand::Clipboard) {
            self.start_delivery();
        }
        let page = self.0.runtime.block_on(self.directory(node, cursor))?;
        if cursor.is_none() && page.entries.is_empty() {
            self.0.batch.node_delivered(node.0)?;
        }
        Ok(page)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        self.start_delivery();
        let slot = self.file_request(id)?;
        let mut state = slot.state.lock().unwrap();
        loop {
            if let Some(Err(reason)) = state.complete {
                return Err(unavailable(reason));
            }
            if let Some(entry) = &state.entry {
                return Ok(entry.clone());
            }
            state = slot.ready.wait(state).unwrap();
        }
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        let read = self.0.batch.read_lease()?;
        let entry = Namespace::select(self, node)?;
        if version != entry.version {
            return Err(unavailable(Unavailable::Superseded));
        }
        let slot = self.file_request(node)?;
        Ok(super::super::terminal_reader(Box::new(Reader {
            slot,
            offset,
            remote: self.clone(),
            _batch: self.0.batch.clone(),
            _read: read,
            whole: offset == 0,
        })))
    }
    fn cancel(&self) {
        Remote::cancel(self);
    }
    fn preparing(&self) {
        if self.0.terminal.load(Ordering::Acquire) != 0 {
            return;
        }
        self.update_progress(false, |p| {
            p.phase = super::super::TransferPhase::Preparing;
            p.enumerating = true;
        });
    }
    fn in_use(&self) -> bool {
        self.0.delivery_started.load(Ordering::Acquire)
            && self.receipt().is_none()
            && !self.0.cancelled.load(Ordering::Acquire)
    }
    fn local_batch(&self) -> Option<Batch> {
        Some(self.0.batch.clone())
    }
    fn on_stored(&self, ready: Box<dyn FnOnce(super::super::Completed) + Send>) {
        let mut progress = self.progress();
        let data = self.0.batch.path().parent().unwrap().parent().unwrap().to_owned();
        let id = self.0.batch.id();
        let refetch = self.refetcher();
        self.0.runtime.spawn(async move {
            if progress
                .wait_for(|p| {
                    matches!(
                        p.phase,
                        super::super::TransferPhase::Stored
                            | super::super::TransferPhase::Failed
                            | super::super::TransferPhase::Cancelled
                    )
                })
                .await
                .is_ok()
                && progress.borrow().phase == super::super::TransferPhase::Stored
            {
                tokio::task::spawn_blocking(move || {
                    if let Ok(local) = super::super::Completed::restore(&data, id) {
                        ready(local.with_refetch(refetch));
                    }
                });
            }
        });
    }
}
struct Reader {
    slot: Arc<FileSlot>,
    offset: u64,
    remote: Remote,
    _read: crate::cache::SourceLease,
    _batch: Batch,
    whole: bool,
}
impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut state = self.slot.state.lock().unwrap();
        loop {
            self.remote.valid()?;
            if let Some(Err(reason)) = state.complete {
                return Err(unavailable(reason));
            }
            if state.published > self.offset {
                let n = (state.published - self.offset).min(buffer.len() as u64) as usize;
                let file = if let Some(file) = &state.file {
                    file.clone()
                } else {
                    let Some(Ok(record)) = &state.complete else {
                        return Err(unavailable(Unavailable::ReadFailed));
                    };
                    Arc::new(File::open(self._batch.path().join(format!("f-{}", record.node.0)))?)
                };
                drop(state);
                crate::sync::files::store::read_exact_at(&file, &mut buffer[..n], self.offset)?;
                self.offset += n as u64;
                return Ok(n);
            }
            if let Some(Ok(record)) = &state.complete {
                return if self.offset <= record.size {
                    if self.whole {
                        self._batch.node_delivered(record.node.0)?;
                    }
                    Ok(0)
                } else {
                    Err(unavailable(Unavailable::InvalidData))
                };
            }
            state = self.slot.ready.wait(state).unwrap();
        }
    }
}
fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[tokio::test]
    async fn aborting_an_unpolled_file_download_wakes_native_file_readers() {
        let slot = Arc::new(FileSlot {
            state: Mutex::new(FileState {
                entry: None,
                file: None,
                verified: 0,
                published: 0,
                last: None,
                complete: None,
            }),
            ready: Condvar::new(),
            changed: tokio::sync::Notify::new(),
        });
        let waiting = slot.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut state = waiting.state.lock().unwrap();
            while state.complete.is_none() {
                state = waiting.ready.wait(state).unwrap();
            }
            send.send(state.complete.as_ref().unwrap().as_ref().err().copied())
                .unwrap();
        });
        let completion = FileCompletion(slot);
        let task = tokio::spawn(async move {
            let _completion = completion;
            std::future::pending::<()>().await;
        });
        task.abort();
        let _ = task.await;
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(Unavailable::Cancelled)
        );
        reader.join().unwrap();
    }
}
