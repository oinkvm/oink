//! Local IPC contract mirrored by apps/oink/src/api/layoutProfiles.ts.
use crate::api::{DeviceKey, Placement, ScreenKey};
use oink_display::layout::LayoutSettings;
use oink_foundation::{Point, Size};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileName {
    Default,
    Custom { value: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Disabled {
    pub screen: ScreenKey,
    pub disabled: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceSharing {
    pub device: DeviceKey,
    pub shared: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeScreen {
    pub screen: ScreenKey,
    pub origin: Point,
    pub size: Size,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeState {
    pub device: DeviceKey,
    pub token: String,
    pub primary: ScreenKey,
    pub screens: Vec<NativeScreen>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArrangementSnapshot {
    pub placements: Vec<Placement>,
    pub disabled: Vec<Disabled>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screen_shared: Vec<DeviceSharing>,
    pub native: Vec<NativeState>,
    pub topology: LayoutSettings,
    pub active_profile_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileSummary {
    pub id: String,
    pub name: ProfileName,
    pub content_revision: String,
    pub last_used_ms: String,
    pub exact_match: bool,
    pub active: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Ready,
    Offline,
    Unsupported,
    Unavailable,
}
/// Retained sync preferences never make an offline desktop independently editable.
pub fn effective_system_sync(setting: bool, online: bool) -> bool {
    setting && online
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceArrangementState {
    pub device: DeviceKey,
    pub sync_system_arrangement: bool,
    pub preference_token: String,
    pub native_token: Option<String>,
    pub availability: Availability,
    pub can_edit_screens: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentCode {
    DeviceAdded,
    DeviceRemoved,
    ScreenAdded,
    ScreenRemoved,
    NativeChanged,
    AnchorChanged,
    AnchorDisplaced,
    OverlapRepaired,
    NativeSnapped,
    OwnerOffline,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Adjustment {
    pub code: AdjustmentCode,
    pub device: DeviceKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenKey>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    StaleState,
    ProfileNotFound,
    ProfileLimit,
    InvalidName,
    DuplicateName,
    LastProfile,
    InvalidGeometry,
    Disconnected,
    NoValidPlacement,
    SizeLimit,
    FactsUnavailable,
    OwnerOffline,
    Unsupported,
    SettingDisabled,
    Busy,
    NotMember,
    PersistenceFailed,
    OsApplyFailed,
    NativeChanged,
    OutcomeUnknown,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ProfileError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenKey>,
}
impl ProfileError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            device: None,
            screen: None,
        }
    }
    pub fn device(mut self, device: DeviceKey) -> Self {
        self.device = Some(device);
        self
    }
}
pub type Result<T> = std::result::Result<T, ProfileError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerPhase {
    Applying,
    Applied,
    Failed,
    SkippedOffline,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerProgress {
    pub device: DeviceKey,
    pub phase: OwnerPhase,
    pub error: Option<ProfileError>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Load,
    Edit,
    Reset,
    Replay,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Applied,
    Partial,
    Failed,
    OutcomeUnknown,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutOperation {
    pub id: String,
    pub kind: OperationKind,
    pub status: OperationStatus,
    pub before: ArrangementSnapshot,
    pub after: Option<ArrangementSnapshot>,
    pub native_restore_devices: Vec<DeviceKey>,
    pub adjustments: Vec<Adjustment>,
    pub owners: Vec<OwnerProgress>,
    pub history_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_save: Option<super::autosave::ManualSave>,
    pub error: Option<ProfileError>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    Initializing,
    Ready,
    Syncing,
    Error,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutProfilesState {
    pub epoch: String,
    pub sequence: String,
    pub token: String,
    pub catalog_token: String,
    pub readiness: Readiness,
    pub profiles: Vec<ProfileSummary>,
    pub active_profile_id: Option<String>,
    pub create_enabled: bool,
    pub snapshot: Option<ArrangementSnapshot>,
    pub devices: Vec<DeviceArrangementState>,
    pub pending: Vec<OwnerProgress>,
    pub operation: Option<LayoutOperation>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationReply {
    pub operation_id: String,
    pub state: LayoutProfilesState,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationQuery {
    pub operation: LayoutOperation,
    pub state: LayoutProfilesState,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationEvent {
    pub epoch: String,
    pub sequence: String,
    pub operation: LayoutOperation,
    pub state: LayoutProfilesState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mutation {
    pub request_id: String,
    pub expected_token: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Create {
    pub request_id: String,
    pub expected_token: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rename {
    pub request_id: String,
    pub expected_catalog_token: String,
    pub profile_id: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delete {
    pub request_id: String,
    pub expected_catalog_token: String,
    pub profile_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Load {
    pub request_id: String,
    pub expected_token: String,
    pub profile_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationRequest {
    pub request_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingRequest {
    pub request_id: String,
    pub device: DeviceKey,
    pub expected_preference_token: String,
    pub enabled: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewRequest {
    pub sequence: String,
    pub expected_token: String,
    pub device: DeviceKey,
    pub moved_screen: ScreenKey,
    pub desired_canvas_origin: Point,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewReply {
    pub sequence: String,
    pub native: NativeState,
    pub placements: Vec<Placement>,
    pub plan_token: String,
    pub adjustments: Vec<Adjustment>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitRequest {
    pub request_id: String,
    pub expected_token: String,
    pub placements: Vec<Placement>,
    pub native_plan_tokens: Vec<String>,
    pub disabled: Vec<Disabled>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screen_shared: Vec<DeviceSharing>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Undo,
    Redo,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayRequest {
    pub request_id: String,
    pub expected_token: String,
    pub from: ArrangementSnapshot,
    pub target: ArrangementSnapshot,
    pub native_restore_devices: Vec<DeviceKey>,
    pub direction: Direction,
    pub original_kind: HistoryKind,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // Like api::Request: one IPC value, serialized and dispatched without queueing.
pub enum Command {
    Query,
    Create(Create),
    Rename(Rename),
    Delete(Delete),
    Load(Load),
    Operation(OperationRequest),
    Setting(SettingRequest),
    Preview(PreviewRequest),
    Commit(CommitRequest),
    Reset(Mutation),
    Replay(ReplayRequest),
}
impl Command {
    pub fn request_id(&self) -> Option<&str> {
        Some(match self {
            Self::Query | Self::Preview(_) | Self::Operation(_) => return None,
            Self::Reset(v) => &v.request_id,
            Self::Create(v) => &v.request_id,
            Self::Rename(v) => &v.request_id,
            Self::Delete(v) => &v.request_id,
            Self::Load(v) => &v.request_id,
            Self::Setting(v) => &v.request_id,
            Self::Commit(v) => &v.request_id,
            Self::Replay(v) => &v.request_id,
        })
    }
}

pub fn json<T: Serialize>(value: T) -> Result<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|_| ProfileError::new(ErrorCode::InvalidGeometry, "Cannot encode profile result"))
}
pub fn error_reply(error: ProfileError) -> serde_json::Value {
    serde_json::json!({"type":"profile_error","error":error})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    Load,
    Edit,
    Reset,
}
