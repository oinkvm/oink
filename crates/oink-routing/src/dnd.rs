//! Native drag lifetime and host handoff domain.
//! Input ordering controls the host; the original content source retains ownership.

use oink_foundation::DeviceId;
use oink_foundation::clipboard::ClipData;
use oink_foundation::content::Holder;
use serde::{Deserialize, Serialize};

use oink_foundation::content::ClipId;
use oink_foundation::content::OfferId;

/// Native identity never changes when the pointer visits another device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DragKey {
    pub offer: OfferId,
    pub native_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PreparationKey {
    pub drag: DragKey,
    pub metadata_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetadataReadiness {
    Pending,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataVersion {
    pub revision: u64,
    pub readiness: MetadataReadiness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceMode {
    Held,
    Exported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreviewAvailability {
    Unknown,
    Absent,
    Available { revision: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevokeReason {
    Cancelled,
    CatcherLost,
    HoldFailed,
    NativeEnded,
    SourceUnavailable,
    Superseded,
    PeerLost,
    PreparationFailed,
    Capacity,
    InvalidTransition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevokeScope {
    Drag,
    Holder(Holder),
    Preparation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DragState {
    /// Can publish a namespace; cannot read source content or inject DOWN.
    Announced,
    Offered {
        holder: Holder,
        mode: SourceMode,
    },
    /// The original source observed the one real physical release.
    Released {
        holder: Holder,
        mode: SourceMode,
    },
    Revoked {
        scope: RevokeScope,
        reason: RevokeReason,
    },
    Ended {
        accepted: bool,
    },
}

impl DragState {
    pub fn holder(self) -> Option<Holder> {
        match self {
            Self::Offered { holder, .. } | Self::Released { holder, .. } => Some(holder),
            Self::Revoked {
                scope: RevokeScope::Holder(holder),
                ..
            } => Some(holder),
            _ => None,
        }
    }
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Revoked {
                scope: RevokeScope::Drag,
                ..
            } | Self::Ended { .. }
        )
    }
}

/// Small state envelope. Large declarations retain the existing attachment
/// transport; the source is authenticated separately from the focus sender.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DragNotice {
    pub key: DragKey,
    pub revision: u64,
    pub input: DeviceId,
    pub state: DragState,
    pub metadata: MetadataVersion,
    pub preview: PreviewAvailability,
    pub items: Vec<ClipData>,
    /// Original typed CBOR for remote metadata, separate from the supported view.
    pub retained_inventory: Option<Vec<u8>>,
}

impl DragNotice {
    pub fn clone_without_items(&self) -> Self {
        Self {
            key: self.key,
            revision: self.revision,
            input: self.input,
            state: self.state,
            metadata: self.metadata,
            preview: self.preview,
            items: Vec::new(),
            retained_inventory: None,
        }
    }

    pub fn preparation(&self) -> PreparationKey {
        PreparationKey {
            drag: self.key,
            metadata_revision: self.metadata.revision,
        }
    }
    /// Early declarations disclose only selected root metadata. In particular,
    /// a promise's private container and inline representations are not roots.
    pub fn valid(&self, source: DeviceId) -> bool {
        self.key.offer.clip.origin == source
            && self.key.native_generation != 0
            && self.revision != 0
            && self.metadata.revision != 0
            && (!matches!(self.state, DragState::Announced)
                || self.items.iter().all(|item| {
                    matches!(item, ClipData::FileRoots(roots)
                    if roots.iter().all(|root| root.parent.is_none() && root.valid()))
                }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceButtonDisposition {
    OriginalSourceOwns,
    ReleaseSubmitted,
    Released,
    ReturnedToOriginalRoute,
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceAction {
    Handoff {
        next: Holder,
    },
    /// Sent for a drop-phase data demand or a real native acceptance. Hover
    /// reads never send this message.
    StartDelivery,
    Cancel,
}

/// 拖放序号的最高位：与剪贴板复制的序号（从 1 递增）不重叠，二者共用文件传输的登记表。
pub const DND_SEQ_BIT: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewBinding {
    pub key: DragKey,
    pub metadata_revision: u64,
    pub preview_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DndMsg {
    /// Only the original content source may send this state or declaration.
    Notice(DragNotice),
    NoticeRequest {
        id: ClipId,
        holder: Holder,
        previous: DeviceId,
    },
    /// A vanished source cannot invent a native generation or input owner.
    Refused {
        id: ClipId,
        holder: Holder,
        reason: RevokeReason,
    },
    SourceButton {
        key: DragKey,
        revision: u64,
        disposition: SourceButtonDisposition,
    },
    Source {
        offer: oink_foundation::content::OfferId,
        holder: Holder,
        action: SourceAction,
        /// Holder pointer in effective reference-plane units, only for cancellation.
        cancel_at: Option<oink_foundation::Point>,
    },
    /// 本机能否接住 / 发起跨设备拖拽（平台支持且设置里没关掉）。连接建立时与能力变化时发送。
    /// 路由只把拖拽交给发过 `enabled: true` 的设备（旧版本不发：对它照旧是墙）。
    Caps { enabled: bool },
    /// 旧宿主 → 新宿主：随后的 `FocusEnter { epoch }` 带着一次拖拽。紧挨在 `FocusEnter` 之前发送。
    Enter {
        /// 内容源 + 拖放序号（带 [`DND_SEQ_BIT`]）。
        id: ClipId,
        /// 随后那条 `FocusEnter` 的 epoch。
        epoch: u64,
    },
    /// Low artwork for one handoff (PNG <= 512 KiB, pixels <= 512x512;
    /// shared `DragImage` validation at source, receiver and native targets).
    /// None explicitly answers a plain drag.
    Preview {
        id: ClipId,
        epoch: u64,
        binding: PreviewBinding,
        image: Option<oink_foundation::dnd::DragImage>,
    },
    /// New host requests optional artwork only after consuming Enter/FocusEnter.
    PreviewRequest {
        id: ClipId,
        epoch: u64,
        binding: PreviewBinding,
    },
}

/// Numeric local observer; no content, routing decision or wire fields are added.
static SOURCE_LEARNED_OBSERVER: std::sync::OnceLock<fn(ClipId, std::time::Instant)> = std::sync::OnceLock::new();
pub fn set_source_learned_observer(observer: fn(ClipId, std::time::Instant)) {
    let _ = SOURCE_LEARNED_OBSERVER.set(observer);
}
pub(crate) fn source_learned_timing(id: ClipId) {
    if let Some(observer) = SOURCE_LEARNED_OBSERVER.get() {
        observer(id, std::time::Instant::now());
    }
}
