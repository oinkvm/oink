//! Shared clipboard/DnD inventory and file namespace. Vector order is native order.
//! Format names and node names are business data, never diagnostic labels.
use oink_foundation::clipboard::ClipPlatform;

pub const AUTO_PULL_BYTES: u64 = 16 * 1024 * 1024;
pub const REPRESENTATION_BYTES: u64 = 256 * 1024 * 1024;
pub const SNAPSHOT_BYTES: u64 = 1024 * 1024 * 1024;
pub const INVENTORY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REPRESENTATIONS: usize = 65_535;
pub const DIRECTORY_PAGE_ENTRIES: usize = 1024;
pub const DIRECTORY_PAGE_BYTES: usize = 1024 * 1024;
pub const MAX_DISCOVERED_NODES: usize = 100_000;
pub const NO_PROGRESS_MS: u64 = 300_000;
pub const PROMISE_EXPORT_MS: u64 = 3000;

pub use oink_foundation::content::{
    ClipId, ContentItem, ContentSize, DeliveryId, FileNode, Holder, ItemId, Medium, NativeFormat, NodeId, NodeKind,
    OfferId, Representation, RepresentationData, RepresentationId, StandardFormat, Unavailable,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    pub id: OfferId,
    pub platform: ClipPlatform,

    pub items: Vec<ContentItem>,

    pub roots: Vec<FileNode>,
}
impl Inventory {
    /// File-service roots, including private promise containers. Native adapters
    /// publish only `roots`; promise containers are protocol capabilities and
    /// must never be turned into user-visible filenames or placeholder URLs.
    pub fn namespace_roots(&self) -> Vec<FileNode> {
        self.namespace_roots_for(self.platform)
    }
    pub fn namespace_roots_for(&self, target: ClipPlatform) -> Vec<FileNode> {
        let mut roots = self.roots.clone();
        let mut seen: std::collections::HashSet<_> = roots.iter().map(|n| n.id).collect();
        for rep in self.wanted(target) {
            if let RepresentationData::Promise(id, _) = rep.data
                && seen.insert(id)
            {
                roots.push(FileNode {
                    id,
                    parent: None,
                    name: "_promise".into(),
                    kind: NodeKind::Directory,
                    size: ContentSize::Unknown,
                    version: 1,
                    modified_ms: None,
                });
            }
        }
        roots
    }
    pub fn has_promises(&self) -> bool {
        self.items
            .iter()
            .flat_map(|item| &item.representations)
            .any(|rep| matches!(rep.data, RepresentationData::Promise(..)))
    }

