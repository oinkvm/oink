//! Shared clipboard/DnD inventory and file namespace. Vector order is native order.
//! Format names and node names are business data, never diagnostic labels.
use oink_foundation::clipboard::ClipPlatform;

pub const AUTO_PULL_BYTES: u64 = oink_protocol::v2::limits::AUTO_FETCH as u64;
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
    /// Reconcile native file items with the usable namespace without changing
    /// item/representation IDs (native readers still use their original slots).
    pub(crate) fn with_roots(mut self, roots: Vec<FileNode>) -> Self {
        let ids: std::collections::HashSet<_> = roots.iter().map(|root| root.id).collect();
        // CF_HDROP represents the entire Windows selection in one native item.
        // Its first-root anchor is only a group handle, unlike per-file Mac items.
        // Keep original native slots while rebinding a skipped anchor.
        if self.platform == ClipPlatform::Windows {
            for item in &mut self.items {
                if item.file.is_some_and(|id| !ids.contains(&id))
                    && !item.virtual_files
                    && item.representations.iter().any(|rep| {
                        rep.format == NativeFormat::WindowsPredefined(15)
                            && matches!(rep.data, RepresentationData::File(_))
                    })
                    && let Some(root) = roots.first()
                {
                    item.file = Some(root.id);
                    for rep in &mut item.representations {
                        if matches!(rep.data, RepresentationData::File(_)) {
                            rep.data = RepresentationData::File(root.id);
                        }
                    }
                }
            }
        }
        self.items.retain(|item| item.file.is_none_or(|id| ids.contains(&id)));
        self.roots = roots;
        self
    }

    /// File-service roots, including private promise containers. Native adapters
    /// publish only `roots`; promise containers are protocol capabilities and
    /// must never be turned into user-visible filenames or placeholder URLs.
    pub fn namespace_roots(&self) -> Vec<FileNode> {
        self.namespace_roots_for(self.platform)
    }
    pub fn namespace_roots_for(&self, target: ClipPlatform) -> Vec<FileNode> {
        let mut roots: Vec<_> = self
            .roots
            .iter()
            .filter(|node| target != ClipPlatform::Windows || node.kind.symlink_target().is_none())
            .cloned()
            .collect();
        let mut seen: std::collections::HashSet<_> = roots.iter().map(|n| n.id).collect();
        // A fulfilled promise keeps its native controls, but its actual roots
        // are already published. The process-local container is no longer a
        // separate source grant or a user-visible received directory.
        seen.extend(self.items.iter().filter(|item| item.file.is_some()).flat_map(|item| {
            item.representations.iter().filter_map(|rep| match rep.data {
                RepresentationData::Promise(anchor, _) => Some(anchor),
                _ => None,
            })
        }));
        for rep in self.items.iter().flat_map(|item| &item.representations) {
            if target != ClipPlatform::Unknown
                && let RepresentationData::Promise(id, _) = rep.data
                && seen.insert(id)
            {
                roots.push(FileNode {
                    exec_bits: None,
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

    /// The file root whose picture the item at `index` is.
    ///
    /// An image file published with its decoded bitmap (a phone's photos on
    /// the PC that received them) is listed as an anchor item for the root —
    /// `file` set, no representations — directly followed by an item whose
    /// representations are all bitmaps. The bitmap cannot sit on the file item
    /// itself: standard formats of a file item are its icon and are never
    /// published (`wanted`). A native writer that carries several bitmaps
    /// puts each one on its root's own native item (macOS: one pasteboard item
    /// per image with its PNG/TIFF and file URL); Windows has one bitmap per
    /// clipboard and takes the first.
    pub fn bitmap_root(&self, index: usize) -> Option<NodeId> {
        let item = self.items.get(index)?;
        let anchor = self.items.get(index.checked_sub(1)?)?;
        let root = anchor.file?;
        (item.file.is_none()
            && !item.virtual_files
            && !item.representations.is_empty()
            && item
                .representations
                .iter()
                .all(|rep| rep.standard == Some(StandardFormat::Png))
            && !anchor.virtual_files
            && anchor.representations.is_empty()
            && self.roots.iter().any(|node| node.id == root))
        .then_some(root)
    }

    /// Projection retains item/representation order. File delivery identities are
    /// adapted by the target; native source paths are never published there.
    pub fn wanted(&self, target: ClipPlatform) -> impl Iterator<Item = &Representation> {
        let skipped: std::collections::HashSet<_> = self
            .roots
            .iter()
            .filter(|root| target == ClipPlatform::Windows && root.kind.symlink_target().is_some())
            .map(|root| root.id)
            .collect();
        self.items.iter().flat_map(move |item| {
            let skip = item.file.is_some_and(|id| skipped.contains(&id));
            item.representations.iter().filter(move |rep| {
                !skip
                    && target != ClipPlatform::Unknown
                    && !matches!(rep.data, RepresentationData::Unavailable(_))
                    && ((target == self.platform && rep.format.platform() != ClipPlatform::Unknown)
                        || matches!(rep.data, RepresentationData::File(_))
                        || rep.standard == Some(StandardFormat::Privacy)
                        || ((item.file.is_none() || item.virtual_files) && rep.standard.is_some()))
            })
        })
    }
    /// Inline bytes and wanted representations share ONE budget. On a Mac,
    /// ordinary file roots join that same budget; unknown sizes, directories
    /// and links disallow whole-offer automatic fetching.
    pub fn automatic_pull(&self, target: ClipPlatform) -> bool {
        automatic_pull(
            self.wanted(target)
                .filter_map(|rep| {
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
                })
                .chain(self.roots.iter().filter(|_| target == ClipPlatform::Mac).map(|node| {
                    if node.parent.is_none() && node.kind == NodeKind::File && node.valid() {
                        node.size
                    } else {
                        ContentSize::Unknown
                    }
                }))
                .chain((target == ClipPlatform::Mac && self.has_promises()).then_some(ContentSize::Unknown)),
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
            .and_then(|value| oink_protocol::v2::schema::WireDto::encode(&value, INVENTORY_BYTES))
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

#[cfg(all(test, debug_assertions))]
mod automatic_file_tests {
    use super::*;
    use crate::content::Provider;

    fn offer(file_bytes: u64, raw: RepresentationData) -> Inventory {
        let mut inventory = crate::content::tests::fixture().inventory();
        inventory.items[0].representations.truncate(1);
        inventory.items[0].representations[0].data = raw;
        inventory.roots = vec![FileNode {
            id: NodeId(1),
            parent: None,
            name: "file".into(),
            kind: NodeKind::File,
            size: ContentSize::Known(file_bytes),
            version: 1,
            modified_ms: None,
            exec_bits: None,
        }];
        inventory
    }
    #[test]
    fn files_inline_and_readers_share_exactly_one_protocol_budget() {
        assert_eq!(AUTO_PULL_BYTES, oink_protocol::v2::limits::AUTO_FETCH as u64);
        let mut inventory = offer(AUTO_PULL_BYTES - 1, RepresentationData::Inline(vec![1]));
        assert!(inventory.automatic_pull(ClipPlatform::Mac));
        inventory.items[0].representations[0].data = RepresentationData::Inline(vec![1, 2]);
        assert!(!inventory.automatic_pull(ClipPlatform::Mac));
        inventory.items[0].representations[0].data = RepresentationData::Reader {
            size: ContentSize::Known(2),
        };
        assert!(!inventory.automatic_pull(ClipPlatform::Mac));
        inventory.items[0].representations[0].data = RepresentationData::Reader {
            size: ContentSize::Unknown,
        };
        assert!(!inventory.automatic_pull(ClipPlatform::Mac));
    }
    #[test]
    fn file_representation_is_not_counted_twice_and_nonordinary_roots_disallow_auto_fetch() {
        let mut inventory = offer(AUTO_PULL_BYTES, RepresentationData::File(NodeId(1)));
        assert!(inventory.automatic_pull(ClipPlatform::Mac));
        inventory.roots[0].size = ContentSize::Unknown;
        assert!(!inventory.automatic_pull(ClipPlatform::Mac));
        inventory.roots[0].size = ContentSize::Known(0);
        inventory.roots[0].kind = NodeKind::Directory;
        assert!(!inventory.automatic_pull(ClipPlatform::Mac));
        // Windows retains its existing representation-only native delivery policy.
        assert!(inventory.automatic_pull(ClipPlatform::Windows));
    }
    #[test]
    fn foreign_translation_bound_joins_the_file_budget() {
        let mut inventory = offer(
            AUTO_PULL_BYTES - 3,
            RepresentationData::Reader {
                size: ContentSize::Known(1),
            },
        );
        inventory.platform = ClipPlatform::Windows;
        let rep = &mut inventory.items[0].representations[0];
        rep.format = NativeFormat::WindowsPredefined(13);
        rep.standard = Some(StandardFormat::Text);
        assert!(inventory.automatic_pull(ClipPlatform::Mac));
        inventory.roots[0].size = ContentSize::Known(AUTO_PULL_BYTES - 2);
        assert!(!inventory.automatic_pull(ClipPlatform::Mac));
    }
}

/// Inventories in the `Inventory::bitmap_root` shape, shared by the native
/// writer tests of both platforms.
#[cfg(all(test, debug_assertions))]
pub(crate) mod file_images {
    use super::*;

    /// One root per entry, in order; an entry that is `true` is an image and
    /// also gets its bitmap item. Root `i` is `NodeId(i + 1)`; image `i`'s
    /// bitmap is representation `RepresentationId(i)` on item `64 + i`.
    pub(crate) fn inventory(images: &[bool]) -> Inventory {
        let mut items = Vec::new();
        let mut roots = Vec::new();
        for (index, image) in images.iter().enumerate() {
            let index = index as u32;
            let root = NodeId(u64::from(index) + 1);
            roots.push(FileNode {
                id: root,
                parent: None,
                name: if *image {
                    format!("photo-{index}.jpg")
                } else {
                    format!("notes-{index}.pdf")
                },
                kind: NodeKind::File,
                size: ContentSize::Known(4),
                version: 1,
                modified_ms: None,
                exec_bits: None,
            });
            items.push(ContentItem {
                id: ItemId(index),
                file: Some(root),
                virtual_files: false,
                representations: Vec::new(),
            });
            if *image {
                items.push(ContentItem {
                    id: ItemId(64 + index),
                    file: None,
                    virtual_files: false,
                    representations: vec![Representation {
                        id: RepresentationId(index),
                        format: NativeFormat::Unknown(crate::wire::portable(StandardFormat::Png).0),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: Some(StandardFormat::Png),
                        data: RepresentationData::Reader {
                            size: ContentSize::Known(4),
                        },
                    }],
                });
            }
        }
        Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: oink_foundation::DeviceId([3; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Unknown,
            items,
            roots,
        }
    }

    #[test]
    fn each_bitmap_names_the_image_file_before_it() {
        // photo, document, photo, photo
        let inventory = inventory(&[true, false, true, true]);
        assert_eq!(inventory.validate(), Ok(()));
        let paired: Vec<_> = (0..inventory.items.len())
            .filter_map(|index| {
                inventory
                    .bitmap_root(index)
                    .map(|root| (inventory.items[index].id, root))
            })
            .collect();
        assert_eq!(
            paired,
            [
                (ItemId(64), NodeId(1)),
                (ItemId(66), NodeId(3)),
                (ItemId(67), NodeId(4))
            ]
        );
        // The anchors stay file items; their bitmaps are published for every
        // target, the anchors themselves carry nothing.
        for target in [ClipPlatform::Mac, ClipPlatform::Windows] {
            assert_eq!(
                inventory.wanted(target).map(|rep| rep.id).collect::<Vec<_>>(),
                [RepresentationId(0), RepresentationId(2), RepresentationId(3)]
            );
        }
    }

    #[test]
    fn a_bitmap_without_an_empty_anchor_is_not_a_file_picture() {
        let mut inventory = inventory(&[true]);
        // An anchor with its own representations is an ordinary file item
        // (e.g. a Finder file URL): the next item is not its picture.
        let mut anchored = inventory.clone();
        anchored.items[0].representations = anchored.items[1].representations.clone();
        anchored.items[0].representations[0].id = RepresentationId(9);
        assert_eq!(anchored.bitmap_root(1), None);
        // Not every representation a bitmap.
        let mut mixed = inventory.clone();
        mixed.items[1].representations[0].standard = Some(StandardFormat::Text);
        assert_eq!(mixed.bitmap_root(1), None);
        // The anchor's root must be one of the offer's roots.
        inventory.roots.clear();
        assert_eq!(inventory.bitmap_root(1), None);
        assert_eq!(inventory.bitmap_root(0), None);
    }
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
                .and_then(|value| oink_protocol::v2::schema::WireDto::encode(&value, DIRECTORY_PAGE_BYTES))
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
        demand: Demand,
    },
    FileBlock {
        offer: OfferId,
        node: NodeId,
        version: u64,
        offset: u64,

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
    },
    Failed {
        delivery: DeliveryId,
        reason: Unavailable,
    },
    TerminalAck {
        delivery: DeliveryId,
    },
    /// Authorize persistent real-file access while the offered source is live.
    CloudLinkRequest {
        offer: OfferId,
        roots: Vec<NodeId>,
        demand: Demand,
    },
    CloudLinkOffer {
        offer: OfferId,
        token: crate::content::grants::LinkToken,
        roots: Vec<FileNode>,
        fulfilled: Vec<crate::content::grants::FulfilledRoot>,
        native: crate::content::native_metadata::NativeFileMetadata,
    },
    CloudLinkUnavailable {
        offer: OfferId,
        reason: Unavailable,
    },
    /// Each operation creates a temporary namespace bound to the authenticated
    /// receiver. `offer` only correlates replies; no process/offer lookup occurs.
    CloudLinkList {
        offer: OfferId,
        token: crate::content::grants::LinkToken,
        node: NodeId,
        cursor: Option<DirectoryCursor>,
    },
    CloudLinkSelect {
        offer: OfferId,
        token: crate::content::grants::LinkToken,
        node: NodeId,
    },
    CloudLinkRead {
        offer: OfferId,
        token: crate::content::grants::LinkToken,
        node: NodeId,
        version: u64,
        offset: u64,
    },
}

impl ContentMsg {
    pub fn persistent_request(&self) -> bool {
        matches!(
            self,
            Self::CloudLinkList { .. } | Self::CloudLinkSelect { .. } | Self::CloudLinkRead { .. }
        )
    }

    pub fn request(&self) -> Option<(OfferId, Demand)> {
        match self {
            Self::BeginDelivery { delivery, demand } => Some((delivery.offer, *demand)),
            Self::ReadRepresentation { offer, demand, .. }
            | Self::ListDirectory { offer, demand, .. }
            | Self::SelectFile { offer, demand, .. }
            | Self::CloudLinkRequest { offer, demand, .. }
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
            Self::CloudLinkRequest { .. } => "CloudLinkRequest",
            Self::CloudLinkOffer { .. } => "CloudLinkOffer",
            Self::CloudLinkUnavailable { .. } => "CloudLinkUnavailable",
            Self::CloudLinkList { .. } => "CloudLinkList",
            Self::CloudLinkSelect { .. } => "CloudLinkSelect",
            Self::CloudLinkRead { .. } => "CloudLinkRead",
        })
    }
}
