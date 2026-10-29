//! Native adapters keep their descriptor index table local. Network identity is
//! always node/version; pagination never changes an already returned OLE index.
use super::*;
use crate::model::{ContentSize, MAX_DISCOVERED_NODES, NodeKind};
use crate::{
    FileContents, FileEntry, FileMetadata,
    sync::files::{TargetOs, sanitize_component},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::OnceLock,
};

type DescriptorTable = (Vec<FileNode>, Vec<FileEntry>);
pub struct Files {
    namespace: Arc<dyn Namespace>,
    target: TargetOs,
    roots: Vec<FileEntry>,
    table: OnceLock<Result<DescriptorTable, Unavailable>>,
}
impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamespaceFiles")
            .field("roots", &self.roots.len())
            .finish()
    }
}
impl Files {
    pub fn new(namespace: Arc<dyn Namespace>, target: TargetOs) -> io::Result<Arc<Self>> {
        let mut names = Names::new(target);
        let roots = namespace
            .roots()
            .iter()
            .filter(|node| target != TargetOs::Windows || node.kind.symlink_target().is_none())
            .map(|node| names.entry(node))
            .collect::<io::Result<_>>()?;
        Ok(Arc::new(Self {
            namespace,
            target,
            roots,
            table: OnceLock::new(),
        }))
    }
    pub fn roots(&self) -> &[FileEntry] {
        &self.roots
    }
    /// Rebuild an already-published native index without enumerating the source.
    #[cfg(any(windows, all(test, debug_assertions)))]
    pub(crate) fn restore_table(
        namespace: Arc<dyn Namespace>,
        target: TargetOs,
        nodes: Vec<FileNode>,
        entries: Vec<FileEntry>,
    ) -> io::Result<Arc<Self>> {
        if nodes.len() != entries.len() || nodes.len() > MAX_DISCOVERED_NODES {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let mut ids = HashSet::new();
        let mut directories = HashSet::new();
        let mut paths = HashSet::new();
        let mut roots = Vec::new();
        for (node, entry) in nodes.iter().zip(&entries) {
            if !node.valid()
                || !ids.insert(node.id)
                || node.parent.is_some_and(|parent| !directories.contains(&parent))
                || entry.is_dir != (node.kind == NodeKind::Directory)
                || entry.local_path.is_some()
                || entry.path.is_empty()
                || entry.path.len() > 32000
                || entry.path.contains(['\\', '\0'])
                || (target == TargetOs::Windows && (entry.path.contains(':') || node.kind.symlink_target().is_some()))
                || node.kind.symlink_target().is_some_and(|target| {
                    !oink_foundation::content::safe_symlink_target(
                        target,
                        entry.path.split('/').count().saturating_sub(1),
                    )
                })
                || entry
                    .path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || !paths.insert(entry.path.to_lowercase())
            {
                return Err(unavailable(Unavailable::InvalidData));
            }
            if node.kind == NodeKind::Directory {
                directories.insert(node.id);
            }
            if node.parent.is_none() {
                roots.push(entry.clone());
            }
        }
        let expected: HashSet<_> = namespace.roots().iter().map(|node| node.id).collect();
        if expected
            != nodes
                .iter()
                .filter(|node| node.parent.is_none())
                .map(|node| node.id)
                .collect()
        {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let table = OnceLock::new();
        let _ = table.set(Ok((nodes, entries)));
        Ok(Arc::new(Self {
            namespace,
            target,
            roots,
            table,
        }))
    }
    fn table(&self) -> io::Result<&(Vec<FileNode>, Vec<FileEntry>)> {
        self.table
            .get_or_init(|| self.expand().map_err(|e| super::super::reason(&e)))
            .as_ref()
            .map_err(|reason| unavailable(*reason))
    }
    fn expand(&self) -> io::Result<(Vec<FileNode>, Vec<FileEntry>)> {
        self.namespace.prepare()?;
        self.namespace.preparing();
        let mut names = Names::new(self.target);
        let mut nodes = Vec::new();
        let mut entries = Vec::new();
        let mut queue: VecDeque<_> = self.namespace.roots().into_iter().map(|n| (n, 0usize)).collect();
        let mut seen = HashSet::new();
        while let Some((node, depth)) = queue.pop_front() {
            if self.target == TargetOs::Windows && node.kind.symlink_target().is_some() {
                continue;
            }
            if depth > 256 || nodes.len() >= MAX_DISCOVERED_NODES || !seen.insert(node.id) {
                return Err(unavailable(Unavailable::LimitExceeded));
            }
            entries.push(names.entry(&node)?);
            if node.kind == NodeKind::Directory {
                let mut cursor = None;
                loop {
                    // A retained descriptor may first expand after source release.
                    // Prefer the received catalog once the drop has authority;
                    // hover still enumerates metadata without hydrating a copy.
                    let page = if self.namespace.source_link().is_some_and(|link| link.provisional) {
                        self.namespace.list(node.id, cursor)
                    } else {
                        self.namespace
                            .native_list(node.id, cursor)
                            .unwrap_or_else(|| self.namespace.list(node.id, cursor))
                    }?;
                    page.validate(cursor).map_err(unavailable)?;
                    queue.extend(page.entries.into_iter().map(|child| (child, depth + 1)));
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
            }
            nodes.push(node);
        }
        Ok((nodes, entries))
    }
    fn selected(&self, index: usize) -> io::Result<FileNode> {
        let published = self
            .table()?
            .0
            .get(index)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        let selected = self.namespace.select(published.id)?;
        if self.target == TargetOs::Windows
            && (matches!(published.size, ContentSize::Known(size) if selected.size != ContentSize::Known(size))
                || (published.version != 0 && published.version != selected.version))
        {
            return Err(unavailable(Unavailable::Superseded));
        }
        Ok(selected)
    }
}
impl FileContents for Files {
    fn descriptor_entries(&self) -> io::Result<Option<Vec<FileEntry>>> {
        Ok(Some(self.table()?.1.clone()))
    }
    fn descriptor_nodes(&self) -> io::Result<Option<Vec<FileNode>>> {
        Ok(Some(self.table()?.0.clone()))
    }
    fn descriptor_size_known(&self, index: usize) -> bool {
        self.table().is_ok_and(|table| {
            table
                .0
                .get(index)
                .is_some_and(|node| matches!(node.size, ContentSize::Known(_)))
        })
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        Some(self.namespace.clone())
    }
    fn open(&self, index: usize) -> io::Result<FileReader> {
        let published = self
            .table()?
            .0
            .get(index)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if let Some(reader) = self.namespace.native_open(published.id, 0) {
            return reader;
        }
        let selected = self.selected(index)?;
        self.namespace.open(selected.id, selected.version, 0)
    }
    fn metadata(&self, index: usize) -> io::Result<Option<FileMetadata>> {
        let published = self
            .table()?
            .0
            .get(index)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if let Some(metadata) = self.namespace.native_metadata(published.id) {
            return metadata.map(Some);
        }
        let entry = self.selected(index)?;
        let ContentSize::Known(size) = entry.size else {
            return Err(unavailable(Unavailable::InvalidData));
        };
        Ok(Some(FileMetadata {
            exec_bits: entry.exec_bits,
            size,
            modified_ms: entry.modified_ms,
        }))
    }
    fn placeholder_metadata(&self, index: usize) -> io::Result<Option<(FileMetadata, u64)>> {
        let node = self
            .table()?
            .0
            .get(index)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        let current = self.namespace.select(node.id)?;
        let ContentSize::Known(size) = current.size else {
            return Err(unavailable(Unavailable::InvalidData));
        };
        Ok(Some((
            FileMetadata {
                exec_bits: current.exec_bits,
                size,
                modified_ms: current.modified_ms,
            },
            current.version,
        )))
    }
    fn open_placeholder(&self, index: usize, version: u64) -> io::Result<FileReader> {
        let node = self
            .table()?
            .0
            .get(index)
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        self.namespace.open(node.id, version, 0)
    }
    fn cache_in_use(&self) -> bool {
        self.namespace.in_use()
    }
    fn cancel(&self) {
        self.namespace.cancel();
    }
}

pub(crate) struct Names {
    target: TargetOs,
    paths: HashMap<NodeId, String>,
    used: HashMap<Option<NodeId>, HashSet<String>>,
}
impl Names {
    pub fn new(target: TargetOs) -> Self {
        Self {
            target,
            paths: HashMap::new(),
            used: HashMap::new(),
        }
    }
    pub fn entry(&mut self, node: &FileNode) -> io::Result<FileEntry> {
        if !node.valid() {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let path = if let Some(path) = self.paths.get(&node.id) {
            path.clone()
        } else {
            let used = self.used.entry(node.parent).or_default();
            let base = sanitize_component(&node.name, self.target);
            let mut name = base.clone();
            let mut number = 1;
            while !used.insert(name.to_lowercase()) {
                number += 1;
                name = crate::sync::files::names::with_suffix(&base, number, self.target);
            }
            let path = match node.parent {
                Some(parent) => format!(
                    "{}/{}",
                    self.paths
                        .get(&parent)
                        .ok_or_else(|| unavailable(Unavailable::InvalidData))?,
                    name
                ),
                None => name,
            };
            self.paths.insert(node.id, path.clone());
            path
        };
        if node.kind.symlink_target().is_some_and(|target| {
            !oink_foundation::content::safe_symlink_target(target, path.split('/').count().saturating_sub(1))
        }) {
            return Err(unavailable(Unavailable::InvalidData));
        }
        Ok(FileEntry {
            path,
            is_dir: node.kind == NodeKind::Directory,
            size: match node.size {
                ContentSize::Known(n) => n,
                ContentSize::Unknown => 0,
            },
            modified_ms: node.modified_ms,
            local_path: None,
        })
    }
}

/// Adapter for normalized local/test providers. File Provider still uses the
/// node/page service; no second indexed loopback protocol is kept alive.
#[cfg(target_os = "macos")]
pub(crate) struct Flat {
    nodes: Vec<FileNode>,
    contents: Arc<dyn FileContents>,
}
#[cfg(target_os = "macos")]
impl Flat {
    pub fn new(entries: &[FileEntry], contents: Arc<dyn FileContents>) -> io::Result<Arc<Self>> {
        let ids: HashMap<_, _> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.as_str(), NodeId(i as u64 + 1)))
            .collect();
        let nodes = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let (parent, name) = match entry.path.rsplit_once('/') {
                    Some((parent, name)) => (
                        Some(*ids.get(parent).ok_or_else(|| unavailable(Unavailable::InvalidData))?),
                        name,
                    ),
                    None => (None, entry.path.as_str()),
                };
                let node = FileNode {
                    exec_bits: None,
                    id: NodeId(i as u64 + 1),
                    parent,
                    name: name.into(),
                    kind: if entry.is_dir {
                        NodeKind::Directory
                    } else {
                        NodeKind::File
                    },
                    size: if entry.is_dir {
                        ContentSize::Unknown
                    } else {
                        ContentSize::Known(entry.size)
                    },
                    version: 1,
                    modified_ms: entry.modified_ms,
                };
                if !node.valid() {
                    return Err(unavailable(Unavailable::InvalidData));
                }
                Ok(node)
            })
            .collect::<io::Result<_>>()?;
        Ok(Arc::new(Self { nodes, contents }))
    }
}
#[cfg(target_os = "macos")]
impl Namespace for Flat {
    fn roots(&self) -> Vec<FileNode> {
        self.nodes.iter().filter(|n| n.parent.is_none()).cloned().collect()
    }
    fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        if self
            .nodes
            .iter()
            .find(|n| n.id == id)
            .is_none_or(|n| n.kind != NodeKind::Directory)
        {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let children: Vec<_> = self.nodes.iter().filter(|n| n.parent == Some(id)).cloned().collect();
        super::page(id, 1, &children, cursor)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        let mut node = self
            .nodes
            .iter()
            .find(|n| n.id == id && n.kind == NodeKind::File)
            .cloned()
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        if let Some(meta) = self.contents.metadata((id.0 - 1) as usize)? {
            node.size = ContentSize::Known(meta.size);
            node.modified_ms = meta.modified_ms;
            node.exec_bits = meta.exec_bits;
        }
        Ok(node)
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.select(id)?;
        if version != 1 || offset != 0 {
            return Err(unavailable(Unavailable::InvalidData));
        }
        self.contents.open((id.0 - 1) as usize)
    }
    fn cancel(&self) {
        self.contents.cancel();
    }
    fn in_use(&self) -> bool {
        self.contents.cache_in_use()
    }
}
