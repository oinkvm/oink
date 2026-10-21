//! Verified deliveries keep dormant catalogs without pinning bytes. A live
//! process may retain an offer-only re-fetch capability; restored catalogs are
//! local-only. Reads and eviction share Batch's existing lifetime lock.
use super::*;
use crate::model::{DirectoryCursor, DirectoryPage, FileNode, MAX_DISCOVERED_NODES, NodeId, NodeKind};
use oink_foundation::clipboard::ClipPlatform;
use std::{collections::HashSet, fs, io::Seek, path::PathBuf};

#[derive(Clone)]
pub struct Completed(Arc<Local>);
impl std::fmt::Debug for Completed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completed").field("nodes", &self.0.nodes.len()).finish()
    }
}
type Refetch = Arc<dyn Fn() -> io::Result<Completed> + Send + Sync>;
struct Retry {
    fetch: Refetch,
    cached: Option<Completed>,
}
struct Local {
    retry: Mutex<Option<Retry>>,
    data: PathBuf,
    id: BatchId,
    inventory: Inventory,
    target: ClipPlatform,
    nodes: Vec<FileNode>,
    index: HashMap<NodeId, usize>,
    children: HashMap<NodeId, Vec<FileNode>>,
    representations: HashMap<RepresentationId, u64>,
}
fn invalid() -> io::Error {
    unavailable(Unavailable::InvalidData)
}
fn regular(batch: &Batch, name: &str, size: u64) -> io::Result<fs::File> {
    let path = batch.path().join(name);
    let metadata = fs::symlink_metadata(&path)?;
    if crate::cache::is_link(&metadata) || !metadata.is_file() || metadata.len() != size {
        return Err(invalid());
    }
    fs::File::open(path)
}
impl Completed {
    #[cfg(all(debug_assertions, all(test, target_os = "macos")))]
    pub(crate) fn test_single_file(data: &Path) -> Batch {
        tests::fixture(data)
    }
    pub fn restore(data: &Path, id: BatchId) -> io::Result<Self> {
        let batch = Batch::restore(data, id)?;
        let _read = batch.read_lease()?;
        let manifest: super::manifest::Manifest = super::manifest::Manifest::decode(&batch.manifest()?)?;
        if manifest.nodes.len() > MAX_DISCOVERED_NODES {
            return Err(invalid());
        }
        let inventory = crate::wire::local_inventory(&manifest.inventory).map_err(|_| invalid())?;
        inventory.validate().map_err(unavailable)?;
        let mut index = HashMap::new();
        let mut children: HashMap<NodeId, Vec<FileNode>> = HashMap::new();
        let mut depths = HashMap::new();
        let mut roots = Vec::new();
        for (position, node) in manifest.nodes.iter().enumerate() {
            if !node.valid() || index.insert(node.id, position).is_some() {
                return Err(invalid());
            }
            let depth = if let Some(parent) = node.parent {
                // The committed manifest is breadth-first, matching discovery.
                // Requiring parent-before-child also rejects cycles and orphans.
                if index
                    .get(&parent)
                    .is_none_or(|i| manifest.nodes[*i].kind != NodeKind::Directory)
                {
                    return Err(invalid());
                }
                children.entry(parent).or_default().push(node.clone());
                depths.get(&parent).copied().ok_or_else(invalid)? + 1usize
            } else {
                roots.push(node);
                0
            };
            if depth > 256 {
                return Err(invalid());
            }
            depths.insert(node.id, depth);
        }
        let offered_roots = inventory.namespace_roots_for(manifest.target);
        if roots.len() != offered_roots.len()
            || roots.iter().zip(&offered_roots).any(|(stored, offered)| {
                stored.id != offered.id || stored.name != offered.name || stored.kind != offered.kind
            })
        {
            return Err(invalid());
        }
        let mut files = HashSet::new();
        for file in &manifest.files {
            let node = index.get(&file.node).map(|i| &manifest.nodes[*i]).ok_or_else(invalid)?;
            if !files.insert(file.node)
                || node.kind != NodeKind::File
                || node.version != file.version
                || node.size != ContentSize::Known(file.size)
            {
                return Err(invalid());
            }
            // Bytes were hashed before commit. Startup checks ownership and
            // length; it does not reread gigabytes merely to list a cached URL.
            regular(&batch, &format!("f-{}", file.node.0), file.size)?;
        }
        if files.len() != manifest.nodes.iter().filter(|n| n.kind == NodeKind::File).count() {
            return Err(invalid());
        }
        let mut representations = HashMap::new();
        for (id, size, _) in &manifest.representations {
            if *size > REPRESENTATION_BYTES || representations.insert(*id, *size).is_some() {
                return Err(invalid());
            }
            regular(&batch, &format!("r-{}", id.0), *size)?;
        }
        let expected: HashSet<_> = inventory
            .wanted(manifest.target)
            .filter_map(|rep| {
                (rep.data.size().is_some()
                    && !(manifest.target == inventory.platform && matches!(rep.data, RepresentationData::Inline(_))))
                .then_some(rep.id)
            })
            .collect();
        if expected.len() != representations.len() || expected.iter().any(|id| !representations.contains_key(id)) {
            return Err(invalid());
        }
        Ok(Self(Arc::new(Local {
            retry: Mutex::new(None),
            data: data.to_owned(),
            id,
            inventory,
            target: manifest.target,
            nodes: manifest.nodes,
            index,
            children,
            representations,
        })))
    }
    pub(super) fn with_refetch(self, fetch: Refetch) -> Self {
        *self.0.retry.lock().unwrap() = Some(Retry { fetch, cached: None });
        self
    }
    fn refetch(&self, error: io::Error) -> io::Result<Self> {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
        let mut retry = self.0.retry.lock().unwrap();
        let retry = retry.as_mut().ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if let Some(local) = &retry.cached
            && local.pin().is_ok()
        {
            return Ok(local.clone());
        }
        let local = (retry.fetch)()?.with_refetch(retry.fetch.clone());
        retry.cached = Some(local.clone());
        Ok(local)
    }
    pub fn pin(&self) -> io::Result<Batch> {
        Batch::restore(&self.0.data, self.0.id)
    }
    fn read_batch(&self) -> io::Result<(Batch, crate::cache::SourceLease)> {
        let batch = self.pin()?;
        let read = batch.read_lease()?;
        Ok((batch, read))
    }
    fn publication_pin(&self) -> Option<Batch> {
        self.pin()
            .ok()
            .or_else(|| self.0.retry.try_lock().ok()?.as_ref()?.cached.as_ref()?.pin().ok())
    }
    /// The bounded Mac clipboard fallback publishes ordinary local file URLs.
    /// Keep user-facing names in a separate owned tree: applications may modify
    /// a URL, so hard-linking it to the immutable verified payload is incorrect.
    pub fn materialize(&self) -> io::Result<Vec<crate::FileEntry>> {
        let batch = self.pin()?;
        let read = batch.read_lease()?;
        let destination = batch.path().join("published");
        let temporary = batch.path().join("published.part");
        let mut names = namespace::Names::new(crate::sync::files::TargetOs::current());
        let entries = self
            .0
            .nodes
            .iter()
            .map(|node| names.entry(node))
            .collect::<io::Result<Vec<_>>>()?;
        if !destination.try_exists()? {
            fs::create_dir(&temporary)?;
            let result = (|| {
                for (node, entry) in self.0.nodes.iter().zip(&entries) {
                    let path = temporary.join(&entry.path);
                    if node.kind == NodeKind::Directory {
                        fs::create_dir(&path)?;
                    } else {
                        regular(&batch, &format!("f-{}", node.id.0), entry.size)?;
                        fs::copy(batch.path().join(format!("f-{}", node.id.0)), &path)?;
                        let output = fs::OpenOptions::new().write(true).open(path)?;
                        if let Some(ms) = entry.modified_ms {
                            let time = if ms >= 0 {
                                std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64)
                            } else {
                                std::time::UNIX_EPOCH - std::time::Duration::from_millis(ms.unsigned_abs())
                            };
                            output.set_modified(time)?;
                        }
                        output.sync_all()?;
                    }
                }
                // Every payload file is flushed above. This is a new directory,
                // so no writable-file replacement handle belongs to the rename.
                fs::rename(&temporary, &destination)?;
                crate::store::sync_directory(batch.path())
            })();
            if result.is_err() {
                let _ = fs::remove_dir_all(&temporary);
            }
            result?;
        }
        let mut roots = Vec::new();
        for (node, mut entry) in self.0.nodes.iter().zip(entries) {
            let path = destination.join(&entry.path);
            let metadata = fs::symlink_metadata(&path)?;
            if crate::cache::is_link(&metadata)
                || metadata.is_dir() != entry.is_dir
                || (!entry.is_dir && metadata.len() != entry.size)
            {
                return Err(invalid());
            }
            if node.parent.is_none() {
                entry.local_path = Some(path);
                roots.push(entry);
            }
        }
        batch.delivered()?;
        drop(read);
        Ok(roots)
    }
    pub fn nodes(&self) -> &[FileNode] {
        &self.0.nodes
    }
    fn node(&self, id: NodeId) -> io::Result<&FileNode> {
        self.0
            .index
            .get(&id)
            .map(|i| &self.0.nodes[*i])
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))
    }
}
impl Provider for Completed {
    fn inventory(&self) -> Inventory {
        self.0.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        let (batch, read) = match self.read_batch() {
            Ok(pinned) => pinned,
            Err(error) => return Provider::open(&self.refetch(error)?, id),
        };
        let rep = self
            .0
            .inventory
            .wanted(self.0.target)
            .find(|rep| rep.id == id)
            .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
        if self.0.target == self.0.inventory.platform
            && let RepresentationData::Inline(bytes) = &rep.data
        {
            return Ok(Box::new(Pinned {
                reader: io::Cursor::new(bytes.clone()),
                _batch: batch,
                _read: read,
                node: None,
            }));
        }
        let size = self
            .0
            .representations
            .get(&id)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        let reader = regular(&batch, &format!("r-{}", id.0), *size)?;
        Ok(Box::new(Pinned {
            reader,
            _batch: batch,
            _read: read,
            node: None,
        }))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(remote::NoTranslation)
    }
    fn local_batch(&self) -> Option<Batch> {
        self.publication_pin()
    }
    fn namespace(&self) -> Option<Arc<dyn namespace::Namespace>> {
        (!self.0.inventory.namespace_roots_for(self.0.target).is_empty())
            .then(|| Arc::new(self.clone()) as Arc<dyn namespace::Namespace>)
    }
}
impl namespace::Namespace for Completed {
    fn origin(&self) -> Option<oink_foundation::DeviceId> {
        Some(self.0.inventory.id.clip.origin)
    }
    fn roots(&self) -> Vec<FileNode> {
        self.0
            .inventory
            .roots
            .iter()
            .map(|n| self.0.nodes[self.0.index[&n.id]].clone())
            .collect()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        let (_pin, _read) = match self.read_batch() {
            Ok(pinned) => pinned,
            Err(error) => return namespace::Namespace::list(&self.refetch(error)?, node, cursor),
        };
        if self.node(node)?.kind != NodeKind::Directory {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let page = namespace::page(node, 1, self.0.children.get(&node).map_or(&[], Vec::as_slice), cursor)?;
        if cursor.is_none() && page.entries.is_empty() {
            _pin.node_delivered(node.0)?;
        }
        Ok(page)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        let (_pin, _read) = match self.read_batch() {
            Ok(pinned) => pinned,
            Err(error) => return namespace::Namespace::select(&self.refetch(error)?, node),
        };
        let node = self.node(node)?;
        if node.kind != NodeKind::File {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        Ok(node.clone())
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        let (batch, read) = match self.read_batch() {
            Ok(pinned) => pinned,
            Err(error) => return namespace::Namespace::open(&self.refetch(error)?, node, version, offset),
        };
        let entry = self.node(node)?;
        let ContentSize::Known(size) = entry.size else {
            return Err(invalid());
        };
        if entry.kind != NodeKind::File || entry.version != version || offset > size {
            return Err(unavailable(Unavailable::Superseded));
        }
        let mut reader = regular(&batch, &format!("f-{}", node.0), size)?;
        reader.seek(io::SeekFrom::Start(offset))?;
        Ok(Box::new(Pinned {
            reader,
            _batch: batch,
            _read: read,
            node: (offset == 0).then_some(node.0),
        }))
    }
    fn local_batch(&self) -> Option<Batch> {
        self.publication_pin()
    }
}
struct Pinned<R> {
    reader: R,
    _read: crate::cache::SourceLease,
    _batch: Batch,
    node: Option<u64>,
}
impl<R: Read> Read for Pinned<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = self.reader.read(bytes)?;
        if n == 0
            && !bytes.is_empty()
            && let Some(node) = self.node.take()
        {
            self._batch.node_delivered(node)?;
        }
        Ok(n)
    }
}

