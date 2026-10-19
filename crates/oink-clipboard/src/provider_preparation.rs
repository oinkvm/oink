//! Target-local timing boundaries. Offsets are microseconds from registration start;
//! an already prepared parent has offset zero. None means the boundary was not observed.
use crate::FileProviderReason;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreparationPath {
    Spare,
    NewParent,
    DegradedPromise,
    Failed,
    Placeholder,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PreparationTiming {
    pub spare_ready: bool,
    pub spare_preparation_us: Option<u64>,
    pub spare_failure: Option<FileProviderReason>,
    /// Offset from target begin request to the registration boundary.
    pub registration_start_us: u64,
    pub registration_us: u64,
    pub parent_visible_us: Option<u64>,
    /// Extension reported all top-level children; not a daemon application ACK.
    pub children_enumerated_us: Option<u64>,
    pub first_url_us: Option<u64>,
    /// Existing AppKit willBegin / phase 28, not the later begin-worker return.
    pub native_begin_us: Option<u64>,
    pub placeholder: PlaceholderTiming,
}

/// Why an enabled experiment did not supply a final namespace. No item strings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaceholderFallback {
    #[default]
    Off,
    None,
    NotReady,
    Capacity,
    UnknownSize,
    PreparationFailed,
    PublicationFailed,
    DisabledDuringPublication,
    DeferredToEnter,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceholderTiming {
    pub enabled: bool,
    pub used: bool,
    pub ready_parents: u32,
    pub preparing: bool,
    pub prepared_files: u32,
    pub prepared_folders: u32,
    pub used_files: u32,
    pub used_folders: u32,
    pub preparation_us: Option<u64>,
    pub final_urls_us: Option<u64>,
    pub fallback: PlaceholderFallback,
    pub failure: Option<FileProviderReason>,
    pub cleanup_pending: u32,
}

/// Live pool inventory, independent of any drag's preparation timing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceholderPoolStatus {
    pub enabled: bool,
    pub ready_parents: u32,
    /// Capacity of each prepared parent, including when the pool is empty/off.
    pub files_per_parent: u32,
    pub folders_per_parent: u32,
    pub preparing: bool,
    pub last_background_failure: Option<FileProviderReason>,
    pub cleanup_pending: u32,
}