    /// Projection retains item/representation order. File delivery identities are
    /// adapted by the target; native source paths are never published there.
    pub fn wanted(&self, target: ClipPlatform) -> impl Iterator<Item = &Representation> {
        self.items.iter().flat_map(move |item| {
            item.representations.iter().filter(move |rep| {
                target != ClipPlatform::Unknown
                    && !matches!(rep.data, RepresentationData::Unavailable(_))
                    && ((target == self.platform && rep.format.platform() != ClipPlatform::Unknown)
                        || matches!(rep.data, RepresentationData::File(_))
                        || rep.standard == Some(StandardFormat::Privacy)
                        || ((item.file.is_none() || item.virtual_files) && rep.standard.is_some()))
            })
        })
    }
    /// Including inline bytes, all wanted non-file representations share ONE
    /// budget. Unknown sizes require demand; no per-format budget multiplication.
    pub fn automatic_pull(&self, target: ClipPlatform) -> bool {
        automatic_pull(
            self.wanted(target).filter_map(|rep| {
                let size = rep.data.size()?;
                if target == self.platform {
                    return Some(size);
                }
                // Foreign encoding can expand text and image bytes. Use a known
                // conservative translation bound; compressed image conversions have
                // no such bound and therefore wait for native demand.
                let bound = match (rep.standard, &rep.format, size) {
                    (Some(StandardFormat::Url), _, ContentSize::Known(_)) => {
                        ContentSize::Known(crate::content::url::MAX_WIRE_BYTES as u64)
                    }
                    (Some(StandardFormat::Text | StandardFormat::Html), _, ContentSize::Known(n)) => {
                        n.checked_mul(3).map(ContentSize::Known).unwrap_or(ContentSize::Unknown)
                    }
                    (Some(StandardFormat::Png), NativeFormat::Mac(name), _) if name != "public.png" => {
                        ContentSize::Unknown
                    }
                    (Some(StandardFormat::Png), NativeFormat::WindowsPredefined(_), _) => ContentSize::Unknown,
                    _ => size,
                };
                Some(bound)
            }),
            AUTO_PULL_BYTES,
        )
    }
    pub fn validate(&self) -> Result<(), Unavailable> {
        use std::collections::HashSet;
        let mut items = HashSet::new();
        let mut reps = HashSet::new();
        let mut roots = HashSet::new();
        if self
            .roots
            .iter()
            .any(|root| !root.valid() || root.parent.is_some() || !roots.insert(root.id))
        {
            return Err(Unavailable::InvalidData);
        }
        let mut promises = std::collections::HashMap::new();
        for item in &self.items {
            if !items.insert(item.id) {
                return Err(Unavailable::InvalidData);
            }
            for rep in &item.representations {
                if !reps.insert(rep.id)
                    || !rep.format.valid()
                    || (rep.format.platform() != ClipPlatform::Unknown
                        && self.platform != ClipPlatform::Unknown
                        && rep.format.platform() != self.platform)
                {
                    return Err(Unavailable::InvalidData);
                }
                if let RepresentationData::Promise(node, _) = rep.data
                    && (self.platform != ClipPlatform::Mac
                        || roots.contains(&node)
                        || rep.standard.is_some()
                        || promises.insert(node, item.id).is_some_and(|old| old != item.id))
                {
                    return Err(Unavailable::InvalidData);
                }
                if matches!(rep.data.size(), Some(ContentSize::Known(n)) if n > REPRESENTATION_BYTES) {
                    return Err(Unavailable::LimitExceeded);
                }
            }
        }
        if reps.len() > MAX_REPRESENTATIONS || self.items.len() > MAX_REPRESENTATIONS {
            return Err(Unavailable::LimitExceeded);
        }
        let encoded = crate::wire::inventory(self)
            .and_then(|value| oink_protocol::v1::schema::WireDto::encode(&value, INVENTORY_BYTES))
            .map_err(|_| Unavailable::InvalidData)?;
        if encoded.len() > INVENTORY_BYTES {
            return Err(Unavailable::LimitExceeded);
        }
        Ok(())
    }
}

pub fn automatic_pull(sizes: impl IntoIterator<Item = ContentSize>, budget: u64) -> bool {
    sizes
        .into_iter()
        .try_fold(0_u64, |total, size| match size {
            ContentSize::Known(n) => total.checked_add(n).filter(|sum| *sum <= budget),
            ContentSize::Unknown => None,
        })
        .is_some()
}

/// A cursor belongs to one immutable direct-child view, never a live iterator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryCursor {
    pub version: u64,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryPage {
    pub directory: NodeId,
    pub version: u64,

    pub entries: Vec<FileNode>,
    /// None is completion, not preparing. Preparing is a distinct response.
    pub next: Option<DirectoryCursor>,
}
impl DirectoryPage {
    pub fn validate(&self, cursor: Option<DirectoryCursor>) -> Result<(), Unavailable> {
        let offset = cursor.map_or(0, |c| c.offset);
        if cursor.is_some_and(|c| c.version != self.version) {
            return Err(Unavailable::Superseded);
        }
        let end = offset
            .checked_add(self.entries.len() as u32)
            .ok_or(Unavailable::LimitExceeded)?;
        let mut ids = std::collections::HashSet::new();
        if self
            .entries
            .iter()
            .any(|e| !e.valid() || e.parent != Some(self.directory) || !ids.insert(e.id))
            || self
                .next
                .is_some_and(|c| c.version != self.version || c.offset != end || c.offset == offset)
        {
            return Err(Unavailable::InvalidData);
        }
        if self.entries.len() > DIRECTORY_PAGE_ENTRIES
            || crate::wire::page(self)
                .and_then(|value| oink_protocol::v1::schema::WireDto::encode(&value, DIRECTORY_PAGE_BYTES))
                .map_err(|_| Unavailable::InvalidData)?
                .len()
                > DIRECTORY_PAGE_BYTES
        {
            return Err(Unavailable::LimitExceeded);
        }
        Ok(())
    }
}

/// Hover may read an active provider but never authorizes source native Drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Demand {
    Clipboard,
    Hover(oink_foundation::content::Holder),
    Drop(oink_foundation::content::Holder),
}

