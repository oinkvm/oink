//! Ordered native clipboard/drag representations. Never log names or bytes.
use crate::clipboard::ClipPlatform;
use serde::{Deserialize, Serialize};

#[path = "content/bounded.rs"]
pub(crate) mod bounded;

/// Clipboard and drag identities are independent of transport stream lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClipId {
    pub origin: crate::DeviceId,
    pub seq: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OfferId {
    pub clip: ClipId,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeliveryId {
    pub offer: OfferId,
    pub receiver: crate::DeviceId,
    pub sequence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Holder {
    pub device: crate::DeviceId,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepresentationId(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentSize {
    Unknown,
    Known(u64),
}

/// Numeric registered clipboard IDs are process-local and never sent.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeFormat {
    Mac(#[serde(deserialize_with = "bounded::string::<_, 4096>")] String),
    WindowsRegistered(#[serde(deserialize_with = "bounded::string::<_, 4096>")] String),
    /// Only predefined CF_* values (1..=17); private numeric ranges are unsupported.
    WindowsPredefined(u16),
    /// Retained format identity; only a recognised portable contract can render it.
    Unknown(String),
}
impl std::fmt::Debug for NativeFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Mac(_) => "MacFormat",
            Self::WindowsRegistered(_) => "WindowsRegisteredFormat",
            Self::WindowsPredefined(_) => "WindowsPredefinedFormat",
            Self::Unknown(_) => "UnsupportedFormat",
        })
    }
}
impl NativeFormat {
    pub fn platform(&self) -> ClipPlatform {
        match self {
            Self::Mac(_) => ClipPlatform::Mac,
            Self::Unknown(_) => ClipPlatform::Unknown,
            _ => ClipPlatform::Windows,
        }
    }
    pub fn valid(&self) -> bool {
        match self {
            Self::Mac(name) | Self::WindowsRegistered(name) => {
                !name.is_empty() && name.len() <= 4096 && !name.contains('\0')
            }
            Self::WindowsPredefined(id) => (1..=17).contains(id),
            Self::Unknown(name) => {
                name.len() <= 4096
                    && !name.contains('\0')
                    && name.split_once(':').is_some_and(|(prefix, body)| {
                        !prefix.is_empty() && prefix.len() <= 64 && prefix.is_ascii() && !body.is_empty()
                    })
            }
        }
    }
}

/// A medium is a reconstruction contract, never a source handle or COM pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Medium {
    Bytes,
    HGlobal,
    Stream,
    Storage,
    Bitmap,
    Palette,
    Metafile,
    EnhancedMetafile,
    File,
}

/// Existing standard translations only. None means no cross-OS mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StandardFormat {
    Text,
    Html,
    Rtf,
    Png,
    Privacy,
    /// A single absolute URL and optional human-readable title.
    Url,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unavailable {
    Unsupported,
    LimitExceeded,
    Superseded,
    ReadFailed,
    Cancelled,
    NoSuchItem,
    Permission,
    NoProgress,
    StorageFull,
    InvalidData,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepresentationData {
    /// Snapshot bytes or an active provider; hash is absent until the complete EOF.
    Reader {
        size: ContentSize,
        hash: Option<[u8; 32]>,
    },
    Inline(#[serde(deserialize_with = "bounded::vec::<_, _, 65536>")] Vec<u8>),
    /// Reconstruct a target-local URL/HDROP/descriptor through the file service.
    File(NodeId),
    Unavailable(Unavailable),
    /// Target-local promise control. The node is an INTERNAL container whose
    /// children become known at Drop; neither its name nor its URL is published.
    /// Payload/identity bytes from the source process must never be replayed.
    Promise(
        NodeId,
        #[serde(deserialize_with = "bounded::vec::<_, _, 65536>")] Vec<u8>,
    ),
}
impl std::fmt::Debug for RepresentationData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reader { size, .. } => f.debug_struct("Reader").field("size", size).finish(),
            Self::Inline(bytes) => f.debug_tuple("Inline").field(&bytes.len()).finish(),
            Self::File(_) => f.write_str("File"),
            Self::Promise(..) => f.write_str("Promise"),
            Self::Unavailable(reason) => f.debug_tuple("Unavailable").field(reason).finish(),
        }
    }
}
impl RepresentationData {
    pub fn size(&self) -> Option<ContentSize> {
        match self {
            Self::Reader { size, .. } => Some(*size),
            Self::Inline(bytes) => Some(ContentSize::Known(bytes.len() as u64)),
            Self::File(_) | Self::Promise(..) | Self::Unavailable(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Representation {
    pub id: RepresentationId,
    pub format: NativeFormat,
    pub medium: Medium,
    pub aspect: u32,
    pub index: i32,
    pub standard: Option<StandardFormat>,
    pub data: RepresentationData,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentItem {
    pub id: ItemId,
    /// Files created by a promise/virtual-file source carry content-bearing
    /// companion representations across platforms. Real-file icons/names do not.
    pub virtual_files: bool,
    /// A file-service identity; native source paths never cross machines.
    pub file: Option<NodeId>,
    #[serde(deserialize_with = "bounded::vec::<_, _, 65535>")]
    pub representations: Vec<Representation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Directory,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    #[serde(deserialize_with = "bounded::string::<_, 4096>")]
    pub name: String,
    pub kind: NodeKind,
    pub size: ContentSize,
    pub version: u64,
    pub modified_ms: Option<i64>,
}
impl std::fmt::Debug for FileNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileNode")
            .field("kind", &self.kind)
            .field("size", &self.size)
            .finish()
    }
}
impl FileNode {
    pub fn valid(&self) -> bool {
        !self.name.is_empty()
            && self.name.len() <= 4096
            && self.name != "."
            && self.name != ".."
            && !self.name.contains(['/', '\\', '\0'])
            && self.parent != Some(self.id)
    }
}