/// Whole-batch consumption is distinct from receiving/committing network bytes.
/// Empty directories also need an enumeration; a partial file or one sibling is
/// insufficient. Markers survive a crash between delivery and superseding.
pub(crate) fn batch_delivered(path: &Path, expected: Option<[u8; 32]>) -> bool {
    let Ok(bytes) = fs::read(path.join("manifest")) else {
        return false;
    };
    if expected != Some(*blake3::hash(&bytes).as_bytes()) {
        return false;
    }
    let Ok(manifest) = super::manifest::Manifest::decode(&bytes) else {
        return false;
    };
    if manifest.nodes.is_empty() {
        return false;
    }
    let parents: HashSet<_> = manifest.nodes.iter().filter_map(|n| n.parent).collect();
    manifest
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File || !parents.contains(&n.id))
        .all(|n| path.join(format!(".delivered-{}", n.id.0)).is_file())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use namespace::Namespace;

    #[test]
    fn recovery_reaps_an_inactive_publication_part_and_retains_verified_content() {
        let data = tempfile::tempdir().unwrap();
        let batch = fixture(data.path());
        let id = batch.id();
        let partial = batch.path().join("published.part");
        fs::create_dir(&partial).unwrap();
        fs::write(partial.join("unfinished"), b"part").unwrap();
        crate::store::recover(data.path());
        assert!(partial.exists(), "active owners retain their staging area");
        drop(batch);
        crate::store::recover(data.path());
        assert!(!partial.exists());
        let complete = Completed::restore(data.path(), id).unwrap();
        let files = complete.materialize().unwrap();
        assert_eq!(fs::read(files[0].local_path.as_ref().unwrap()).unwrap(), b"cached");
    }
    #[test]
    fn materialized_delivery_is_local_and_uses_the_completed_batch() {
        let data = tempfile::tempdir().unwrap();
        let batch = fixture(data.path());
        let complete = Completed::restore(data.path(), batch.id()).unwrap();
        let files = complete.materialize().unwrap();
        assert_eq!(files.len(), 1);
        let path = files[0].local_path.as_ref().unwrap();
        assert!(path.starts_with(batch.path().join("published")));
        assert_eq!(fs::read(path).unwrap(), b"cached");
        assert_eq!(complete.materialize().unwrap(), files);
    }
    pub(super) fn fixture(data: &Path) -> Batch {
        fixture_many(data, 1)
    }
    fn fixture_many(data: &Path, count: u64) -> Batch {
        let batch = Batch::create(data, StoreKind::CompletedDelivery, BatchId([91; 16])).unwrap();
        fs::write(batch.path().join("f-1"), b"cached").unwrap();
        let node = FileNode {
            id: NodeId(1),
            parent: None,
            name: "file".into(),
            kind: NodeKind::File,
            size: ContentSize::Known(6),
            version: 7,
            modified_ms: None,
        };
        let mut manifest = super::manifest::Manifest {
            inventory: crate::wire::inventory(&Inventory {
                id: OfferId {
                    clip: oink_foundation::content::ClipId {
                        origin: oink_foundation::DeviceId([1; 32]),
                        seq: 1,
                    },
                    generation: 1,
                },
                platform: ClipPlatform::Mac,
                items: vec![],
                roots: vec![node.clone()],
            })
            .unwrap(),
            target: ClipPlatform::Mac,
            nodes: vec![node],
            files: vec![remote::tree::FileRecord {
                node: NodeId(1),
                version: 7,
                size: 6,
                hash: *blake3::hash(b"cached").as_bytes(),
            }],
            representations: vec![],
        };
        for id in 2..=count {
            let mut node = manifest.nodes[0].clone();
            node.id = NodeId(id);
            node.name = format!("file{id}");
            let mut inventory = crate::wire::local_inventory(&manifest.inventory).unwrap();
            inventory.roots.push(node.clone());
            manifest.inventory = crate::wire::inventory(&inventory).unwrap();
            manifest.nodes.push(node);
            let mut file = manifest.files[0].clone();
            file.node = NodeId(id);
            manifest.files.push(file);
            fs::write(batch.path().join(format!("f-{id}")), b"cached").unwrap();
        }
        batch.commit_manifest(&manifest.encode().unwrap()).unwrap();
        batch
    }

    #[test]
    fn retention_waits_for_every_file_in_the_batch() {
        let data = tempfile::tempdir().unwrap();
        let owned = fixture_many(data.path(), 2);
        let path = owned.path().to_owned();
        let local = Completed::restore(data.path(), owned.id()).unwrap();
        drop(owned);
        Namespace::open(&local, NodeId(1), 7, 0)
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap();
        crate::store::sweep(data.path());
        assert!(path.exists(), "one completed sibling is not the whole batch");
        Namespace::open(&local, NodeId(2), 7, 0)
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap();
        crate::store::sweep(data.path());
        assert!(!path.exists());
    }

    #[test]
    fn retention_partial_and_empty_buffer_reads_are_not_consumption() {
        let data = tempfile::tempdir().unwrap();
        let owned = fixture(data.path());
        let path = owned.path().to_owned();
        let local = Completed::restore(data.path(), owned.id()).unwrap();
        let mut reader = Namespace::open(&local, NodeId(1), 7, 0).unwrap();
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        assert_eq!(reader.read(&mut [0; 1]).unwrap(), 1);
        drop((reader, owned));
        crate::store::sweep(data.path());
        assert!(path.exists());
        let mut suffix = Namespace::open(&local, NodeId(1), 7, 1).unwrap();
        suffix.read_to_end(&mut Vec::new()).unwrap();
        drop(suffix);
        crate::store::sweep(data.path());
        assert!(
            path.exists(),
            "a suffix alone does not establish whole-file consumption"
        );
    }

    #[test]
    fn retention_eof_supersede_and_provider_rerequest() {
        let data = tempfile::tempdir().unwrap();
        let owned = fixture(data.path());
        let path = owned.path().to_owned();
        let local = Completed::restore(data.path(), owned.id()).unwrap();
        let mut reader = Namespace::open(&local, NodeId(1), 7, 0).unwrap();
        drop(owned);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        crate::store::sweep(data.path());
        assert!(path.exists());
        drop(reader);
        crate::store::sweep(data.path());
        assert!(!path.exists());
        let error = Namespace::open(&local, NodeId(1), 7, 0).err().unwrap();
        assert_eq!(reason(&error), Unavailable::NoSuchItem);
    }

    #[test]
    fn restored_catalog_is_dormant_but_each_reader_pins_bytes_until_release() {
        let data = tempfile::tempdir().unwrap();
        let batch = fixture(data.path());
        let restored = Completed::restore(data.path(), batch.id()).unwrap();
        drop(batch);
        assert_eq!(
            crate::store::usage(data.path(), StoreKind::CompletedDelivery, false)
                .0
                .kept_bytes,
            0
        );
        let mut reader = Namespace::open(&restored, NodeId(1), 7, 2).unwrap();
        assert_eq!(
            crate::store::usage(data.path(), StoreKind::CompletedDelivery, true).1,
            0
        );
        assert_eq!(read_all(&mut reader).unwrap(), b"ched");
        drop(reader);
        assert!(crate::store::usage(data.path(), StoreKind::CompletedDelivery, true).1 > 0);
        assert!(Namespace::select(&restored, NodeId(1)).is_err());
        assert!(Namespace::open(&restored, NodeId(1), 7, 0).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ordinary_published_files_are_editable_copies_of_verified_cache() {
        let data = tempfile::tempdir().unwrap();
        let batch = fixture(data.path());
        let restored = Completed::restore(data.path(), batch.id()).unwrap();
        let entries = restored.materialize().unwrap();
        let path = entries[0].local_path.as_ref().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"cached");
        fs::write(path, b"edited").unwrap();
        assert_eq!(fs::read(batch.path().join("f-1")).unwrap(), b"cached");
    }

    #[test]
    fn restart_rejects_changed_manifest_and_missing_or_truncated_payload() {
        let data = tempfile::tempdir().unwrap();
        let batch = fixture(data.path());
        let path = batch.path().join("manifest");
        let original = fs::read(&path).unwrap();
        fs::write(&path, b"invalid receipt").unwrap();
        assert!(Completed::restore(data.path(), batch.id()).is_err());
        fs::write(&path, original).unwrap();
        fs::write(batch.path().join("f-1"), b"short").unwrap();
        assert!(Completed::restore(data.path(), batch.id()).is_err());
        fs::remove_file(batch.path().join("f-1")).unwrap();
        assert!(Completed::restore(data.path(), batch.id()).is_err());
    }

    #[test]
    fn receipt_does_not_make_an_orphan_or_missing_file_table_readable() {
        let data = tempfile::tempdir().unwrap();
        let batch = fixture(data.path());
        let mut manifest: super::manifest::Manifest =
            super::manifest::Manifest::decode(&batch.manifest().unwrap()).unwrap();
        manifest.nodes[0].parent = Some(NodeId(20));
        let invalid = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([92; 16])).unwrap();
        fs::write(invalid.path().join("f-1"), b"cached").unwrap();
        invalid.commit_manifest(&manifest.encode().unwrap()).unwrap();
        assert!(Completed::restore(data.path(), invalid.id()).is_err());
        manifest.nodes[0].parent = None;
        manifest.files.clear();
        let absent = Batch::create(data.path(), StoreKind::CompletedDelivery, BatchId([93; 16])).unwrap();
        absent.commit_manifest(&manifest.encode().unwrap()).unwrap();
        assert!(Completed::restore(data.path(), absent.id()).is_err());
    }
}