#[derive(Clone, PartialEq, Eq)]
pub enum ContentMsg {
    BeginDelivery {
        delivery: DeliveryId,
        demand: Demand,
    },
    DeliveryReady {
        offer: OfferId,
        delivery: DeliveryId,
    },
    ReadRepresentation {
        offer: OfferId,
        representation: RepresentationId,
        item: ItemId,
        accepted_format: String,
        target: ClipPlatform,
        demand: Demand,
    },
    RepresentationBytes {
        offer: OfferId,
        representation: RepresentationId,
        offset: u64,

        bytes: Vec<u8>,
    },
    RepresentationEnd {
        offer: OfferId,
        representation: RepresentationId,
        size: u64,
        hash: [u8; 32],
    },
    RepresentationFailed {
        offer: OfferId,
        representation: RepresentationId,
        reason: Unavailable,
    },
    ListDirectory {
        offer: OfferId,
        node: NodeId,
        cursor: Option<DirectoryCursor>,
        demand: Demand,
    },
    Directory {
        offer: OfferId,
        page: DirectoryPage,
    },
    Preparing {
        offer: OfferId,
        node: NodeId,
    },
    DirectoryFailed {
        offer: OfferId,
        node: NodeId,
        reason: Unavailable,
    },
    SelectFile {
        offer: OfferId,
        node: NodeId,
        demand: Demand,
    },
    FileSelected {
        offer: OfferId,
        entry: FileNode,
    },
    ReadFile {
        offer: OfferId,
        node: NodeId,
        version: u64,
        offset: u64,
        /// Verified last block of a resumed prefix. A mismatch fails this
        /// operation; it cannot silently mix the old prefix and new suffix.
        check: Option<[u8; 32]>,
        demand: Demand,
    },
    FileBlock {
        offer: OfferId,
        node: NodeId,
        version: u64,
        offset: u64,
        hash: [u8; 32],

        bytes: Vec<u8>,
    },
    FileEnd {
        offer: OfferId,
        node: NodeId,
        version: u64,
        size: u64,
    },
    FileFailed {
        offer: OfferId,
        node: NodeId,
        reason: Unavailable,
    },
    /// Current real-file metadata, selected before reading. Receiving this
    /// reply permits native placeholders to update their previously shown size,
    /// mtime and version; immutable virtual/Drop descriptors use FileSelected.
    FileVersion {
        offer: OfferId,
        entry: FileNode,
    },
    /// Sent only after the whole target manifest and bytes are durable and all
    /// target-facing readers can resolve locally. Native accepted is NOT stored.
    TargetStored {
        delivery: DeliveryId,
        receipt: [u8; 32],
    },
    Failed {
        delivery: DeliveryId,
        reason: Unavailable,
    },
    TerminalAck {
        delivery: DeliveryId,
        receipt: Option<[u8; 32]>,
    },
}

impl ContentMsg {
    pub fn request(&self) -> Option<(OfferId, Demand)> {
        match self {
            Self::BeginDelivery { delivery, demand } => Some((delivery.offer, *demand)),
            Self::ReadRepresentation { offer, demand, .. }
            | Self::ListDirectory { offer, demand, .. }
            | Self::SelectFile { offer, demand, .. }
            | Self::ReadFile { offer, demand, .. } => Some((*offer, *demand)),
            _ => None,
        }
    }
}

impl std::fmt::Debug for ContentMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A receipt is content-derived. Do not put hashes, names or paths in
        // protocol traces (including traces of an outer Message).
        f.write_str(match self {
            Self::BeginDelivery { .. } => "BeginDelivery",
            Self::DeliveryReady { .. } => "DeliveryReady",
            Self::ReadRepresentation { .. } => "ReadRepresentation",
            Self::RepresentationBytes { .. } => "RepresentationBytes",
            Self::RepresentationEnd { .. } => "RepresentationEnd",
            Self::RepresentationFailed { .. } => "RepresentationFailed",
            Self::ListDirectory { .. } => "ListDirectory",
            Self::Directory { .. } => "Directory",
            Self::Preparing { .. } => "Preparing",
            Self::DirectoryFailed { .. } => "DirectoryFailed",
            Self::SelectFile { .. } => "SelectFile",
            Self::FileSelected { .. } => "FileSelected",
            Self::FileVersion { .. } => "FileVersion",
            Self::ReadFile { .. } => "ReadFile",
            Self::FileBlock { .. } => "FileBlock",
            Self::FileEnd { .. } => "FileEnd",
            Self::FileFailed { .. } => "FileFailed",
            Self::TargetStored { .. } => "TargetStored",
            Self::Failed { .. } => "Failed",
            Self::TerminalAck { .. } => "TerminalAck",
        })
    }
}
