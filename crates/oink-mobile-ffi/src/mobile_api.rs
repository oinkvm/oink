//! Typed mobile owner facade.
//!
//! These records intentionally contain only local state and v2 projection
//! data. They do not mirror a legacy envelope and they never carry arbitrary
//! wire payloads.

use crate::transfer::{MobileShareItem, MobileShareSensitivity};
use crate::{
    AndroidControlListener, AndroidControls, AndroidInputListener, MobileAndroidInput, MobileDeliveryFailure,
    MobileDeliveryState, MobileDiagnosticCollector, MobileMediaSession,
};
use oink_foundation::{DeviceId, session::Boot};
use oink_mobile_core::{
    DirectoryEntry, MobileNetwork, MobileNetworkConfig, MobileProcess, MobileV2Identity, MobileV2Session,
    MobileV2TransportError, TransferEvent, TransferStore,
};
use oink_network::discovery::{self, native::NativeDiscovery};
use std::sync::{Arc, Mutex};

mod addresses;
mod blocks;
mod clipboard;
pub use clipboard::MobileClipboard;
mod device_control;
mod history;
mod last_seen;
mod names;
pub use names::LocalDeviceName;
mod pairing;
mod presence;
mod targets;
pub use targets::*;
mod control;
pub use control::*;

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum MobilePlatform {
    Android,
    Ios,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum SessionProfile {
    Mobile,
    ShareSession,
    Broadcast,
}
/// Host-reported device shape; PCs draw the iPhone or iPad badge from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MobileFormFactor {
    Phone,
    Tablet,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileConfig {
    pub data_dir: String,
    pub cache_dir: Option<String>,
    pub log_dir: Option<String>,
    pub device_name: String,
    pub platform: MobilePlatform,
    pub profile: SessionProfile,
    #[uniffi(default = None)]
    pub form_factor: Option<MobileFormFactor>,
    /// The installed App's version as its About page shows it (Android
    /// versionName, iOS CFBundleShortVersionString). Published as this
    /// phone's DeviceFacts build / app_version so PCs show the real version;
    /// without it the core's crate version is used.
    #[uniffi(default = None)]
    pub app_version: Option<String>,
}

/// What the native owners can do right now. These are facts, not user intent,
/// and never capabilities: the negotiated capability set follows from
/// `MobileConfig.profile` and never changes while the session runs (protocol-v2
/// 1.1). Audio facts become the endpoint directions' `supported`;
/// `privileged_input_ready` (the Android helper accepts input, including asleep
/// or at keyguard) becomes the I0 `InputCapability.controllable`. No link drops.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct MobilePlatformFacts {
    pub microphone_send: bool,
    pub speaker_receive: bool,
    /// Native output capture currently available (helper / broadcast extension).
    pub speaker_send: bool,
    pub privileged_input_ready: bool,
}

impl MobilePlatformFacts {
    fn core(self) -> oink_mobile_core::MobilePlatformFacts {
        oink_mobile_core::MobilePlatformFacts {
            microphone_send: self.microphone_send,
            speaker_receive: self.speaker_receive,
            speaker_send: self.speaker_send,
            privileged_input_ready: self.privileged_input_ready,
        }
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum UserFacingCode {
    Unspecified,
    PermissionRequired,
    PermissionDenied,
    CaptureUnavailable,
    InjectionUnavailable,
    SecureInput,
    DeviceUnavailable,
    CleanupFailed,
    Unsupported,
    BackendFailed,
    LinkLost,
    Timeout,
    StorageFull,
    InvalidValue,
    StateConflict,
    LocalServiceUnavailable,
    InvalidAddress,
    NotMember,
    PersistenceFailed,
    NetworkStopped,
    Unrecognized,
}
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileError {
    #[error("{detail}")]
    InvalidArgument { detail: String },
    #[error("mobile owner is busy")]
    OwnerBusy,
    #[error("mobile session is stopped")]
    Stopped,
    #[error("startup failed: {detail}")]
    Startup { detail: String },
    #[error("shutdown failed: {detail}")]
    Shutdown { detail: String },
    #[error("command failed: {detail}")]
    Command { code: UserFacingCode, detail: String },
    #[error("clipboard {reason:?}: {detail}")]
    Clipboard { reason: ClipboardFailure, detail: String },
    #[error("worker failed: {detail}")]
    Worker { detail: String },
    /// A transfer request failed for a typed reason; hosts show the reason,
    /// never a generic "connection lost".
    #[error("transfer {reason:?}: {detail}")]
    Transfer { reason: TransferFailure, detail: String },
}

/// Why a transfer request failed (contract section 9 keeps these apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TransferFailure {
    /// The link to the source or target ended during the request.
    LinkLost,
    /// No online device can take the request (no PC for the current text,
    /// no share target after the first connect round).
    NoTarget,
    /// The content's source device is offline.
    SourceOffline,
    /// The source no longer keeps this content.
    Evicted,
    /// The source does not have this item.
    NoSuchItem,
    /// The source replaced this content with a newer one.
    Superseded,
    /// The source cannot provide this content.
    Unsupported,
    /// The source's current clipboard expired before it could be read.
    Expired,
    /// The history record does not exist (deleted meanwhile).
    NotFound,
    /// An own message has no remote source to download from.
    OwnMessage,
    /// A download of this record is already running.
    InProgress,
    /// This phone has no room for the content (copying, packaging).
    StorageFull,
    /// The share was cancelled (`transfer_cancel_share`) before it was recorded.
    Cancelled,
    /// The content to share could not be read on this phone.
    LocalRead,
    /// The transfer history could not be written (storage full, ...).
    Storage,
    /// A peer sent something this version cannot use.
    Incompatible,
    /// The source or target paused clipboard sharing.
    Paused,
    /// The source turned off sending its clipboard.
    SendDisabled,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileAdvertisement {
    pub instance: String,
    pub service_type: String,
    pub port: u16,
    pub txt: std::collections::HashMap<String, String>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ResolvedEndpoint {
    pub address: String,
    pub port: u16,
    pub scope_id: u32,
}

#[derive(Debug, uniffi::Object)]
pub struct MobileDiscovery {
    inner: NativeDiscovery,
}
#[uniffi::export]
impl MobileDiscovery {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: NativeDiscovery::new(),
        })
    }
    pub fn network_changed(&self, generation: u64) {
        self.inner.network_changed(generation);
    }
    pub fn upsert(
        &self,
        generation: u64,
        key: String,
        txt: std::collections::HashMap<String, String>,
        endpoints: Vec<ResolvedEndpoint>,
    ) {
        use std::net::{IpAddr, SocketAddr, SocketAddrV6};
        let endpoints = endpoints
            .into_iter()
            .filter_map(|value| match value.address.parse::<IpAddr>().ok()? {
                IpAddr::V4(ip) => Some(SocketAddr::new(ip.into(), value.port)),
                IpAddr::V6(ip) => Some(SocketAddr::V6(SocketAddrV6::new(ip, value.port, 0, value.scope_id))),
            })
            .collect();
        self.inner.upsert(generation, key, txt.into_iter().collect(), endpoints);
    }
    pub fn withdraw(&self, generation: u64, key: String) {
        self.inner.withdraw(generation, &key);
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ScreenKey {
    pub device: String,
    pub display: String,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum KeyPreset {
    WinFeel,
    MacFeel,
    Custom,
    None,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct KeyMapping {
    pub from: u32,
    pub to: u32,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct CapsTapHold {
    pub tap: u32,
    pub hold: u32,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct SavedCustomKeymap {
    pub table: Vec<KeyMapping>,
    pub caps_hold: Option<u32>,
    pub caps_split: bool,
    pub allow_fn: bool,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct KeyboardMapping {
    pub preset: KeyPreset,
    pub table: Vec<KeyMapping>,
    pub caps_tap_hold: Option<CapsTapHold>,
    pub saved_custom: Option<SavedCustomKeymap>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct TargetTransform {
    pub reverse_wheel: bool,
    pub keyboard: KeyboardMapping,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct InputTargetOverride {
    pub target: String,
    pub reverse_wheel: Option<bool>,
    pub keyboard: Option<KeyboardMapping>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct InputLocalExclusion {
    pub scope: String,
    pub target: Option<String>,
    pub mapping: KeyMapping,
    pub reason: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct InputTransform {
    pub macos: TargetTransform,
    pub windows: TargetTransform,
    pub overrides: Vec<InputTargetOverride>,
    pub local_exclusions: Vec<InputLocalExclusion>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ManagedScreen {
    pub screen: ScreenKey,
    pub name: String,
    pub gain: f64,
    pub remote_gain: f64,
    pub disabled: bool,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceSettingsView {
    pub mac_scroll_feel: Option<bool>,
    pub native_multifinger: Option<bool>,
    pub touchpad_injection: Option<bool>,
    pub device: String,
    pub input_transform: Option<InputTransform>,
    pub screens: Vec<ManagedScreen>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct FocusSummary {
    pub owner: String,
    pub revision: u64,
    pub epoch: u64,
    pub holder: Option<String>,
    pub locked_device: Option<String>,
    pub valid: bool,
    pub version_token: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct GroupRef {
    pub id: String,
    pub name: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct PeerView {
    pub device: String,
    pub name: String,
    pub os: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairEndpoint {
    pub device: String,
    pub name: String,
    pub os: String,
}
/// Typed manual-add / address-query failure, so the UI can tell
/// "no Oink device here" from timeouts, unreachable networks and old versions.
#[derive(Debug, Clone, uniffi::Enum, PartialEq)]
pub enum AddressNetworkProblem {
    InvalidAddress,
    /// No route / host unreachable.
    Unreachable,
    /// Nothing answered within the query budget.
    Timeout,
    /// The port is closed (nothing listening).
    Refused,
    /// An Oink device that needs a compatible version ("版本过低，请升级").
    Incompatible,
    /// Something answered but it is not an Oink device.
    NotOink,
    /// The address is this phone.
    SelfDevice,
    /// This phone has no usable network interface / route.
    NoNetwork,
    /// Too many simultaneous queries; retry.
    Busy,
}
#[derive(Debug, Clone, uniffi::Record, PartialEq)]
pub struct PairProblem {
    pub code: u32,
    pub stage: u32,
    pub detail: String,
    pub network: Option<AddressNetworkProblem>,
}
#[derive(Debug, Clone, uniffi::Enum, PartialEq)]
pub enum IdentityEvidence {
    Unverified,
    GroupMember,
    Authenticated,
}
#[derive(Debug, Clone, uniffi::Record, PartialEq)]
pub struct DiscoveredGroupRef {
    pub group: String,
    pub root: Option<String>,
}
#[derive(Debug, Clone, uniffi::Record, PartialEq)]
pub struct SocketAddress {
    pub address: String,
    pub port: u16,
    pub scope_id: u32,
}
#[derive(Debug, Clone, uniffi::Record, PartialEq)]
pub struct NearbyRow {
    pub device: String,
    pub name: String,
    pub os: String,
    pub group: DiscoveredGroupRef,
    pub count: Option<u8>,
    pub addresses: Vec<SocketAddress>,
    pub selected: SocketAddress,
    pub source: u32,
    pub identity: IdentityEvidence,
    pub reachable: bool,
    pub compatible: bool,
    pub problem: Option<PairProblem>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct NearbySnapshot {
    pub revision: u64,
    pub rows: Vec<NearbyRow>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Direction {
    Incoming,
    Outgoing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Feedback {
    Waiting,
    Correct,
    Wrong,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairView {
    pub id: String,
    pub revision: u64,
    pub direction: Direction,
    pub peer: PairEndpoint,
    pub phase: u32,
    pub attempts_left: u8,
    pub feedback: Feedback,
    pub terminal: Option<PairProblem>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct PendingRequest {
    pub id: String,
    pub revision: u64,
    pub peer: PeerView,
    pub source: GroupRef,
    pub target: GroupRef,
    pub source_count: u8,
    pub target_count: u8,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairingSnapshot {
    pub revision: u64,
    pub requests: Vec<PendingRequest>,
    pub sessions: Vec<PairView>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct FoundPeer {
    pub query_id: String,
    pub generation: u64,
    pub peer: PeerView,
    pub prepared: bool,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PairTarget {
    NearbyDevice { device: String },
    PreparedQuery { query_id: String, generation: u64 },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CancelResult {
    Cancelled,
    /// The merge already committed; pairing succeeded and cannot be undone
    /// (decision 38: no undo after a successful pairing).
    AlreadyCommitted,
    CommitUnconfirmed,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum TypedResult {
    Ok,
    Problem { problem: PairProblem },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AddressQueryResult {
    Found { peer: FoundPeer },
    Problem { problem: PairProblem },
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct CodeView {
    pub six_digits: String,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PairRejection {
    Problem { problem: PairProblem },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PairAcceptedReply {
    Value { v1: String },
    Problem { v1: PairRejection },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PairStartedReply {
    Value { v1: String },
    Problem { v1: PairRejection },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PairCancelReply {
    Value { v1: CancelResult },
    Problem { v1: PairRejection },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum NegotiationFailureCode {
    Protocol,
    Incompatible,
    Authentication,
    Timeout,
    Unknown,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum NegotiationStage {
    Discovery,
    Admission,
    Handshake,
    Session,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct NegotiationProblem {
    pub code: NegotiationFailureCode,
    pub stage: NegotiationStage,
    pub missing_capability: Option<String>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct PeerCompatibility {
    pub device: String,
    pub problem: Option<NegotiationProblem>,
}

#[derive(Debug, Clone, Copy, uniffi::Enum, PartialEq)]
pub enum CardOs {
    Android,
    Ios,
    Macos,
    Windows,
    Linux,
    Other,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CardSection {
    ThisDevice,
    Online,
    Offline,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum OfflineKind {
    Disconnected,
    Connecting,
    AddressStale,
    ProtocolIncompatible,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CardCondition {
    Ready,
    LocalPaused,
    Sleeping,
    Starting,
    WaitingForTarget,
    ScreenDisabledFallback,
    Faulted,
    Unsupported,
    StateUnknown,
    StateStale,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CardActionUnavailable {
    Offline,
    Unsupported,
    StateUnknown,
    StateStale,
    RemoteFault,
    NotMember,
    Busy,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CardCommand {
    SetScreenMode,
    SetLocalOption,
    ClearAudioExit,
    MakeAudioExit,
    SetGameMode,
    SetDevicePaused,
    SetFocusLock,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CardAction {
    Invoke { command: CardCommand, token: String },
    OpenPermissions,
    Unavailable { reason: CardActionUnavailable },
}
/// Current owner mode of a shareable resource. `Disabled` exists only for the
/// mouse and keyboard; screen and clipboard are Shared or Local.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ResourceMode {
    Shared,
    Local,
    Disabled,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ResourceCard {
    /// The single-tap action: Shared <-> Local, Disabled -> Shared (enabled).
    pub action: CardAction,
    pub condition: CardCondition,
    /// None when the owner did not publish the value.
    pub mode: Option<ResourceMode>,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum SpeakerActivity {
    Idle,
    Playing,
    Forwarding,
    SoundSleep,
    Preparing,
    WaitingForExit,
    Paused,
    Sleeping,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum MicrophoneMode {
    Unavailable,
    Idle,
    Source,
    Receiver,
    Muted,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceCardActions {
    pub screen_shared: CardAction,
    pub screen_local: CardAction,
    pub mouse_shared: CardAction,
    pub mouse_local: CardAction,
    pub mouse_disabled: CardAction,
    pub keyboard_shared: CardAction,
    pub keyboard_local: CardAction,
    pub keyboard_disabled: CardAction,
    pub keyboard_enabled: CardAction,
    pub clipboard_shared: CardAction,
    pub clipboard_local: CardAction,
    pub forwarding_enabled: CardAction,
    pub forwarding_disabled: CardAction,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct KeyboardCard {
    pub resource: ResourceCard,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ForwardingCard {
    pub volume_percent: u16,
    pub muted: bool,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct SpeakerCard {
    /// `Invoke{ClearAudioExit}` when this device is the exit, else MakeAudioExit.
    pub make_exit_action: CardAction,
    /// This device is the group audio exit (ready or playing).
    pub is_exit: bool,
    pub forwarding_controls_token: Option<String>,
    pub activity: SpeakerActivity,
    pub forwarding: Option<ForwardingCard>,
    /// The PC's "转发" switch in the global audio configuration this phone
    /// holds (protocol-v2 6.1); None while unknown here.
    /// `activity` only reports what the forwarding pipeline is doing now
    /// (it is Idle/Unknown while nothing plays or no exit is resolved).
    #[uniffi(default = None)]
    pub forwarding_enabled: Option<bool>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct MicrophoneCard {
    pub mode: MicrophoneMode,
    pub share_action: CardAction,
    pub local_action: CardAction,
    pub receive_action: CardAction,
    pub controls_token: Option<String>,
    pub forwarding: Option<ForwardingCard>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct GameActions {
    pub auto: CardAction,
    pub on: CardAction,
    pub off: CardAction,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum GameModePreference {
    Auto,
    On,
    Off,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct GameCard {
    pub actions: GameActions,
    /// The owner's saved preference (Auto/On/Off).
    pub preference: Option<GameModePreference>,
    /// Owner observation: game mode currently in effect / a game was detected.
    pub active: Option<bool>,
    pub detected: Option<bool>,
}
/// Group pointer-focus lock as observed by this desktop owner.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FocusCard {
    /// The group pointer is locked to this device.
    pub locked: bool,
    /// The group pointer is locked to another device.
    pub locked_elsewhere: bool,
    /// `Invoke{SetFocusLock}`: unlock when `locked`, otherwise lock to this device.
    pub action: CardAction,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceCardStatus {
    pub actions: DeviceCardActions,
    pub screen: ResourceCard,
    pub mouse: ResourceCard,
    pub keyboard: KeyboardCard,
    pub speaker: SpeakerCard,
    pub microphone: MicrophoneCard,
    pub clipboard: ResourceCard,
    pub game: GameCard,
    pub focus: FocusCard,
}
/// Audio endpoint state of a phone (this phone or another group phone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MobileEndpointState {
    /// Not forwarded (local).
    Local,
    /// Enabled, waiting for the route or the native start.
    Starting,
    /// Speaker: playing as the group exit. Microphone: sending.
    Active,
    Failed,
    /// The phone does not advertise this endpoint.
    Unavailable,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileAudioCard {
    pub speaker: MobileEndpointState,
    pub microphone: MobileEndpointState,
    pub forward: MobileEndpointState,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct LocalPauseCard {
    pub action: CardAction,
    pub paused: bool,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceCard {
    pub is_content_target_pc: bool,
    pub device: String,
    pub name: String,
    pub os: CardOs,
    pub is_self: bool,
    pub section: CardSection,
    pub connection: CardConnection,
    pub last_seen_ms: Option<u64>,
    pub status: Option<DeviceCardStatus>,
    pub pause: Option<LocalPauseCard>,
    /// Phones only (this phone included): speaker/microphone endpoint state.
    pub mobile_audio: Option<MobileAudioCard>,
    /// The Crockford base32 4-4-4 fingerprint the PC shows for this device.
    pub fingerprint: String,
    /// An online Android device (this phone included) whose privileged mode is
    /// on and running (DeviceFacts key 32): draw the crowned Android badge.
    /// False for every other state, including offline.
    pub android_privileged: bool,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Signal {
    Measured { rtt_ms: f64 },
    Unknown { rtt_ms: Option<f64> },
    Offline { rtt_ms: Option<f64> },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CardConnection {
    ThisDevice,
    Online { signal: Signal },
    Offline { reason: OfflineKind, signal: Signal },
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ExitVolumeCard {
    pub receiver_device: String,
    pub receiver_name: String,
    pub volume_percent: Option<u8>,
    pub muted: Option<bool>,
    pub version_token: Option<String>,
    pub can_set: bool,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceCardsView {
    pub stream: String,
    pub revision: u64,
    pub cards: Vec<DeviceCard>,
    pub exit_volume: Option<ExitVolumeCard>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceCardSignal {
    pub device: String,
    pub rtt_ms: Option<f64>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceCardSignals {
    pub stream: String,
    pub revision: u64,
    pub sampled_at_ms: u64,
    pub signals: Vec<DeviceCardSignal>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockedDevice {
    pub device: String,
    pub name: Option<String>,
    pub os: Option<String>,
    pub blocked_at_ms: u64,
    pub fingerprint: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockedDevicesSnapshot {
    pub stream: String,
    pub revision: u64,
    pub entries: Vec<BlockedDevice>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceControlCompleted {
    pub request_id: String,
    pub result: DeviceControlResult,
}
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum DeviceControlFailure {
    Offline,
    Unsupported,
    StaleBoot,
    StaleRevision,
    NotMember,
    ResourceFault,
    InvalidValue,
    Busy,
    PersistenceFailed,
    ApplyFailed,
    LinkLost,
    OutcomeUnknown,
    NotExit,
    RouteChanged,
    OutputChanged,
    OutputUnavailable,
    VolumeUnsupported,
    PermissionDenied,
    Superseded,
    UnsupportedOption,
    StateConflict,
    Unrecognized,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum DeviceControlResult {
    Applied,
    NoChange,
    Refused { reason: DeviceControlFailure },
    Failed { reason: DeviceControlFailure },
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceControlAccepted {
    pub request_id: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct PauseAllAccepted {
    pub request_id: String,
    pub event_token: String,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum DeviceControlReply {
    Accepted(DeviceControlAccepted),
    Completed(DeviceControlCompleted),
}
/// `code`: 0 = this device cannot be blocked, 1 = persistence failed,
/// 2 = block list limit (`limit` entries) reached.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockListError {
    pub code: u32,
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockEdited {
    pub operation_id: String,
    pub changed: bool,
    pub snapshot: BlockedDevicesSnapshot,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum BlockReply {
    Error(BlockListError),
    Edited(BlockEdited),
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ClipboardEvent {
    Metadata {
        source: String,
        clock: u64,
        fresh_copy: bool,
        automatic_eligible: bool,
    },
    Progress {
        operation_id: u64,
        target: String,
        bytes: u64,
        total: Option<u64>,
    },
    Received(ReceiveResult),
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ReceiveKind {
    Text,
    Image,
    Files,
    Mixed,
    Empty,
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum EmptyReason {
    NoContent,
    SourceOffline,
    SourceVersionUnsupported,
    Superseded,
    Consumed,
    Paused,
    Disabled,
    Unavailable,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct ReceivedFile {
    pub path: String,
    pub name: String,
    pub mime: String,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileCurrentClipboardText {
    pub text: String,
    pub sensitive: bool,
    /// Oink clip marker (`oink_foundation::clipboard_marker`): the host writes
    /// these bytes next to the text, unchanged.
    pub marker: Option<Vec<u8>>,
    /// The text's rich text, written with it (see `MobileRichText.html` /
    /// `rtf`; 2026-11-12).
    #[uniffi(default = None)]
    pub html: Option<String>,
    #[uniffi(default = None)]
    pub rtf: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MobileContentKind {
    Text,
    Image,
    File,
    Folder,
    RichText,
}

/// The group's current clipboard for a one-tap copy (`receive_current_clipboard`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum MobileCurrentClipboard {
    /// `html` / `rtf`: the text's rich text, written with it (see
    /// `MobileRichText`; 2026-11-12).
    Text {
        text: String,
        sensitive: bool,
        marker: Option<Vec<u8>>,
        #[uniffi(default = None)]
        html: Option<String>,
        #[uniffi(default = None)]
        rtf: Option<String>,
    },
    /// A downloaded image in the transfer store. The host reads `path` and
    /// must not move or delete it (the conversation shows the same file).
    Image {
        path: String,
        mime: Option<String>,
        size: u64,
        sensitive: bool,
        marker: Option<Vec<u8>>,
    },
    /// The image is bigger than `limit` bytes and is not handed over.
    ImageTooLarge { size: u64, limit: u64 },
    /// A file, folder or rich-text-only content: not copied by the shortcut.
    Unsupported { kind: MobileContentKind },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ReceiveResult {
    pub operation_id: u64,
    pub native_revision: u64,
    pub kind: ReceiveKind,
    pub source: String,
    pub text: Option<String>,
    pub files: Vec<ReceivedFile>,
    pub sensitive: bool,
    pub empty_reason: Option<EmptyReason>,
    /// Oink clip marker (`oink_foundation::clipboard_marker`): the host writes
    /// these bytes next to the content, unchanged. None for a direct share.
    pub marker: Option<Vec<u8>>,
    /// The text's rich text (2026-11-12): the host writes it with the text,
    /// `html` as Android `ClipData.newHtmlText` / iOS `public.html`, `rtf`
    /// (when the source was RTF) also as iOS `public.rtf`. See
    /// `MobileRichText`.
    #[uniffi(default = None)]
    pub html: Option<String>,
    #[uniffi(default = None)]
    pub rtf: Option<String>,
}
impl MobileSession {
    /// The first directly connected PC that shares its clipboard (waiting up
    /// to 4 s for one to come online) and the running transfer session.
    /// The first directly connected PC that takes clipboard requests. Right
    /// after the session started it waits for one to be ready (driven by
    /// link events); once the first connect round ended without one, it fails
    /// with `TransferFailure::NoTarget`.
    async fn current_clipboard_source(
        &self,
    ) -> Result<(String, Arc<MobileV2Session<oink_mobile_core::MobileNetworkLink>>), MobileError> {
        let mut changes = self.network.subscribe_changes();
        let peer = loop {
            {
                let state = self.presence.lock().unwrap();
                if state.stopped || state.suspended {
                    return Err(MobileError::Stopped);
                }
            }
            if let Some(peer) = self
                .network_peers(true)
                .into_iter()
                .filter(|peer| peer.kind == oink_mobile_core::PeerKind::Pc && peer.accepts_clipboard)
                .min_by_key(|peer| peer.connection_order)
            {
                break peer;
            }
            if self.network.connect_round_settled() {
                return Err(transfer_error(MobileV2TransportError::NoTarget));
            }
            changes.changed().await.map_err(|_| MobileError::Stopped)?;
        };
        let transfer = self.transfer_owner()?;
        transfer.set_peers(self.network_peers(false));
        Ok((peer.id, transfer))
    }

    fn new(
        discovery: Arc<MobileDiscovery>,
        config: &MobileConfig,
        lease: std::fs::File,
        network: MobileNetwork,
        transfer: MobileV2Session<oink_mobile_core::MobileNetworkLink>,
        addresses: addresses::AddressBook,
        names: names::DeviceNames,
    ) -> Arc<Self> {
        let transfer = Arc::new(transfer);
        network.link.attach_clipboard(&transfer);
        let clipboard = Arc::new(MobileClipboard::new(&transfer));
        if matches!(config.profile, SessionProfile::Mobile) {
            transfer.attach_fresh_clipboard_observer(clipboard.clone());
        }
        // The embedded router owns this phone's I0 (protocol §14): every I0
        // message reaches it from the network's receive tasks, and the
        // openings the core posts are its records.
        let input = oink_mobile_core::InputRouter::start(network.boot(), Arc::new(network.clone()));
        network.set_input_sink(Some(input.clone()));
        if let Err(error) = network.set_input_opening(input.clone()) {
            tracing::warn!(%error, "input router: opening provider not installed");
        }
        // Transfer and clipboard events are pushed as they happen; one task
        // delivers them to the listener in order, outside every Rust lock.
        let (events, queued) = tokio::sync::mpsc::unbounded_channel();
        let sender = events.clone();
        transfer.set_event_sink(Arc::new(move |event| {
            let _ = sender.send(transfer_event(event));
        }));
        clipboard.set_event_sink(Arc::new(move |event| {
            let _ = events.send(MobileEvent::Clipboard { event });
        }));
        let (subscribed, listening) = tokio::sync::watch::channel(false);
        let session = Arc::new(Self {
            id: network.device_id(),
            addresses: Mutex::new(addresses),
            names,
            stream: format!("{:032x}", rand::random::<u128>()),
            discovery,
            lease: Mutex::new(Some(lease)),
            runtime: tokio::runtime::Handle::current(),
            listener: Mutex::new(None),
            presence: Mutex::new(presence::Presence::new(last_seen::LastSeen::load(
                std::path::Path::new(&config.data_dir),
            ))),
            targets_cache: crate::device_only::transfer_targets(std::path::Path::new(&config.data_dir)),
            prepared_pairing: Mutex::new(None),
            acknowledged_pairing_event: std::sync::atomic::AtomicU64::new(0),
            media: MobileMediaSession::new(
                network.clone(),
                matches!(config.profile, SessionProfile::Mobile | SessionProfile::Broadcast),
                std::path::PathBuf::from(&config.data_dir),
            ),
            clipboard,
            network,
            transfer: Mutex::new(Some(transfer)),
            subscribed,
            android_input: Mutex::new(None),
            input,
            control_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            started: std::time::Instant::now(),
            diagnostics: Mutex::new(std::sync::Weak::new()),
        });
        tokio::spawn(deliver_events(Arc::downgrade(&session), queued, listening));
        tokio::spawn(watch_network(Arc::downgrade(&session), session.network.clone()));
        session
    }
}

/// Restart pacing across this process's sessions (the host starts a new
/// session after each failure; the process stays).
static NETWORK_RESTARTS: Mutex<oink_network::link::restart::Backoff> =
    Mutex::new(oink_network::link::restart::Backoff::new());

/// Fail fast (user 2026-11-12): when the network layer ends on its own the
/// session tells its host, which restarts it. A requested stop ends the
/// layer with `Ok` and this task with it.
async fn watch_network(session: std::sync::Weak<MobileSession>, network: MobileNetwork) {
    let Err(fatal) = network.exited().await else {
        return;
    };
    let Some(session) = session.upgrade() else {
        return;
    };
    session.network_failed(fatal);
}

impl MobileSession {
    /// Records why (log, diagnostics) and asks the host to restart the
    /// session after the back-off. Nothing is rebuilt in place.
    fn network_failed(&self, fatal: oink_network::link::NetworkFatal) {
        if self.presence.lock().unwrap_or_else(|error| error.into_inner()).stopped {
            return;
        }
        let (delay, attempt) = NETWORK_RESTARTS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .next(self.started.elapsed());
        let retry_after_ms = delay.as_millis() as u64;
        tracing::error!(reason = fatal.code(), error = %fatal, attempt, retry_after_ms,
            "network layer ended with a fatal error; the host restarts the session");
        let collector = self
            .diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .upgrade();
        if let Some(collector) = collector {
            let event = oink_mobile_core::DiagnosticEvent {
                timestamp_ms: presence::now_ms(),
                kind: oink_mobile_core::DiagnosticKind::Error,
                code: oink_mobile_core::DiagnosticCode::Failed,
                metric: Some(u64::from(attempt)),
                count: 1,
            };
            if let Err(error) = collector.record(event) {
                tracing::warn!(%error, "network failure not recorded in diagnostics");
            }
        }
        self.emit_event(MobileEvent::NetworkFailed {
            detail: fatal.to_string(),
            retry_after_ms,
        });
    }
}

/// Hands queued transfer and clipboard events to the listener. Events wait
/// while no listener is subscribed (between start and `subscribe`).
async fn deliver_events(
    session: std::sync::Weak<MobileSession>,
    mut queued: tokio::sync::mpsc::UnboundedReceiver<MobileEvent>,
    mut listening: tokio::sync::watch::Receiver<bool>,
) {
    while let Some(event) = queued.recv().await {
        if listening.wait_for(|subscribed| *subscribed).await.is_err() {
            return;
        }
        let Some(session) = session.upgrade() else { return };
        let mut event = event;
        // Named here, outside every core lock (the sink only queues).
        if let MobileEvent::TransferDirectIncoming {
            sender_id, sender_name, ..
        } = &mut event
        {
            *sender_name = session.member_name(sender_id);
        }
        session.emit_event(event);
    }
}

async fn start_session(
    config: MobileConfig,
    discovery: Arc<MobileDiscovery>,
    identity_seed: Option<Vec<u8>>,
) -> Result<Arc<MobileSession>, MobileError> {
    crate::logging::install();
    let path = std::path::Path::new(&config.data_dir);
    let lease = acquire_mobile_lease(path)?;
    let (database, attachments) = crate::transfer_store_paths(path);
    let store = TransferStore::open(database, attachments).map_err(|error| MobileError::Startup {
        detail: error.to_string(),
    })?;
    let identity_seed = load_seed(&config, identity_seed)?;
    let names = names::DeviceNames::load(path, &config.device_name)?;
    let addresses = addresses::AddressBook::load(path)?;
    let network_identity = oink_network::identity::Identity::from_seed(identity_seed);
    let boot = Boot {
        device_id: network_identity.id(),
        nonce: rand::random(),
    };
    let network = MobileNetwork::start(MobileNetworkConfig {
        identity_seed,
        boot,
        bind: "0.0.0.0:0".parse().expect("fixed mobile bind address"),
        group_state: None,
        group_state_path: crate::device_only::group_state(path),
        pairing_journal_path: crate::device_only::pairing_journal(path),
        device_name: names.view().name,
        os: match config.platform {
            MobilePlatform::Android => "android".into(),
            MobilePlatform::Ios => "ios".into(),
        },
        form_factor: config.form_factor.map(|form| match form {
            MobileFormFactor::Phone => oink_protocol::v2::dto::FormFactor::Phone,
            MobileFormFactor::Tablet => oink_protocol::v2::dto::FormFactor::Tablet,
        }),
        trusted_peers: Vec::new(),
        trust_path: Some(crate::device_only::trust(path)),
        process: match config.profile {
            SessionProfile::Mobile => MobileProcess::App,
            SessionProfile::ShareSession => MobileProcess::Share,
            SessionProfile::Broadcast => MobileProcess::Broadcast,
        },
    })
    .await
    .map_err(|error| MobileError::Startup {
        detail: error.to_string(),
    })?;
    // The core's crate version is the workspace version, not the App's release:
    // publish what the host says is installed (PCs show it in the member list).
    if let Some(version) = config.app_version.as_deref().filter(|version| !version.is_empty())
        && let Err(error) = network.set_app_version(version)
    {
        tracing::warn!(%error, "app version not published");
    }
    let link = network.link.clone();
    let identity = MobileV2Identity::from_parts(network_identity.id().0, network.advertisement().group.0, boot.nonce);
    let session = MobileSession::new(
        discovery,
        &config,
        lease,
        network,
        MobileV2Session::new(store, identity, link),
        addresses,
        names,
    );
    session.refresh_presence();
    session.begin_presence();
    Ok(session)
}

#[derive(uniffi::Object)]
pub struct MobileSession {
    id: String,
    addresses: Mutex<addresses::AddressBook>,
    names: names::DeviceNames,
    stream: String,
    discovery: Arc<MobileDiscovery>,
    lease: Mutex<Option<std::fs::File>>,
    runtime: tokio::runtime::Handle,
    listener: Mutex<Option<Arc<dyn MobileEventListener>>>,
    presence: Mutex<presence::Presence>,
    /// `cached_transfer_targets` of this data directory.
    targets_cache: std::path::PathBuf,
    prepared_pairing: Mutex<Option<pairing::PreparedAddress>>,
    acknowledged_pairing_event: std::sync::atomic::AtomicU64,
    media: Arc<MobileMediaSession>,
    clipboard: Arc<MobileClipboard>,
    network: MobileNetwork,
    transfer: Mutex<Option<Arc<MobileV2Session<oink_mobile_core::MobileNetworkLink>>>>,
    /// A listener is subscribed: queued events may be delivered.
    subscribed: tokio::sync::watch::Sender<bool>,
    android_input: Mutex<Option<Arc<MobileAndroidInput>>>,
    /// The embedded router (protocol §14): control page, device sheet lock,
    /// and the privileged Android screen.
    input: Arc<oink_mobile_core::InputRouter>,
    control_generation: Arc<std::sync::atomic::AtomicU64>,
    /// For the restart back-off: how long this session's network ran.
    started: std::time::Instant,
    /// The host's diagnostics collector, once opened.
    diagnostics: Mutex<std::sync::Weak<crate::MobileDiagnosticCollector>>,
}
impl std::fmt::Debug for MobileSession {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("MobileSession")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for MobileSession {
    fn drop(&mut self) {
        let Some(lease) = self.lease.get_mut().unwrap_or_else(|error| error.into_inner()).take() else {
            return;
        };
        let network = self.network.clone();
        self.runtime.spawn(async move {
            network.stop().await;
            drop(lease);
        });
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileSession {
    #[uniffi::constructor]
    pub async fn start(
        config: MobileConfig,
        discovery: Arc<MobileDiscovery>,
        identity_seed: Option<Vec<u8>>,
    ) -> Result<Arc<Self>, MobileError> {
        start_session(config, discovery, identity_seed).await
    }

    /// The host moved the process to another network (Android
    /// `bindProcessToNetwork` after a Wi-Fi reconnect): reopen the endpoint
    /// socket there, on the same port. Live connections continue on the new
    /// socket; a failure is returned for the host to show (review H17).
    pub async fn rebind_network(&self) -> Result<(), MobileError> {
        self.network
            .rebind_endpoint()
            .await
            .map_err(|error| MobileError::Worker {
                detail: error.to_string(),
            })
    }

    /// Report the native owners' current facts. Nothing is renegotiated and
    /// no link is dropped (protocol-v2 1.1).
    pub fn update_platform_facts(&self, facts: MobilePlatformFacts) {
        self.network.update_platform_facts(facts.core());
        self.media.wake_media();
    }

    pub fn platform_facts(&self) -> MobilePlatformFacts {
        let facts = self.network.platform_facts();
        MobilePlatformFacts {
            microphone_send: facts.microphone_send,
            speaker_receive: facts.speaker_receive,
            speaker_send: facts.speaker_send,
            privileged_input_ready: facts.privileged_input_ready,
        }
    }

    /// The capabilities this session negotiates; fixed for its lifetime.
    pub fn advertised_capabilities(&self) -> Vec<String> {
        self.network.advertised_capabilities()
    }

    pub fn open_diagnostics_collector(
        &self,
        session_id: String,
        batch_id: String,
        max_pending_bytes: u64,
        segment_bytes: u32,
    ) -> Result<Arc<MobileDiagnosticCollector>, MobileError> {
        let sink = self.network.diagnostic_sink().map_err(|error| MobileError::Worker {
            detail: error.to_string(),
        })?;
        let collector = MobileDiagnosticCollector::from_network(
            sink,
            self.network.device_id(),
            session_id,
            batch_id,
            max_pending_bytes,
            segment_bytes,
        )
        .map_err(|error| MobileError::Worker {
            detail: error.to_string(),
        })?;
        self.media.attach_diagnostics(&collector);
        *self.diagnostics.lock().unwrap_or_else(|error| error.into_inner()) = Arc::downgrade(&collector);
        Ok(collector)
    }
    pub fn connect_peer(&self, peer_id: String, address: String, port: u16) -> Result<(), MobileError> {
        let address = address
            .parse::<std::net::IpAddr>()
            .map_err(|error| MobileError::InvalidArgument {
                detail: format!("invalid peer address: {error}"),
            })?;
        self.network
            .dial_hex(&peer_id, vec![std::net::SocketAddr::new(address, port)])
            .map_err(|error| MobileError::Command {
                code: UserFacingCode::LinkLost,
                detail: error.to_string(),
            })
    }
    /// Resolve the first directly connected PC's current clipboard, including
    /// its correlated text body. An offline source never returns cached text.
    /// Right after the session started, it waits for the first such PC to be
    /// ready (driven by link events); once the first connect round ended
    /// without one, it fails with `TransferFailure::NoTarget`. `None` means
    /// the PC's current clipboard has no text.
    pub async fn receive_current_clipboard_text(&self) -> Result<Option<MobileCurrentClipboardText>, MobileError> {
        let (peer, transfer) = self.current_clipboard_source().await?;
        transfer
            .receive_current_text(&peer)
            .await
            .map(|value| {
                value.map(|value| MobileCurrentClipboardText {
                    text: value.text,
                    sensitive: value.sensitive,
                    html: value.rich.as_ref().map(|rich| rich.html.clone()),
                    rtf: value.rich.and_then(|rich| rich.rtf),
                    marker: value.marker,
                })
            })
            .map_err(transfer_error)
    }

    /// The group's current clipboard for the one-tap copy shortcut, asked
    /// from the first directly connected PC: text, or an image downloaded on
    /// demand (up to `oink_mobile_core::MAX_CURRENT_IMAGE_BYTES`). None when
    /// the clipboard is empty or its source is offline.
    pub async fn receive_current_clipboard(&self) -> Result<Option<MobileCurrentClipboard>, MobileError> {
        let (peer, transfer) = self.current_clipboard_source().await?;
        let current = transfer
            .receive_current_clipboard(&peer)
            .await
            .map_err(transfer_error)?;
        Ok(current.map(current_clipboard))
    }

    pub fn transfer_request_current(&self, request_id: Vec<u8>, max_items: u32) -> Result<(), MobileError> {
        let request_id: [u8; 16] = request_id.try_into().map_err(|_| MobileError::InvalidArgument {
            detail: "request_id must contain 16 bytes".into(),
        })?;
        let max_items = u16::try_from(max_items).map_err(|_| MobileError::InvalidArgument {
            detail: "max_items exceeds u16".into(),
        })?;
        self.transfer_owner()?
            .request_current(request_id, max_items)
            .map_err(transfer_error)
    }

    /// Records and sends one share. The size limit is checked from file
    /// metadata first; copying and folder packaging run on a blocking
    /// worker, never on the caller's thread. Right after the session started
    /// the share waits for its first target; the outcome always arrives as
    /// the share's `DeliveryEvent` (an error returned here is also recorded
    /// there when the own records exist).
    /// Cancels an own share by its `operation_id` (16 bytes). In progress:
    /// every target is told, uploads stop, and the share's single
    /// `DeliveryEvent` reports `MobileDeliveryFailure::Cancelled`. Called
    /// before `transfer_send_share` recorded it (a hand-over request that was
    /// cancelled at once): that call then fails with
    /// `TransferFailure::Cancelled` and records nothing. Finished or failed
    /// shares: nothing happens.
    pub fn transfer_cancel_share(&self, operation_id: Vec<u8>) -> Result<(), MobileError> {
        let operation_id = operation_id.try_into().map_err(|_| MobileError::InvalidArgument {
            detail: "operation_id must contain 16 bytes".into(),
        })?;
        self.transfer_owner()?
            .cancel_share(operation_id)
            .map_err(transfer_error)
    }

    /// Cancels the phone-to-phone receive that message `message_id` belongs
    /// to. Both ends end it as cancelled (`delivery_failure = Cancelled`,
    /// not retried). Not a running phone-to-phone receive:
    /// `TransferFailure::NotFound`.
    pub fn transfer_cancel_receive(&self, message_id: i64) -> Result<(), MobileError> {
        self.transfer_owner()?
            .cancel_receive(message_id)
            .map_err(transfer_error)
    }

    /// `target_id` is the device the user chose (None: automatic). Text is
    /// broadcast to every online device whoever was chosen. Anything else
    /// goes to the chosen device: a PC keeps it and announces it to the
    /// group (the next online PC takes over before sending if it is
    /// offline); a phone gets it alone, phone to phone, and an offline phone
    /// fails the share with `NoTarget`.
    ///
    /// `wait_for_target` (a share to the group's only other device, sent
    /// without a picker; user 2026-11-11): while no target can take it,
    /// nothing is recorded and this call waits. `TransferShareWaiting
    /// { waiting: true }` says the target is offline (after the first
    /// connect round); once it comes online the share starts by itself
    /// (`waiting: false`, then its records as usual) and this call returns.
    /// `transfer_cancel_share` while waiting makes it fail with
    /// `TransferFailure::Cancelled` and nothing recorded; the session
    /// stopping with `LinkLost`. Keep the staged files until it returns.
    #[allow(clippy::too_many_arguments)] // Typed FFI identity, route and source classification.
    pub async fn transfer_send_share(
        &self,
        operation_id: Vec<u8>,
        message_id: Vec<u8>,
        items: Vec<MobileShareItem>,
        origin: String,
        target_id: Option<String>,
        wait_for_target: bool,
        sensitivity: Option<MobileShareSensitivity>,
    ) -> Result<(), MobileError> {
        let payload = oink_mobile_core::SharePayload {
            sensitivity: sensitivity.unwrap_or(MobileShareSensitivity::Normal).into(),
            items: items
                .into_iter()
                .map(crate::transfer::share_item)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| MobileError::InvalidArgument {
                    detail: error.to_string(),
                })?,
            origin: crate::transfer::send_origin(&origin).map_err(|error| MobileError::InvalidArgument {
                detail: error.to_string(),
            })?,
            operation_id: operation_id.try_into().map_err(|_| MobileError::InvalidArgument {
                detail: "operation_id must contain 16 bytes".into(),
            })?,
            message_id: message_id.try_into().map_err(|_| MobileError::InvalidArgument {
                detail: "message_id must contain 16 bytes".into(),
            })?,
        };
        let choice = match target_id.or_else(|| self.targets_view().active_pc()) {
            None => oink_mobile_core::ProxyChoice::Automatic,
            Some(device) if self.is_mobile_member(&device) => oink_mobile_core::ProxyChoice::Direct {
                name: self.member_name(&device),
                device,
            },
            Some(device) => oink_mobile_core::ProxyChoice::Device(device),
        };
        let transfer = self.transfer_owner()?;
        if wait_for_target {
            return transfer
                .send_share_when_online(payload, choice)
                .await
                .map(|_| ())
                .map_err(transfer_error);
        }
        tokio::task::spawn_blocking(move || transfer.send_share(payload, choice))
            .await
            .map_err(|error| MobileError::Worker {
                detail: error.to_string(),
            })?
            .map(|_| ())
            .map_err(transfer_error)
    }

    /// Starts a typed v2 content read for a historical message. Rust writes
    /// the bytes to the record's fixed path (real file name and extension)
    /// and reports progress through `TransferDownloadProgress` keyed by
    /// `record_id`. `open=true` makes this record the single auto-open
    /// target; `open=false` (plain download button) clears that target.
    pub fn transfer_download(&self, message_id: i64, open: bool) -> Result<(), MobileError> {
        self.transfer_owner()?
            .request_download(message_id, open)
            .map_err(transfer_error)
    }

    /// Asks the source PC for a preview of image message `message_id` when
    /// it becomes visible (protocol-v2 §4.4), or of a PC folder's image
    /// child record (see `transfer_request_directory_thumbnail`). Returns
    /// whether a request was sent: only received images not downloaded,
    /// without a final thumbnail state and with an online source are asked
    /// for. Completion
    /// arrives as `TransferAnnouncement` for the record; read
    /// `image_preview` / `thumbnail_path` from the record then.
    pub fn transfer_request_thumbnail(&self, message_id: i64) -> Result<bool, MobileError> {
        self.transfer_owner()?
            .request_thumbnail(message_id)
            .map_err(transfer_error)
    }

    /// Clears the auto-open target. Call when leaving the transfer page or
    /// when the user taps something else while a "download and open" is
    /// still running, so a late completion does not open a file unasked.
    pub fn cancel_transfer_auto_open(&self) -> Result<(), MobileError> {
        self.transfer_owner()?.cancel_auto_open();
        Ok(())
    }

    /// Takes the auto-open target once its file is complete (one-shot).
    /// Call it when a `TransferAnnouncement` arrives with
    /// `auto_open_ready=true`; then open `path` with QuickLook /
    /// UIDocumentInteraction (iOS) or a FileProvider intent (Android) using
    /// `mime` or the file extension. Returns `None` if the target was
    /// cleared or replaced meanwhile.
    pub fn take_transfer_auto_open(&self) -> Result<Option<MobileTransferOpen>, MobileError> {
        Ok(self
            .transfer_owner()?
            .take_ready_auto_open()
            .map_err(transfer_error)?
            .and_then(|record| {
                Some(MobileTransferOpen {
                    record_id: record.id,
                    path: record.local_path?,
                    display_name: record.display_name,
                    mime: record.mime,
                })
            }))
    }

    /// Downloads one file entry of a folder message's listing. The file is
    /// stored as a child of `folder_message_id` (it never becomes a new
    /// conversation message) and is returned by
    /// `MobileTransferStore.folder_children`. Returns the child record id;
    /// progress and completion events carry it (`TransferAnnouncement`
    /// has `parent_id = folder_message_id`). `open` behaves as in
    /// `transfer_download`.
    pub fn transfer_download_directory_file(
        &self,
        operation_id: Vec<u8>,
        folder_message_id: i64,
        entry: MobileDirectoryEntry,
        open: bool,
    ) -> Result<i64, MobileError> {
        let operation_id = operation_id.try_into().map_err(|_| MobileError::InvalidArgument {
            detail: "operation_id must contain 16 bytes".into(),
        })?;
        self.transfer_owner()?
            .request_directory_file(operation_id, folder_message_id, entry.into(), open)
            .map_err(transfer_error)
    }

    /// Asks for the thumbnail of image file `entry` of folder message
    /// `folder_message_id` when its folder page row becomes visible. For a
    /// PC folder it is the same request, cache and states as
    /// `transfer_request_thumbnail`, on the entry's child record (created,
    /// not downloaded, when the entry has none yet; downloading the file
    /// later uses the same record). Returns that child's id: read
    /// `thumbnail_path` and `image_width/height` from it after its
    /// `TransferAnnouncement` (`parent_id = folder_message_id`). `None`: the
    /// core fetches no thumbnail for this entry (not a file with an image
    /// name, or a folder received phone to phone, whose files are on this
    /// phone: make the thumbnail from `child.local_path`).
    pub fn transfer_request_directory_thumbnail(
        &self,
        folder_message_id: i64,
        entry: MobileDirectoryEntry,
    ) -> Result<Option<i64>, MobileError> {
        self.transfer_owner()?
            .request_directory_thumbnail(folder_message_id, entry.into())
            .map_err(transfer_error)
    }

    /// Lists one page of folder message `content_id` (`node` 0 = its root,
    /// otherwise a directory entry id). The first page passes no cursor;
    /// following pages pass the `next_version` and `next_offset` of the
    /// previous `TransferDirectoryPage` (both or neither). Each listing needs
    /// its own `operation_id`; the page event echoes it together with the
    /// owning `folder_message_id` and this `node` as its `directory`. A
    /// listing that cannot be made ends in `TransferDirectoryFailed` for the
    /// same folder and node instead (now or later); an error here only means
    /// bad arguments or that `content_id` names no received folder.
    pub fn transfer_list_directory(
        &self,
        operation_id: Vec<u8>,
        content_id: String,
        node: u64,
        cursor_version: Option<u64>,
        cursor_offset: Option<u32>,
    ) -> Result<(), MobileError> {
        let operation_id = operation_id.try_into().map_err(|_| MobileError::InvalidArgument {
            detail: "operation_id must contain 16 bytes".into(),
        })?;
        let cursor = match (cursor_version, cursor_offset) {
            (Some(version), Some(offset)) => Some((version, offset)),
            (None, None) => None,
            _ => {
                return Err(MobileError::InvalidArgument {
                    detail: "a directory cursor needs both version and offset".into(),
                });
            }
        };
        self.transfer_owner()?
            .list_directory(operation_id, &content_id, node, cursor)
            .map_err(transfer_error)
    }
    pub fn device_id(&self) -> String {
        self.id.clone()
    }
    /// This phone's ID as the PC shows it: Crockford base32, `XXXX-XXXX-XXXX`.
    pub fn device_fingerprint(&self) -> String {
        self.network.boot().device_id.short()
    }
    pub fn media(&self) -> Arc<MobileMediaSession> {
        self.media.clone()
    }
    pub fn clipboard(&self) -> Arc<MobileClipboard> {
        self.clipboard.clone()
    }
    pub async fn shutdown(self: Arc<Self>) -> Result<(), MobileError> {
        // The native waiter may be cancelled while the network drains.
        tokio::spawn(async move {
            {
                let mut presence = self.presence.lock().unwrap();
                if presence.stopped {
                    return;
                }
                presence.stopped = true;
            }
            self.save_last_seen();
            self.clipboard.cancel_all();
            if let Some(transfer) = self.transfer.lock().unwrap().take() {
                // A share waiting for its offline target ends unrecorded.
                transfer.stop_target_waits();
            }
            self.media.shutdown().await;
            // Release what this phone holds before its links go.
            self.network.set_input_sink(None);
            let input = self.input.clone();
            let _ = tokio::task::spawn_blocking(move || input.stop()).await;
            self.network.stop().await;
            self.lease.lock().unwrap().take();
            self.emit_event(MobileEvent::Stopped);
            self.unsubscribe().await;
        })
        .await
        .map_err(|error| MobileError::Shutdown {
            detail: error.to_string(),
        })
    }
    pub async fn resume(&self) -> Result<(), MobileError> {
        {
            let mut presence = self.presence.lock().unwrap();
            if presence.stopped {
                return Err(MobileError::Stopped);
            }
            if !std::mem::replace(&mut presence.suspended, false) {
                drop(presence);
                self.refresh_presence();
                return Ok(());
            }
        }
        // Links were suspended: requests made now wait for a new round.
        self.network.begin_connect_round();
        self.refresh_presence();
        Ok(())
    }
    pub async fn prepare_suspend(self: Arc<Self>) -> Result<(), MobileError> {
        // A suspended app may be killed without notice.
        self.save_last_seen();
        if self.media.snapshot().running_mask == 0 {
            self.presence.lock().unwrap().suspended = true;
            self.network.suspend_links();
        }
        Ok(())
    }
    pub async fn advertisement(&self) -> Result<MobileAdvertisement, MobileError> {
        let advert = self.network.advertisement();
        let txt: std::collections::HashMap<_, _> = discovery::txt_pairs(&advert).into_iter().collect();
        if txt.is_empty() {
            return Err(MobileError::Startup {
                detail: "invalid mobile advertisement".into(),
            });
        }
        Ok(MobileAdvertisement {
            instance: discovery::instance_name(&advert),
            service_type: oink_protocol::v2::DISCOVERY_SERVICE.into(),
            port: advert.port,
            txt,
        })
    }
    pub async fn subscribe(&self, listener: Box<dyn MobileEventListener>) -> Result<(), MobileError> {
        {
            let presence = self.presence.lock().unwrap();
            if presence.stopped {
                return Err(MobileError::Stopped);
            }
            *self.listener.lock().unwrap() = Some(Arc::from(listener));
        }
        self.subscribed.send_replace(true);
        self.emit_event(MobileEvent::AdvertisementChanged);
        self.emit_event(MobileEvent::DeviceCards {
            snapshot: self.cards_view(),
        });
        self.emit_event(MobileEvent::TransferTargets {
            targets: self.targets_view(),
        });
        Ok(())
    }
    pub async fn unsubscribe(&self) {
        self.subscribed.send_replace(false);
        self.listener.lock().unwrap().take();
    }
    pub async fn nearby(&self) -> Result<NearbySnapshot, MobileError> {
        Ok(self.nearby_view())
    }
    pub async fn device_cards(&self) -> Result<DeviceCardsView, MobileError> {
        Ok(self.cards_view())
    }
    pub async fn blocked_devices(&self) -> Result<BlockedDevicesSnapshot, MobileError> {
        Ok(self.blocked_view())
    }
    /// One entry per online desktop owner: the device its input owner sees
    /// the group pointer locked to. `version_token` toggles that owner's lock
    /// (same token as `DeviceCardStatus.focus.action`).
    pub async fn focus_summaries(&self) -> Result<Vec<FocusSummary>, MobileError> {
        Ok(self.focus_view())
    }
    pub async fn negotiation_failures(&self) -> Result<Vec<PeerCompatibility>, MobileError> {
        Ok(self.compatibility_view())
    }
    pub async fn pairings(&self) -> Result<PairingSnapshot, MobileError> {
        self.pairing_view()
    }
    pub async fn subscribe_pairing_events(&self, after: u64) -> Result<Vec<PairingEvent>, MobileError> {
        self.pairing_errors(after)
    }
    pub async fn device_settings(&self, device: String) -> Result<DeviceSettingsView, MobileError> {
        self.owner_settings(&device)
    }
    pub async fn set_input_transform(&self, _device: String, _value: InputTransform) -> Result<(), MobileError> {
        Err(device_control::unsupported(
            "remote input transform editing is unavailable",
        ))
    }
    pub async fn set_screen_gain(&self, screen: ScreenKey, gain: f64) -> Result<(), MobileError> {
        self.edit_screen_gain(screen, 2, gain).await
    }
    pub async fn set_screen_remote_gain(&self, screen: ScreenKey, gain: f64) -> Result<(), MobileError> {
        self.edit_screen_gain(screen, 4, gain).await
    }
    pub async fn set_screen_disabled(&self, device: String, disabled: bool) -> Result<(), MobileError> {
        self.edit_owner_flag(&device, oink_protocol::v2::dto::LocalOption::ScreenShared, !disabled)
            .await
    }
    pub async fn set_mac_scroll_feel(&self, device: String, value: bool) -> Result<(), MobileError> {
        self.edit_owner_flag(&device, oink_protocol::v2::dto::LocalOption::MacScrollFeel, value)
            .await
    }
    pub async fn set_native_multifinger(&self, device: String, value: bool) -> Result<(), MobileError> {
        self.edit_owner_flag(&device, oink_protocol::v2::dto::LocalOption::NativeMultifinger, value)
            .await
    }
    pub async fn set_touchpad_injection(&self, device: String, value: bool) -> Result<(), MobileError> {
        self.edit_owner_flag(&device, oink_protocol::v2::dto::LocalOption::TouchpadInjection, value)
            .await
    }
    /// Lock the group pointer to `owner` (`locked`) or release its lock.
    /// `token` is that owner's `FocusSummary.version_token` /
    /// `DeviceCardStatus.focus.action` token. Locking to a third device goes
    /// through that device's own card token, so `target` must be `owner`.
    pub async fn set_remote_focus(
        &self,
        request: String,
        owner: String,
        token: String,
        locked: bool,
        target: Option<String>,
    ) -> Result<DeviceControlReply, MobileError> {
        Ok(self
            .edit_focus_lock(request, &owner, token, locked, target.as_deref())
            .await)
    }
    pub async fn invoke_device_card_action(
        &self,
        request: String,
        token: String,
    ) -> Result<DeviceControlReply, MobileError> {
        Ok(self.invoke_action(request, token).await)
    }
    pub async fn leave_group(&self) -> Result<(), MobileError> {
        self.network.leave_group().map_err(device_control::membership_error)?;
        self.refresh_presence();
        Ok(())
    }
    pub async fn start_pairing(&self, _request: String, target: PairTarget) -> Result<PairStartedReply, MobileError> {
        Ok(match self.begin_pairing(target).await {
            Ok(v1) => PairStartedReply::Value { v1 },
            Err(problem) => PairStartedReply::Problem {
                v1: PairRejection::Problem { problem },
            },
        })
    }
    pub async fn accept_pairing(&self, request: String, revision: u64) -> Result<PairAcceptedReply, MobileError> {
        Ok(pairing::accepted(
            request.clone(),
            self.network.accept_pairing(&request, revision),
        ))
    }
    pub async fn submit_pairing_code(
        &self,
        request: String,
        revision: u64,
        attempt: u8,
        code: String,
    ) -> Result<PairAcceptedReply, MobileError> {
        Ok(pairing::accepted(
            request.clone(),
            self.network.submit_pairing_code(&request, revision, attempt, &code),
        ))
    }
    pub async fn decline_pairing(&self, request: String, revision: u64) -> Result<PairAcceptedReply, MobileError> {
        Ok(pairing::accepted(
            request.clone(),
            self.network.decline_pairing(&request, revision),
        ))
    }
    pub async fn cancel_pairing(&self, request: String, revision: u64) -> Result<PairCancelReply, MobileError> {
        Ok(match self.network.cancel_pairing(&request, revision) {
            Ok(outcome) => PairCancelReply::Value {
                v1: match outcome {
                    oink_mobile_core::CancelOutcome::Cancelled => CancelResult::Cancelled,
                    oink_mobile_core::CancelOutcome::CommitUnconfirmed => CancelResult::CommitUnconfirmed,
                    oink_mobile_core::CancelOutcome::AlreadyCommitted => CancelResult::AlreadyCommitted,
                },
            },
            Err(error) => match self.committed_pairing(&request) {
                Some(result) => PairCancelReply::Value { v1: result },
                None => PairCancelReply::Problem {
                    v1: PairRejection::Problem {
                        problem: pairing::network_problem(error),
                    },
                },
            },
        })
    }
    pub async fn pairing_code(&self, id: String) -> Result<CodeView, MobileError> {
        self.network
            .pairing_code(&id)
            .map(|six_digits| CodeView { six_digits })
            .map_err(|error| MobileError::Command {
                code: UserFacingCode::DeviceUnavailable,
                detail: error.to_string(),
            })
    }
    pub async fn ack_pairing_event(&self, event_id: u64) -> Result<(), MobileError> {
        self.acknowledged_pairing_event
            .fetch_max(event_id, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    pub async fn refresh_nearby(&self, request: String) -> Result<PairAcceptedReply, MobileError> {
        self.refresh_presence();
        self.emit_event(MobileEvent::Pairing {
            event: PairingEvent::NearbyRefreshFinished {
                result: TypedResult::Ok,
            },
        });
        Ok(PairAcceptedReply::Value { v1: request })
    }
    pub async fn query_pairing_address(
        &self,
        request: String,
        generation: u64,
        address: String,
        port: Option<u16>,
    ) -> Result<PairAcceptedReply, MobileError> {
        self.query_address(request.clone(), generation, address, port).await;
        Ok(PairAcceptedReply::Value { v1: request })
    }
    pub async fn release_pairing_query(&self, id: String, generation: u64) -> Result<(), MobileError> {
        let mut prepared = self.prepared_pairing.lock().unwrap();
        if prepared
            .as_ref()
            .is_some_and(|value| value.id == id && value.generation == generation)
        {
            *prepared = None;
        }
        Ok(())
    }
    /// Durable local block: hides the device from nearby, declines its
    /// incoming and pending pairing requests. Emits `BlockedDevices`.
    pub async fn block(&self, request: String, device: String) -> Result<BlockReply, MobileError> {
        self.edit_block(request, &device, true)
    }
    pub async fn unblock(&self, request: String, device: String) -> Result<BlockReply, MobileError> {
        self.edit_block(request, &device, false)
    }
    /// Rename a desktop owner, or this phone (same as `set_local_device_name`).
    pub async fn rename_device(&self, device: String, name: String) -> Result<(), MobileError> {
        if device == self.id {
            return self.set_custom_device_name(Some(&name)).map(|_| ());
        }
        self.edit_owner_name(&device, name).await
    }
    pub async fn remove_member(&self, device: String) -> Result<(), MobileError> {
        self.remove_group_member(&device)?;
        self.refresh_presence();
        Ok(())
    }
    pub async fn pause_all(&self, _request: String) -> Result<PauseAllAccepted, MobileError> {
        Err(device_control::unsupported("group emergency pause is unavailable"))
    }
    pub async fn set_forwarding_controls(
        &self,
        request: String,
        token: String,
        volume: u16,
        muted: bool,
    ) -> Result<DeviceControlReply, MobileError> {
        Ok(self.edit_gain(request, token, volume, muted).await)
    }
    pub async fn set_exit_volume(
        &self,
        request: String,
        token: String,
        volume: u8,
        muted: bool,
    ) -> Result<DeviceControlReply, MobileError> {
        Ok(self.edit_exit_volume(request, token, volume, muted).await)
    }
    /// Manual address first (if any), then the addresses discovery sees.
    pub async fn member_addresses(&self, device: String) -> Result<Vec<String>, MobileError> {
        self.known_member_addresses(&device)
    }
    /// `address` is `a.b.c.d` (port 47470) or `a.b.c.d:port`; empty clears it.
    pub async fn set_member_address(&self, device: String, address: String) -> Result<(), MobileError> {
        self.edit_member_address(&device, &address)
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum PairingEvent {
    AddressQueryFinished {
        operation_id: String,
        generation: u64,
        result: AddressQueryResult,
    },
    PairingFailed {
        event_id: u64,
        problem: PairProblem,
    },
    /// A pairing session completed; show "已与 X 配对" (no undo).
    PairingCompleted {
        event_id: u64,
        peer: PeerView,
    },
    PeerCompatibilityChanged {
        v1: PeerCompatibility,
    },
    MembershipEdited {
        result: TypedResult,
    },
    NearbyRefreshFinished {
        result: TypedResult,
    },
}
#[derive(Debug, Clone, uniffi::Enum)]
pub enum MobileEvent {
    DeviceCards {
        snapshot: DeviceCardsView,
    },
    BlockedDevices {
        snapshot: BlockedDevicesSnapshot,
    },
    DeviceCardSignals {
        signals: DeviceCardSignals,
    },
    DeviceControlCompleted {
        result: DeviceControlCompleted,
    },
    Pairing {
        event: PairingEvent,
    },
    Clipboard {
        event: ClipboardEvent,
    },
    AdvertisementChanged,
    PeerCompatibilityChanged {
        status: PeerCompatibility,
    },
    FocusChanged,
    TransferMessage {
        peer_id: String,
        message: String,
    },
    /// A history record was created or changed: reload it by `record_id`
    /// (`MobileTransferStore.message`). `parent_id` is set for a file inside
    /// a folder message. `auto_open_ready` asks the host to call
    /// `take_transfer_auto_open` now.
    TransferAnnouncement {
        record_id: i64,
        content_id: String,
        parent_id: Option<i64>,
        auto_open_ready: bool,
        /// The record is a secret marked by its source PC (see
        /// `MobileMessage.sensitive`): hide it and keep it out of previews.
        sensitive: bool,
    },
    /// Download progress of one record; key progress by `record_id` (the
    /// operation id is shared by all items of one copy).
    TransferDownloadProgress {
        operation_id: Vec<u8>,
        received: u64,
        total: Option<u64>,
        record_id: i64,
    },
    TransferRateSample {
        operation_id: Vec<u8>,
        bytes_per_second: u64,
    },
    /// Upload progress of an own share; `record_ids` are its own records.
    TransferSendProgress {
        operation_id: Vec<u8>,
        sent: u64,
        total: Option<u64>,
        record_ids: Vec<i64>,
    },
    /// Final outcome of an own share (emitted once per operation).
    /// `status` is "Complete" or "Failed"; `delivery_state`/`failure` are the
    /// typed form also stored on the records.
    DeliveryEvent {
        operation_id: Vec<u8>,
        status: String,
        record_ids: Vec<i64>,
        delivery_state: MobileDeliveryState,
        failure: Option<MobileDeliveryFailure>,
        detail: Option<String>,
    },
    TransferError {
        operation_id: Option<Vec<u8>>,
        detail: String,
        record_id: Option<i64>,
    },
    /// One folder page. Request the next page with `next_version` and
    /// `next_offset` (both present or both absent); `folder_message_id` is
    /// the owning conversation record and `operation_id` echoes the request.
    /// `directory` is the `node` the listing was requested with: 0 for the
    /// root page, whatever id the folder's root has (entries' `parent` carry
    /// that real id).
    TransferDirectoryPage {
        directory: u64,
        entries: Vec<MobileDirectoryEntry>,
        next_offset: Option<u32>,
        operation_id: Vec<u8>,
        folder_message_id: i64,
        next_version: Option<u64>,
    },
    /// Listing `operation_id` of `directory` (the requested node, 0 for the
    /// root) of folder message `folder_message_id` ended without a page: the
    /// folder view stops loading and shows `reason`. `retryable` is false
    /// when the content is gone for good (local copy cleared, the source no
    /// longer has it); `detail` is diagnostic text.
    TransferDirectoryFailed {
        operation_id: Vec<u8>,
        folder_message_id: i64,
        directory: u64,
        reason: TransferFailure,
        retryable: bool,
        detail: String,
    },
    /// The devices a transfer can go to changed (`transfer_targets`).
    TransferTargets {
        targets: TransferTargetsView,
    },
    /// The core deleted these records: an own share the user cancelled, or
    /// a phone-to-phone transfer its sender cancelled. Drop them.
    TransferRemoved {
        record_ids: Vec<i64>,
    },
    /// A phone-to-phone transfer started arriving (its first record exists).
    /// In the background, Android notifies with it ("sender: first_name"
    /// or "N files") and opens the transfer page at `record_ids[0]`.
    TransferDirectIncoming {
        operation_id: Vec<u8>,
        sender_id: String,
        sender_name: String,
        record_ids: Vec<i64>,
        item_count: u32,
        first_name: String,
        total_bytes: u64,
    },
    /// An own share sent with `wait_for_target` waits for its offline
    /// target (`waiting`: show「对方不在线…」), or the target came online and
    /// it starts now (`!waiting`, only after a `waiting`). Nothing is
    /// recorded while it waits.
    TransferShareWaiting {
        operation_id: Vec<u8>,
        waiting: bool,
    },
    /// A phone-to-phone receive ended: complete, failed or cancelled.
    TransferDirectEnded {
        operation_id: Vec<u8>,
        record_ids: Vec<i64>,
        completed: bool,
        failure: Option<MobileDeliveryFailure>,
    },
    ResyncRequired,
    Stopped,
    /// The network layer ended with a fatal error (its QUIC endpoint
    /// stopped; user 2026-11-12: fail fast). This session cannot recover:
    /// its Boot cannot rejoin with a new network layer. The host ends it
    /// (`shutdown`) and starts a new session through its normal start after
    /// `retry_after_ms` (1 s doubling to 30 s across quick failures, reset
    /// after a stable minute). `detail` is diagnostic text.
    NetworkFailed {
        detail: String,
        retry_after_ms: u64,
    },
}
#[uniffi::export(callback_interface)]
pub trait MobileEventListener: Send + Sync {
    fn on_event(&self, event: MobileEvent);
}

fn load_seed(config: &MobileConfig, supplied: Option<Vec<u8>>) -> Result<[u8; 32], MobileError> {
    let bytes = if let Some(bytes) = supplied {
        bytes
    } else {
        let key = crate::device_only::dir(std::path::Path::new(&config.data_dir)).join("device.key");
        std::fs::read(key).map_err(|error| MobileError::Startup {
            detail: format!("cannot load mobile identity: {error}"),
        })?
    };
    bytes.try_into().map_err(|_| MobileError::InvalidArgument {
        detail: "identity seed must contain 32 bytes".into(),
    })
}

/// Format any 64-hex device ID (cards, nearby rows, pairing peers) as the
/// PC's Crockford base32 `XXXX-XXXX-XXXX` fingerprint.
#[uniffi::export]
pub fn device_fingerprint(device: String) -> Result<String, MobileError> {
    let bytes: Vec<u8> = (0..device.len())
        .step_by(2)
        .map(|index| {
            device
                .get(index..index + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| MobileError::InvalidArgument {
        detail: "device ID must be 64 hex characters".into(),
    })?;
    Ok(DeviceId(bytes).short())
}

/// What `initialize_mobile_group` did with this device's group state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MobileGroupProvision {
    /// The saved group is kept. A state that does not decode is kept too:
    /// `MobileSession.start` reports it.
    Existing,
    /// First launch: a new group of this device alone.
    Created,
    /// The saved group did not contain this device's identity: on iOS the
    /// App Group data came from another iPhone (device migration or a
    /// restored backup) while the ThisDeviceOnly Keychain identity stayed
    /// behind. It was replaced by a new group of this device alone, and the
    /// host asks the user to pair again (decision D8).
    Replaced,
}

/// First-launch provisioning is explicit: a share extension cannot silently
/// create a new group. Files left by unreleased pre-v2 mobile builds (such as
/// `group-state.json`) are not a group and do not block provisioning.
#[uniffi::export]
pub fn initialize_mobile_group(
    config: MobileConfig,
    identity_seed: Option<Vec<u8>>,
) -> Result<MobileGroupProvision, MobileError> {
    use oink_network::{group_state::State, identity::Identity};
    use oink_protocol::v2::{self, schema::WireDto};
    // Provisioning runs before the first session start; log it too.
    crate::logging::install();
    let path = std::path::Path::new(&config.data_dir);
    let state_path = crate::device_only::group_state(path);
    if state_path.try_exists().map_err(startup_io)?
        && !restored_without_identity(&config, &state_path, identity_seed.as_deref())?
    {
        return Ok(MobileGroupProvision::Existing);
    }
    let _lease = acquire_mobile_lease(path)?;
    let replacing = state_path.try_exists().map_err(startup_io)?;
    if replacing && !restored_without_identity(&config, &state_path, identity_seed.as_deref())? {
        return Ok(MobileGroupProvision::Existing);
    }
    if matches!(config.profile, SessionProfile::ShareSession) {
        return Err(MobileError::Startup {
            detail: "existing mobile group state is required".into(),
        });
    }
    // Use the same owner-only key file format as the desktop and phase2.
    // An iOS Keychain seed remains owned by the host and is never written here.
    let identity = match identity_seed {
        Some(seed) => Identity::from_seed(load_seed(&config, Some(seed))?),
        None if matches!(config.platform, MobilePlatform::Android) => {
            Identity::load_or_create(&crate::device_only::dir(path)).map_err(startup_io)?
        }
        None => {
            return Err(MobileError::InvalidArgument {
                detail: "iOS requires its Keychain seed".into(),
            });
        }
    };
    let os = match config.platform {
        MobilePlatform::Android => "android",
        MobilePlatform::Ios => "ios",
    };
    let name = names::effective(path, &config.device_name)?;
    let bytes = State::create(&identity, &name, os, presence::now_ms())
        .and_then(|state| state.wire().encode(v2::limits::CONFIG_STATE))
        .map_err(|error| MobileError::Startup {
            detail: error.to_string(),
        })?;
    std::fs::create_dir_all(crate::device_only::dir(path)).map_err(startup_io)?;
    oink_foundation::storage::atomic_write(&state_path, &bytes).map_err(startup_io)?;
    Ok(if replacing {
        MobileGroupProvision::Replaced
    } else {
        MobileGroupProvision::Created
    })
}

/// D8: the app's saved group is valid but does not list the identity the host
/// supplied (the iOS Keychain seed). Only the app profile with a supplied seed
/// is checked; an unreadable state is not this case.
fn restored_without_identity(
    config: &MobileConfig,
    state_path: &std::path::Path,
    identity_seed: Option<&[u8]>,
) -> Result<bool, MobileError> {
    let (Some(seed), SessionProfile::Mobile) = (identity_seed, &config.profile) else {
        return Ok(false);
    };
    let seed: [u8; 32] = seed.try_into().map_err(|_| MobileError::InvalidArgument {
        detail: "identity seed must contain 32 bytes".into(),
    })?;
    let device = oink_network::identity::Identity::from_seed(seed).id();
    let bytes = std::fs::read(state_path).map_err(startup_io)?;
    Ok(matches!(
        oink_mobile_core::decode_group_state(&bytes, device),
        Err(oink_mobile_core::MobileNetworkError::GroupState {
            problem: oink_mobile_core::GroupStateProblem::NotMember,
            ..
        })
    ))
}

fn acquire_mobile_lease(path: &std::path::Path) -> Result<std::fs::File, MobileError> {
    use std::fs::{OpenOptions, TryLockError};
    std::fs::create_dir_all(path).map_err(startup_io)?;
    let file = path.join("mobile-session.lock");
    let owner = match oink_foundation::storage::create_private_new(&file) {
        Ok(created) => created,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file)
            .map_err(startup_io)?,
        Err(error) => return Err(startup_io(error)),
    };
    match owner.try_lock() {
        Ok(()) => Ok(owner),
        Err(TryLockError::WouldBlock) => Err(MobileError::OwnerBusy),
        Err(TryLockError::Error(error)) => Err(startup_io(error)),
    }
}

fn startup_io(error: std::io::Error) -> MobileError {
    MobileError::Startup {
        detail: error.to_string(),
    }
}

fn current_clipboard(value: oink_mobile_core::CurrentClipboard) -> MobileCurrentClipboard {
    use oink_mobile_core::{AttachmentKind, CurrentClipboard};
    match value {
        CurrentClipboard::Text(text) => MobileCurrentClipboard::Text {
            text: text.text,
            sensitive: text.sensitive,
            html: text.rich.as_ref().map(|rich| rich.html.clone()),
            rtf: text.rich.and_then(|rich| rich.rtf),
            marker: text.marker,
        },
        CurrentClipboard::Image(image) => MobileCurrentClipboard::Image {
            path: image.path,
            mime: image.mime,
            size: image.size,
            sensitive: image.sensitive,
            marker: image.marker,
        },
        CurrentClipboard::ImageTooLarge { size, limit } => MobileCurrentClipboard::ImageTooLarge { size, limit },
        CurrentClipboard::Unsupported { kind } => MobileCurrentClipboard::Unsupported {
            kind: match kind {
                AttachmentKind::Text => MobileContentKind::Text,
                AttachmentKind::Image => MobileContentKind::Image,
                AttachmentKind::File => MobileContentKind::File,
                AttachmentKind::Folder => MobileContentKind::Folder,
                AttachmentKind::RichText => MobileContentKind::RichText,
            },
        },
    }
}

fn transfer_error(error: MobileV2TransportError) -> MobileError {
    MobileError::Transfer {
        reason: transfer_failure(&error),
        detail: error.to_string(),
    }
}

fn transfer_failure(error: &MobileV2TransportError) -> TransferFailure {
    use oink_mobile_core::SourceAvailability;
    match error {
        MobileV2TransportError::Protocol(_) => TransferFailure::Incompatible,
        MobileV2TransportError::Link(_) => TransferFailure::LinkLost,
        MobileV2TransportError::PeerUnavailable(_) => TransferFailure::SourceOffline,
        MobileV2TransportError::PeerPaused(_) => TransferFailure::Paused,
        MobileV2TransportError::SendDisabled => TransferFailure::SendDisabled,
        MobileV2TransportError::NoTarget => TransferFailure::NoTarget,
        MobileV2TransportError::SourceUnavailable(availability) => match availability {
            SourceAvailability::Ready | SourceAvailability::SourceOffline => TransferFailure::SourceOffline,
            SourceAvailability::Evicted => TransferFailure::Evicted,
            SourceAvailability::NoSuchItem => TransferFailure::NoSuchItem,
            SourceAvailability::Superseded => TransferFailure::Superseded,
            SourceAvailability::Unsupported => TransferFailure::Unsupported,
        },
        MobileV2TransportError::Expired => TransferFailure::Expired,
        MobileV2TransportError::NotFound(_) => TransferFailure::NotFound,
        MobileV2TransportError::OwnMessage(_) => TransferFailure::OwnMessage,
        MobileV2TransportError::InProgress => TransferFailure::InProgress,
        MobileV2TransportError::StorageFull => TransferFailure::StorageFull,
        MobileV2TransportError::Cancelled => TransferFailure::Cancelled,
        MobileV2TransportError::Local(_) => TransferFailure::LocalRead,
        MobileV2TransportError::Storage(_) => TransferFailure::Storage,
    }
}

fn transfer_event(event: TransferEvent) -> MobileEvent {
    match event {
        TransferEvent::MessageReceived { peer_id, message } => MobileEvent::TransferMessage { peer_id, message },
        TransferEvent::Announcement {
            record_id,
            content_id,
            parent_id,
            auto_open_ready,
            sensitive,
        } => MobileEvent::TransferAnnouncement {
            record_id,
            content_id,
            parent_id,
            auto_open_ready,
            sensitive,
        },
        TransferEvent::DownloadProgress {
            operation_id,
            record_id,
            received,
            total,
        } => MobileEvent::TransferDownloadProgress {
            operation_id: operation_id.to_vec(),
            received,
            total,
            record_id,
        },
        TransferEvent::RateSample {
            operation_id,
            bytes_per_second,
        } => MobileEvent::TransferRateSample {
            operation_id: operation_id.to_vec(),
            bytes_per_second,
        },
        TransferEvent::SendProgress {
            operation_id,
            record_ids,
            sent,
            total,
        } => MobileEvent::TransferSendProgress {
            operation_id: operation_id.to_vec(),
            sent,
            total,
            record_ids,
        },
        TransferEvent::DeliveryEvent {
            operation_id,
            record_ids,
            status,
            delivery,
            failure,
            detail,
        } => MobileEvent::DeliveryEvent {
            operation_id: operation_id.to_vec(),
            status,
            record_ids,
            delivery_state: delivery.into(),
            failure: failure.map(Into::into),
            detail,
        },
        TransferEvent::Error {
            operation_id,
            record_id,
            detail,
        } => MobileEvent::TransferError {
            operation_id: operation_id.map(|value| value.to_vec()),
            detail,
            record_id,
        },
        TransferEvent::Removed { record_ids } => MobileEvent::TransferRemoved { record_ids },
        TransferEvent::DirectIncoming {
            operation_id,
            sender_id,
            record_ids,
            item_count,
            first_name,
            total_bytes,
        } => MobileEvent::TransferDirectIncoming {
            operation_id: operation_id.to_vec(),
            sender_id,
            sender_name: String::new(),
            record_ids,
            item_count,
            first_name,
            total_bytes,
        },
        TransferEvent::ShareWaiting { operation_id, waiting } => MobileEvent::TransferShareWaiting {
            operation_id: operation_id.to_vec(),
            waiting,
        },
        TransferEvent::DirectEnded {
            operation_id,
            record_ids,
            completed,
            failure,
        } => MobileEvent::TransferDirectEnded {
            operation_id: operation_id.to_vec(),
            record_ids,
            completed,
            failure: failure.map(Into::into),
        },
        TransferEvent::DirectoryPage {
            operation_id,
            folder_message_id,
            directory,
            entries,
            next_version,
            next_offset,
        } => MobileEvent::TransferDirectoryPage {
            directory,
            entries: entries.into_iter().map(Into::into).collect(),
            next_offset,
            operation_id: operation_id.to_vec(),
            folder_message_id,
            next_version,
        },
        TransferEvent::DirectoryFailed {
            operation_id,
            folder_message_id,
            directory,
            error,
            retryable,
        } => MobileEvent::TransferDirectoryFailed {
            operation_id: operation_id.to_vec(),
            folder_message_id,
            directory,
            reason: transfer_failure(&error),
            retryable,
            detail: error.to_string(),
        },
    }
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct MobileTransferOpen {
    pub record_id: i64,
    pub path: String,
    pub display_name: String,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct MobileDirectoryEntry {
    pub id: u64,
    pub parent: Option<u64>,
    pub name: String,
    pub kind: String,
    pub size: Option<u64>,
    pub version: u64,
    /// This file's record under the folder message when it has one, as of
    /// the listing (download state, `local_path`, thumbnail, button state):
    /// every file of a folder received phone to phone (complete, make its
    /// thumbnail from `local_path`), a PC folder's file once downloaded or
    /// asked for a thumbnail. Later changes arrive as `TransferAnnouncement`
    /// with `parent_id`; match `directory_node` with `id`. Ignored when the
    /// entry is passed back in.
    #[uniffi(default = None)]
    pub child: Option<crate::MobileMessage>,
}

impl From<DirectoryEntry> for MobileDirectoryEntry {
    fn from(value: DirectoryEntry) -> Self {
        Self {
            id: value.id,
            parent: value.parent,
            name: value.name,
            kind: value.kind,
            size: value.size,
            version: value.version,
            child: value.child.map(Into::into),
        }
    }
}

impl From<MobileDirectoryEntry> for DirectoryEntry {
    fn from(value: MobileDirectoryEntry) -> Self {
        Self {
            id: value.id,
            parent: value.parent,
            name: value.name,
            kind: value.kind,
            size: value.size,
            version: value.version,
            child: None,
        }
    }
}

#[uniffi::export]
impl MobileSession {
    /// The control page's handle (protocol §14.6). A newer handle replaces
    /// the previous one; `close()` releases everything this phone holds.
    pub fn control(&self, listener: Box<dyn MobileControlListener>) -> Arc<MobileControl> {
        self.open_control(listener)
    }
    /// Privileged Android control: the helper is the embedded router's own
    /// screen; I0 traffic reaches the router directly, no host poll.
    pub fn android_input(&self, listener: Box<dyn AndroidInputListener>) -> Arc<MobileAndroidInput> {
        let network = self.network.clone();
        let input = MobileAndroidInput::new(listener, self.input.clone(), move |display| {
            network.set_android_input_display(display).is_ok()
        });
        *self.android_input.lock().expect("android input mutex poisoned") = Some(input.clone());
        input
    }
    pub fn android_controls(&self, listener: Box<dyn AndroidControlListener>) -> Arc<AndroidControls> {
        self.media.bind_android_controls(listener)
    }
}

impl MobileSession {
    fn transfer_owner(&self) -> Result<Arc<MobileV2Session<oink_mobile_core::MobileNetworkLink>>, MobileError> {
        self.transfer
            .lock()
            .map_err(|_| MobileError::Worker {
                detail: "transfer mutex poisoned".into(),
            })?
            .clone()
            .ok_or(MobileError::Stopped)
    }
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ClipboardFailure {
    Cancelled,
    Busy,
    Unknown,
    Timeout,
    Superseded,
    Unsupported,
    SelectTarget,
    NoTargets,
    Io,
    Protocol,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct OfflineMember {
    pub device: String,
    pub fingerprint: String,
    pub name: String,
    pub os: String,
    pub is_self: bool,
    /// When this member was added to the group (signed roster record).
    pub added_ms: u64,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct OfflineSnapshot {
    pub revision: u64,
    pub members: Vec<OfflineMember>,
}
#[derive(Debug, Clone, uniffi::Record)]
pub struct OfflineConfig {
    pub directory: String,
    pub device_name: String,
    pub platform: MobilePlatform,
}
/// Group management while no `MobileSession` runs (e.g. the session cannot
/// start). Holds the mobile owner lease, so it never races a live session.
#[derive(Debug, uniffi::Object)]
pub struct OfflineManagement {
    config: OfflineConfig,
    identity_seed: Option<Vec<u8>>,
    lease: Mutex<Option<std::fs::File>>,
}
#[uniffi::export]
impl OfflineManagement {
    /// `OwnerBusy` while a session owns the data directory.
    #[uniffi::constructor]
    pub fn open(config: OfflineConfig, identity_seed: Option<Vec<u8>>) -> Result<Arc<Self>, MobileError> {
        let lease = acquire_mobile_lease(std::path::Path::new(&config.directory))?;
        Ok(Arc::new(Self {
            config,
            identity_seed,
            lease: Mutex::new(Some(lease)),
        }))
    }
    /// Members of the persisted signed group state.
    pub fn snapshot(&self) -> Result<OfflineSnapshot, MobileError> {
        self.held()?;
        let state = self.read_state()?;
        let local = self.identity()?.id();
        let mut revision = 0u64;
        let members = state
            .roster()
            .members()
            .map(|member| {
                revision = revision.max(member.issued_ms);
                OfflineMember {
                    device: member.device.to_hex(),
                    fingerprint: member.device.short(),
                    name: member.name.clone().unwrap_or_else(|| member.device.short()),
                    os: member.os.clone().unwrap_or_default(),
                    is_self: member.device == local,
                    added_ms: member.issued_ms,
                }
            })
            .collect();
        Ok(OfflineSnapshot { revision, members })
    }
    /// Leave while offline: this phone starts a new group of its own. The
    /// signed departure cannot reach the other members now, so they keep
    /// listing this phone as an offline member until one of them removes it;
    /// they can no longer admit it into the old group either way.
    pub fn leave_group(&self) -> Result<OfflineSnapshot, MobileError> {
        self.held()?;
        let state = self.read_state()?;
        let identity = self.identity()?;
        if state.roster().members().count() == 1 {
            return Err(MobileError::Command {
                code: UserFacingCode::StateConflict,
                detail: "device is already alone".into(),
            });
        }
        let directory = std::path::Path::new(&self.config.directory);
        let name = names::effective(directory, &self.config.device_name)?;
        let os = match self.config.platform {
            MobilePlatform::Android => "android",
            MobilePlatform::Ios => "ios",
        };
        let bytes = oink_network::group_state::State::create(&identity, &name, os, presence::now_ms())
            .and_then(|state| {
                use oink_protocol::v2::schema::WireDto;
                state.wire().encode(oink_protocol::v2::limits::CONFIG_STATE)
            })
            .map_err(|error| MobileError::Command {
                code: UserFacingCode::BackendFailed,
                detail: error.to_string(),
            })?;
        let state_path = crate::device_only::group_state(directory);
        oink_foundation::storage::atomic_write(&state_path, &bytes).map_err(|error| MobileError::Command {
            code: UserFacingCode::PersistenceFailed,
            detail: error.to_string(),
        })?;
        self.snapshot()
    }
    pub fn shutdown(&self) {
        self.lease.lock().unwrap_or_else(|error| error.into_inner()).take();
    }
}
impl OfflineManagement {
    fn held(&self) -> Result<(), MobileError> {
        self.lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|_| ())
            .ok_or(MobileError::Stopped)
    }
    fn read_state(&self) -> Result<oink_network::group_state::State, MobileError> {
        use oink_protocol::v2::schema::WireDto;
        let path = crate::device_only::group_state(std::path::Path::new(&self.config.directory));
        let bytes = std::fs::read(&path).map_err(startup_io)?;
        oink_protocol::v2::cbor::decode_map(&bytes, oink_protocol::v2::limits::CONFIG_STATE)
            .and_then(oink_protocol::v2::dto::GroupState::from_raw)
            .and_then(oink_network::group_state::State::from_wire)
            .map_err(|error| MobileError::Startup {
                detail: format!("group state: {error}"),
            })
    }
    fn identity(&self) -> Result<oink_network::identity::Identity, MobileError> {
        let config = MobileConfig {
            data_dir: self.config.directory.clone(),
            cache_dir: None,
            log_dir: None,
            device_name: self.config.device_name.clone(),
            platform: self.config.platform,
            profile: SessionProfile::Mobile,
            // Only the seed location is read here; the shape is not needed.
            form_factor: None,
            app_version: None,
        };
        Ok(oink_network::identity::Identity::from_seed(load_seed(
            &config,
            self.identity_seed.clone(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_facts_map_to_the_core_facts() {
        let facts = MobilePlatformFacts {
            microphone_send: true,
            speaker_receive: false,
            speaker_send: false,
            privileged_input_ready: true,
        };
        let core = facts.core();
        assert!(core.microphone_send);
        assert!(!core.speaker_receive);
        assert!(core.privileged_input_ready);
    }

    fn android_config(dir: &std::path::Path, profile: SessionProfile) -> MobileConfig {
        MobileConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            cache_dir: None,
            log_dir: None,
            device_name: "an Android test device".into(),
            platform: MobilePlatform::Android,
            profile,
            form_factor: None,
            app_version: None,
        }
    }

    #[test]
    fn first_launch_ignores_files_from_pre_v2_mobile_builds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("group-state.json"), b"{\"schema\":1}").unwrap();
        initialize_mobile_group(android_config(dir.path(), SessionProfile::Mobile), None).unwrap();
        assert!(crate::device_only::group_state(dir.path()).exists());
    }

    fn file_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Backup split: the identity, everything naming this phone's group
    /// (state, pairing blocks, member addresses) and the custom device name
    /// are written into the device-only directory that the hosts keep out of
    /// backup and migration; the data directory root keeps only backed-up
    /// data and the owner lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_and_group_files_are_written_to_the_device_only_directory() {
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let dir = tempfile::tempdir().unwrap();
        let config = android_config(dir.path(), SessionProfile::Mobile);
        initialize_mobile_group(config.clone(), None).unwrap();
        let device_only = crate::device_only::dir(dir.path());
        assert_eq!(file_names(&device_only), ["device.key", "group-state.v2"]);

        // A second member, so this phone has an address to remember.
        let phone = Identity::load_existing(&device_only).unwrap();
        let peer = Identity::from_seed([42; 32]).id();
        let base = State::create(&phone, "phone", "android", 1).unwrap();
        let roster = base.roster().add(&phone, peer, "peer", "windows", 2).unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        oink_foundation::storage::atomic_write(&crate::device_only::group_state(dir.path()), &state).unwrap();

        let session = MobileSession::start(config, MobileDiscovery::new(), None)
            .await
            .unwrap();
        session
            .set_member_address(peer.to_hex(), "192.0.2.1".into())
            .await
            .unwrap();
        session.block("1".into(), "ab".repeat(32)).await.unwrap();
        session.set_local_device_name(Some("Desk phone".into())).unwrap();
        session.shutdown().await.unwrap();

        let kept = file_names(&device_only);
        for name in [
            "device.key",
            "device-name.json",
            "group-state.v2",
            "member-addresses.json",
            "pairing-blocks.json",
        ] {
            assert!(kept.iter().any(|kept| kept == name), "{name} missing from {kept:?}");
        }
        for name in file_names(dir.path()) {
            assert!(
                matches!(name.as_str(), "device-only" | "transfers" | "mobile-session.lock")
                    || name.starts_with("transfer.sqlite"),
                "{name} is in the backed-up data directory"
            );
        }
    }

    #[test]
    fn transfer_failures_keep_their_kind_at_the_host_boundary() {
        use oink_mobile_core::SourceAvailability;
        let reason = |error| match transfer_error(error) {
            MobileError::Transfer { reason, .. } => reason,
            other => panic!("untyped transfer error: {other:?}"),
        };
        assert_eq!(
            reason(MobileV2TransportError::SourceUnavailable(
                SourceAvailability::SourceOffline
            )),
            TransferFailure::SourceOffline
        );
        assert_eq!(
            reason(MobileV2TransportError::SourceUnavailable(SourceAvailability::Evicted)),
            TransferFailure::Evicted
        );
        assert_eq!(
            reason(MobileV2TransportError::SourceUnavailable(
                SourceAvailability::NoSuchItem
            )),
            TransferFailure::NoSuchItem
        );
        assert_eq!(
            reason(MobileV2TransportError::SourceUnavailable(
                SourceAvailability::Superseded
            )),
            TransferFailure::Superseded
        );
        assert_eq!(reason(MobileV2TransportError::Expired), TransferFailure::Expired);
        assert_eq!(reason(MobileV2TransportError::NoTarget), TransferFailure::NoTarget);
        assert_eq!(
            reason(MobileV2TransportError::StorageFull),
            TransferFailure::StorageFull
        );
        assert_eq!(reason(MobileV2TransportError::Cancelled), TransferFailure::Cancelled);
        assert_eq!(reason(MobileV2TransportError::NotFound(3)), TransferFailure::NotFound);
        assert_eq!(
            reason(MobileV2TransportError::Link("gone".into())),
            TransferFailure::LinkLost
        );
    }

    /// A failed listing reaches the host as a page-level failure of the same
    /// folder and node, with the typed reason the folder view shows.
    #[test]
    fn a_failed_listing_reaches_the_host_with_its_folder_node_and_reason() {
        use oink_mobile_core::SourceAvailability;
        let event = transfer_event(TransferEvent::DirectoryFailed {
            operation_id: [3; 16],
            folder_message_id: 34,
            directory: 0,
            error: MobileV2TransportError::SourceUnavailable(SourceAvailability::Evicted),
            retryable: false,
        });
        let MobileEvent::TransferDirectoryFailed {
            operation_id,
            folder_message_id,
            directory,
            reason,
            retryable,
            ..
        } = event
        else {
            panic!("expected a directory failure, got {event:?}")
        };
        assert_eq!(
            (operation_id, folder_message_id, directory, reason, retryable),
            (vec![3; 16], 34, 0, TransferFailure::Evicted, false)
        );
    }

    struct Events(tokio::sync::mpsc::UnboundedSender<MobileEvent>);

    impl MobileEventListener for Events {
        fn on_event(&self, event: MobileEvent) {
            let _ = self.0.send(event);
        }
    }

    /// User 2026-11-12: a network layer that ends on its own is reported to
    /// the host with the restart back-off; the host ends the session and a
    /// new one (new Boot, same identity) starts in the same data directory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_network_failure_asks_the_host_to_restart_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let config = android_config(dir.path(), SessionProfile::Mobile);
        initialize_mobile_group(config.clone(), None).unwrap();
        let session = MobileSession::start(config.clone(), MobileDiscovery::new(), None)
            .await
            .unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();
        let boot = session.network.boot();
        session.network.inject_fatal_for_test();
        let (detail, retry_after_ms) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(MobileEvent::NetworkFailed { detail, retry_after_ms }) => break (detail, retry_after_ms),
                    Some(_) => continue,
                    None => panic!("event listener closed"),
                }
            }
        })
        .await
        .expect("the host hears about the failure");
        assert!(detail.contains("endpoint driver"), "{detail}");
        // The back-off is process-wide: 1 s first, at most 30 s.
        assert!((1_000..=30_000).contains(&retry_after_ms), "{retry_after_ms}");
        // What the host does: end this session, then start a new one.
        session.shutdown().await.unwrap();
        let restarted = MobileSession::start(config, MobileDiscovery::new(), None)
            .await
            .unwrap();
        assert_eq!(restarted.network.boot().device_id, boot.device_id);
        assert_ne!(restarted.network.boot(), boot, "a restarted session has a new Boot");
        restarted.shutdown().await.unwrap();
    }

    /// User rule 2026-11-11: a running session takes a pairing request with
    /// no page or foreground signal from the host. The request waits in the
    /// request list (the add button's badge), the host hears about it from
    /// the pairing event, and nothing accepts or ends it on its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_running_session_lists_a_pairing_request_until_the_user_takes_it() {
        use oink_mobile_core::MobileNetworkConfig;
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let phone_dir = tempfile::tempdir().unwrap();
        initialize_mobile_group(android_config(phone_dir.path(), SessionProfile::Mobile), None).unwrap();
        let session = MobileSession::start(
            android_config(phone_dir.path(), SessionProfile::Mobile),
            MobileDiscovery::new(),
            None,
        )
        .await
        .unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();
        let phone_id = session.network.boot().device_id;
        let phone_anchor = State::from_wire(
            oink_protocol::v2::dto::GroupState::from_raw(
                oink_protocol::v2::cbor::decode_map(
                    &session.network.state_bytes().unwrap(),
                    oink_protocol::v2::limits::CONFIG_STATE,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap()
        .roster()
        .anchor();

        let peer_seed = [42; 32];
        let peer_identity = Identity::from_seed(peer_seed);
        let peer_dir = tempfile::tempdir().unwrap();
        let peer = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: peer_seed,
            boot: Boot {
                device_id: peer_identity.id(),
                nonce: [6; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(
                State::create(&peer_identity, "peer", "ios", 1)
                    .unwrap()
                    .wire()
                    .encode(oink_protocol::v2::limits::CONFIG_STATE)
                    .unwrap(),
            ),
            group_state_path: peer_dir.path().join("group-state.v2"),
            pairing_journal_path: peer_dir.path().join("pairing-recovery.cbor"),
            device_name: "peer".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: Default::default(),
        })
        .await
        .unwrap();
        let port = session.advertisement().await.unwrap().port;
        let phone_addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port).into();
        let mut key = None;
        for _ in 0..400 {
            let _ = peer.dial(phone_id, vec![phone_addr]);
            if let Ok(value) = peer.start_pairing_with_anchor(phone_id, phone_anchor) {
                key = Some(value);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let key = key.expect("the peer reaches the phone to pair");

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.expect("listener open");
                if matches!(event, MobileEvent::Pairing { .. })
                    && session.pairings().await.unwrap().requests.iter().any(|r| r.id == key)
                {
                    break;
                }
            }
        })
        .await
        .expect("a pairing event announces the listed request");
        // Several presence rounds later the request still waits: no auto
        // accept into code entry, no refusal, nothing ended.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let listed = session.pairings().await.unwrap();
        let request = listed
            .requests
            .iter()
            .find(|r| r.id == key)
            .expect("still listed")
            .clone();
        assert!(!listed.sessions.iter().any(|s| s.id == key));
        assert!(
            peer.pairing_snapshot().unwrap().sessions.iter().any(|s| {
                oink_network::pairing::key(s.id) == key && s.phase == oink_network::pairing::Phase::ShowCode
            })
        );

        let accepted = session
            .accept_pairing(request.id.clone(), request.revision)
            .await
            .unwrap();
        assert!(matches!(accepted, PairAcceptedReply::Value { .. }), "{accepted:?}");
        let after = session.pairings().await.unwrap();
        assert!(after.requests.is_empty());
        assert!(
            after.sessions.iter().any(|s| s.id == key && s.phase == 2),
            "code entry after accept"
        );
        session.clone().shutdown().await.unwrap();
        peer.stop().await;
    }

    /// H10, H11 and M19 on a running session. A share made right after the
    /// start, before any link, waits for the first ready target instead of
    /// failing; its outcome is pushed to the listener (no host poll). A new
    /// discovery generation (the host saw a network path change) then keeps
    /// the healthy link: the peer's next share still arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn early_share_waits_for_its_target_and_a_path_change_keeps_the_link() {
        use oink_mobile_core::{MobileNetworkConfig, Peer, PeerKind, PeerState, SendItem, SendOrigin, SharePayload};
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let phone_seed = [21; 32];
        let peer_seed = [22; 32];
        let phone_identity = Identity::from_seed(phone_seed);
        let peer_identity = Identity::from_seed(peer_seed);
        let base = State::create(&phone_identity, "phone", "android", 1).unwrap();
        let roster = base
            .roster()
            .add(&phone_identity, peer_identity.id(), "peer", "android", 2)
            .unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        let phone_dir = tempfile::tempdir().unwrap();
        let group_state = crate::device_only::group_state(phone_dir.path());
        std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
        std::fs::write(group_state, &state).unwrap();
        let discovery = MobileDiscovery::new();
        let session = MobileSession::start(
            android_config(phone_dir.path(), SessionProfile::Mobile),
            discovery.clone(),
            Some(phone_seed.to_vec()),
        )
        .await
        .unwrap();
        // This fixture models the visible app body. Background sessions intentionally
        // ignore shared history, including phone-authored fully inline records.
        session.transfer_set_foreground(true).unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();

        let peer_dir = tempfile::tempdir().unwrap();
        let peer_boot = Boot {
            device_id: peer_identity.id(),
            nonce: [5; 16],
        };
        let peer = MobileNetwork::start(MobileNetworkConfig {
            identity_seed: peer_seed,
            boot: peer_boot,
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state),
            group_state_path: peer_dir.path().join("group-state.v2"),
            pairing_journal_path: peer_dir.path().join("pairing-recovery.cbor"),
            device_name: "peer".into(),
            os: "android".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: Default::default(),
        })
        .await
        .unwrap();
        let (database, attachments) = crate::transfer_store_paths(peer_dir.path());
        let peer_transfer = Arc::new(MobileV2Session::new(
            TransferStore::open(database, attachments).unwrap(),
            MobileV2Identity::from_parts(peer_identity.id().0, peer.group_id().0, peer_boot.nonce),
            peer.link.clone(),
        ));
        peer_transfer.set_history_foreground(true).unwrap();
        peer.link.attach_clipboard(&peer_transfer);

        // No link exists yet; the share waits instead of failing.
        session
            .transfer_send_share(
                vec![1; 16],
                vec![2; 16],
                vec![MobileShareItem::Text {
                    text: "early".into(),
                    html: None,
                    rtf: None,
                }],
                "transfer".into(),
                None,
                false,
                Some(MobileShareSensitivity::Concealed),
            )
            .await
            .unwrap();
        let port = session.advertisement().await.unwrap().port;
        peer.dial(phone_identity.id(), vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
            .unwrap();
        async fn wait(events: &mut tokio::sync::mpsc::UnboundedReceiver<MobileEvent>, found: fn(&MobileEvent) -> bool) {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let event = events.recv().await.expect("listener open");
                    if let MobileEvent::DeliveryEvent {
                        failure: Some(failure),
                        detail,
                        ..
                    } = &event
                    {
                        panic!("share failed: {failure:?} {detail:?}");
                    }
                    if found(&event) {
                        break;
                    }
                }
            })
            .await
            .expect("event pushed to the listener")
        }
        wait(&mut events, |event| {
            matches!(
                event,
                MobileEvent::DeliveryEvent {
                    delivery_state: MobileDeliveryState::Sent,
                    ..
                }
            )
        })
        .await;

        assert!(
            session
                .transfer_page(None, None, 20)
                .unwrap()
                .messages
                .iter()
                .any(|message| message.sensitive)
        );

        // The host reports a network path change; links stay up.
        discovery.network_changed(7);
        session.refresh_presence();
        peer_transfer.set_peers(vec![Peer {
            id: phone_identity.id().to_hex(),
            kind: PeerKind::Mobile,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        peer_transfer
            .send_share(
                SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                    items: vec![SendItem::Text {
                        text: "after the change".into(),
                    }],
                    origin: SendOrigin::TransferPage,
                    operation_id: [3; 16],
                    message_id: [4; 16],
                },
                oink_mobile_core::ProxyChoice::Automatic,
            )
            .unwrap();
        wait(&mut events, |event| {
            matches!(event, MobileEvent::TransferAnnouncement { content_id, .. } if content_id.starts_with("v2:"))
        })
        .await;
        assert!(session.network.links().ready(peer_identity.id()));
        session.clone().shutdown().await.unwrap();
        peer.stop().await;
    }

    /// The target list follows the other phone online and offline, the
    /// cache keeps the last online list for the share picker, and
    /// `transfer_confirm_online` answers from the dial: online while the
    /// phone runs, offline (and dropped from the cache) once it stopped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targets_follow_the_other_phone_and_the_cache_keeps_the_last_online_list() {
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let (phone_seed, peer_seed) = ([31; 32], [32; 32]);
        let phone_identity = Identity::from_seed(phone_seed);
        let peer_identity = Identity::from_seed(peer_seed);
        let base = State::create(&phone_identity, "phone", "android", 1).unwrap();
        let roster = base
            .roster()
            .add(&phone_identity, peer_identity.id(), "tablet", "ios", 2)
            .unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        let phone_dir = tempfile::tempdir().unwrap();
        let group_state = crate::device_only::group_state(phone_dir.path());
        std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
        std::fs::write(group_state, &state).unwrap();
        let data_dir = phone_dir.path().to_string_lossy().into_owned();
        let session = MobileSession::start(
            android_config(phone_dir.path(), SessionProfile::Mobile),
            MobileDiscovery::new(),
            Some(phone_seed.to_vec()),
        )
        .await
        .unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();
        async fn targets(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MobileEvent>,
            found: impl Fn(&TransferTargetsView) -> bool,
        ) -> TransferTargetsView {
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                loop {
                    if let MobileEvent::TransferTargets { targets } = events.recv().await.expect("listener open")
                        && found(&targets)
                    {
                        return targets;
                    }
                }
            })
            .await
            .expect("targets pushed to the listener")
        }
        let tablet = peer_identity.id().to_hex();
        // A group of two: the other device is the only one, offline.
        let alone = targets(&mut events, |view| !view.searching).await;
        assert!(alone.online.is_empty());
        assert_eq!(alone.other_devices, 1);
        assert_eq!(
            alone
                .only_device
                .as_ref()
                .map(|only| (only.device.as_str(), only.online)),
            Some((tablet.as_str(), false))
        );

        let peer_dir = tempfile::tempdir().unwrap();
        let peer = MobileNetwork::start(oink_mobile_core::MobileNetworkConfig {
            identity_seed: peer_seed,
            boot: Boot {
                device_id: peer_identity.id(),
                nonce: [6; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state),
            group_state_path: peer_dir.path().join("group-state.v2"),
            pairing_journal_path: peer_dir.path().join("pairing-recovery.cbor"),
            device_name: "tablet".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: Default::default(),
        })
        .await
        .unwrap();
        let port = session.advertisement().await.unwrap().port;
        peer.dial(phone_identity.id(), vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
            .unwrap();
        let online = targets(&mut events, |view| !view.online.is_empty()).await;
        assert_eq!(online.online.len(), 1);
        assert_eq!(online.online[0].device, tablet);
        assert_eq!(online.online[0].kind, TransferTargetKind::Mobile);
        assert_eq!(online.online[0].os, CardOs::Ios);
        assert!(online.only_device.unwrap().online);
        assert_eq!(cached_transfer_targets(data_dir.clone()), online.online);
        assert!(session.transfer_confirm_online(tablet.clone()).await.unwrap());

        // It goes away: the live list empties, the cache keeps it until a
        // confirmation finds it offline.
        peer.stop().await;
        let gone = targets(&mut events, |view| view.online.is_empty()).await;
        assert!(!gone.only_device.unwrap().online);
        assert_eq!(cached_transfer_targets(data_dir.clone()).len(), 1);
        assert!(!session.transfer_confirm_online(tablet.clone()).await.unwrap());
        assert!(cached_transfer_targets(data_dir.clone()).is_empty());
        // Not a member: offline at once.
        assert!(!session.transfer_confirm_online("ab".repeat(32)).await.unwrap());
        session.clone().shutdown().await.unwrap();
    }

    /// When a member was last seen online is written when it goes offline
    /// and read at the next start: the offline card shows it at once
    /// instead of "很久以前" until the member is seen again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn last_seen_of_an_offline_member_survives_a_restart() {
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let (phone_seed, peer_seed) = ([33; 32], [34; 32]);
        let phone_identity = Identity::from_seed(phone_seed);
        let peer_identity = Identity::from_seed(peer_seed);
        let base = State::create(&phone_identity, "phone", "android", 1).unwrap();
        let roster = base
            .roster()
            .add(&phone_identity, peer_identity.id(), "tablet", "ios", 2)
            .unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        let phone_dir = tempfile::tempdir().unwrap();
        let group_state = crate::device_only::group_state(phone_dir.path());
        std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
        std::fs::write(group_state, &state).unwrap();
        let start = || {
            MobileSession::start(
                android_config(phone_dir.path(), SessionProfile::Mobile),
                MobileDiscovery::new(),
                Some(phone_seed.to_vec()),
            )
        };
        let session = start().await.unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();
        let tablet = peer_identity.id().to_hex();
        async fn tablet_card(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MobileEvent>,
            tablet: &str,
            found: impl Fn(&DeviceCard) -> bool,
        ) -> DeviceCard {
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                loop {
                    if let MobileEvent::DeviceCards { snapshot } = events.recv().await.expect("listener open")
                        && let Some(card) = snapshot.cards.into_iter().find(|card| card.device == tablet)
                        && found(&card)
                    {
                        return card;
                    }
                }
            })
            .await
            .expect("device cards pushed to the listener")
        }
        // Never seen: no time yet.
        let never = tablet_card(&mut events, &tablet, |card| {
            matches!(card.section, CardSection::Offline)
        })
        .await;
        assert_eq!(never.last_seen_ms, None);

        let peer_dir = tempfile::tempdir().unwrap();
        let peer = MobileNetwork::start(oink_mobile_core::MobileNetworkConfig {
            identity_seed: peer_seed,
            boot: Boot {
                device_id: peer_identity.id(),
                nonce: [7; 16],
            },
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state),
            group_state_path: peer_dir.path().join("group-state.v2"),
            pairing_journal_path: peer_dir.path().join("pairing-recovery.cbor"),
            device_name: "tablet".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: Default::default(),
        })
        .await
        .unwrap();
        let dialed_at = presence::now_ms();
        let port = session.advertisement().await.unwrap().port;
        peer.dial(phone_identity.id(), vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
            .unwrap();
        let online = tablet_card(&mut events, &tablet, |card| matches!(card.section, CardSection::Online)).await;
        assert_eq!(online.last_seen_ms, None, "an online card shows no last seen time");

        peer.stop().await;
        let offline = tablet_card(&mut events, &tablet, |card| {
            matches!(card.section, CardSection::Offline)
        })
        .await;
        let stopped_at = presence::now_ms();
        let seen = offline.last_seen_ms.expect("last seen when it went offline");
        assert!((dialed_at..=stopped_at).contains(&seen));
        // Written when it went offline, not only at shutdown.
        let group = session.network.advertisement().group;
        assert_eq!(
            last_seen::LastSeen::load(phone_dir.path()).get(group, peer_identity.id()),
            Some(seen)
        );
        session.clone().shutdown().await.unwrap();

        let restarted = start().await.unwrap();
        restarted.refresh_presence();
        let card = restarted
            .cards_view()
            .cards
            .into_iter()
            .find(|card| card.device == tablet)
            .unwrap();
        assert!(matches!(card.section, CardSection::Offline));
        assert_eq!(card.last_seen_ms, Some(seen));
        restarted.clone().shutdown().await.unwrap();
    }

    #[test]
    fn share_session_never_provisions_a_group() {
        let dir = tempfile::tempdir().unwrap();
        let error =
            initialize_mobile_group(android_config(dir.path(), SessionProfile::ShareSession), None).unwrap_err();
        assert!(matches!(error, MobileError::Startup { .. }));
        assert!(!crate::device_only::group_state(dir.path()).exists());
    }

    fn ios_config(dir: &std::path::Path) -> MobileConfig {
        MobileConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            cache_dir: None,
            log_dir: None,
            device_name: "iPhone".into(),
            platform: MobilePlatform::Ios,
            profile: SessionProfile::Mobile,
            form_factor: None,
            app_version: None,
        }
    }

    /// M20 / D8: the App Group state came over from the old iPhone (backup
    /// or migration) but the Keychain identity did not. Provisioning replaces
    /// it with a group of this phone alone and says so; a usable or an
    /// unreadable state is left alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_state_restored_without_its_identity_is_replaced_by_a_new_group() {
        let dir = tempfile::tempdir().unwrap();
        let config = ios_config(dir.path());
        assert_eq!(
            initialize_mobile_group(config.clone(), Some(vec![7; 32])).unwrap(),
            MobileGroupProvision::Created
        );
        let new_phone = Some(vec![9; 32]);
        assert_eq!(
            initialize_mobile_group(config.clone(), new_phone.clone()).unwrap(),
            MobileGroupProvision::Replaced
        );
        let session = MobileSession::start(config.clone(), MobileDiscovery::new(), new_phone.clone())
            .await
            .unwrap();
        session.shutdown().await.unwrap();
        assert_eq!(
            initialize_mobile_group(config.clone(), new_phone.clone()).unwrap(),
            MobileGroupProvision::Existing
        );

        std::fs::write(crate::device_only::group_state(dir.path()), b"not cbor").unwrap();
        assert_eq!(
            initialize_mobile_group(config.clone(), new_phone.clone()).unwrap(),
            MobileGroupProvision::Existing
        );
        assert!(matches!(
            MobileSession::start(config, MobileDiscovery::new(), new_phone)
                .await
                .unwrap_err(),
            MobileError::Startup { .. }
        ));
    }

    /// A folder received phone to phone is browsed through the same calls as
    /// a PC folder, from its unpacked copy on this phone: listing pages level
    /// by level, `transfer_folder_children` with every file complete at its
    /// listing id, and opening a file without asking the sender (which is
    /// gone by then).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_folder_received_from_a_phone_is_browsed_and_opened_locally() {
        use oink_mobile_core::{Peer, PeerKind, PeerState, ProxyChoice, SendItem, SendOrigin, SharePayload};
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let (phone_seed, peer_seed) = ([41; 32], [42; 32]);
        let phone_identity = Identity::from_seed(phone_seed);
        let peer_identity = Identity::from_seed(peer_seed);
        let base = State::create(&phone_identity, "phone", "android", 1).unwrap();
        let roster = base
            .roster()
            .add(&phone_identity, peer_identity.id(), "tablet", "ios", 2)
            .unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        let phone_dir = tempfile::tempdir().unwrap();
        let group_state = crate::device_only::group_state(phone_dir.path());
        std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
        std::fs::write(group_state, &state).unwrap();
        let session = MobileSession::start(
            android_config(phone_dir.path(), SessionProfile::Mobile),
            MobileDiscovery::new(),
            Some(phone_seed.to_vec()),
        )
        .await
        .unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();
        async fn next<T>(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MobileEvent>,
            mut pick: impl FnMut(MobileEvent) -> Option<T>,
        ) -> T {
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                loop {
                    if let Some(found) = pick(events.recv().await.expect("listener open")) {
                        return found;
                    }
                }
            })
            .await
            .expect("event pushed to the listener")
        }

        // The tablet sends a folder to this phone.
        let peer_dir = tempfile::tempdir().unwrap();
        let peer_boot = Boot {
            device_id: peer_identity.id(),
            nonce: [8; 16],
        };
        let peer = MobileNetwork::start(oink_mobile_core::MobileNetworkConfig {
            identity_seed: peer_seed,
            boot: peer_boot,
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state),
            group_state_path: peer_dir.path().join("group-state.v2"),
            pairing_journal_path: peer_dir.path().join("pairing-recovery.cbor"),
            device_name: "tablet".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: Default::default(),
        })
        .await
        .unwrap();
        let (database, attachments) = crate::transfer_store_paths(peer_dir.path());
        let peer_transfer = Arc::new(MobileV2Session::new(
            TransferStore::open(database, attachments).unwrap(),
            MobileV2Identity::from_parts(peer_identity.id().0, peer.group_id().0, peer_boot.nonce),
            peer.link.clone(),
        ));
        peer.link.attach_clipboard(&peer_transfer);
        let port = session.advertisement().await.unwrap().port;
        peer.dial(phone_identity.id(), vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while peer
                .outbound_stream(phone_identity.id(), oink_protocol::frame::StreamKind::Clipboard)
                .is_none()
                || session
                    .network
                    .outbound_stream(peer_identity.id(), oink_protocol::frame::StreamKind::Clipboard)
                    .is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("clipboard streams open both ways");
        peer_transfer.set_peers(vec![Peer {
            id: phone_identity.id().to_hex(),
            kind: PeerKind::Mobile,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);
        let folder = peer_dir.path().join("Trip");
        std::fs::create_dir_all(folder.join("day 1/empty")).unwrap();
        std::fs::write(folder.join("notes.txt"), b"beach").unwrap();
        std::fs::write(folder.join("day 1/photo.png"), b"not really a png").unwrap();
        std::fs::write(folder.join("day 1/route.gpx"), vec![7u8; 4096]).unwrap();
        peer_transfer
            .send_share(
                SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                    items: vec![SendItem::Folder {
                        path: folder.to_string_lossy().into_owned(),
                        name: "Trip".into(),
                    }],
                    origin: SendOrigin::TransferPage,
                    operation_id: [4; 16],
                    message_id: [5; 16],
                },
                ProxyChoice::Direct {
                    device: phone_identity.id().to_hex(),
                    name: "phone".into(),
                },
            )
            .unwrap();
        let folder_id = next(&mut events, |event| match event {
            MobileEvent::TransferDirectEnded {
                record_ids,
                completed: true,
                ..
            } => Some(record_ids[0]),
            MobileEvent::TransferDirectEnded { failure, .. } => panic!("receive failed: {failure:?}"),
            _ => None,
        })
        .await;
        // The sender is gone: everything below is local.
        peer.stop().await;
        let message = session.transfer_message(folder_id).unwrap().unwrap();
        assert_eq!((message.kind.as_str(), message.item_count), ("Folder", Some(3)));

        let page = |operation: u8, node: u64| {
            session
                .transfer_list_directory(vec![operation; 16], message.content_id.clone(), node, None, None)
                .unwrap();
        };
        async fn listing(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MobileEvent>,
            operation: u8,
        ) -> (u64, Vec<MobileDirectoryEntry>) {
            next(events, |event| match event {
                MobileEvent::TransferDirectoryPage {
                    operation_id,
                    directory,
                    entries,
                    next_offset: None,
                    next_version: None,
                    ..
                } if operation_id == vec![operation; 16] => Some((directory, entries)),
                _ => None,
            })
            .await
        }
        page(1, 0);
        let (root, top) = listing(&mut events, 1).await;
        // The hosts file the root page under the node they asked for (0);
        // any other id leaves their folder view loading forever.
        assert_eq!(root, 0, "the root page reports the node it was asked for");
        let names: Vec<_> = top
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind.as_str(), entry.size))
            .collect();
        assert_eq!(
            names,
            vec![("day 1", "directory", None), ("notes.txt", "file", Some(5))]
        );
        assert!(
            top.iter()
                .all(|entry| entry.parent.is_some() && entry.parent == top[0].parent)
        );
        page(2, top[0].id);
        let (day, inner) = listing(&mut events, 2).await;
        assert_eq!(day, top[0].id);
        assert!(inner.iter().all(|entry| entry.parent == Some(day)));
        let names: Vec<_> = inner
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind.as_str(), entry.size))
            .collect();
        assert_eq!(
            names,
            vec![
                ("empty", "directory", None),
                ("photo.png", "file", Some(16)),
                ("route.gpx", "file", Some(4096)),
            ]
        );

        // Every file is a complete child at its listing id.
        let children = session.transfer_folder_children(folder_id).unwrap();
        let mut found: Vec<_> = children
            .iter()
            .map(|child| {
                (
                    child.directory_node.unwrap(),
                    child.display_name.as_str(),
                    child.download_state.as_str(),
                )
            })
            .collect();
        found.sort();
        let mut expected = vec![
            (top[1].id, "notes.txt", "Complete"),
            (inner[1].id, "photo.png", "Complete"),
            (inner[2].id, "route.gpx", "Complete"),
        ];
        expected.sort();
        assert_eq!(found, expected);
        let photo = children.iter().find(|child| child.display_name == "photo.png").unwrap();
        assert_eq!(photo.kind, "Image");
        assert_eq!(
            std::fs::read(photo.local_path.as_ref().unwrap()).unwrap(),
            b"not really a png"
        );

        // The listing rows carry those children: the folder page makes the
        // image thumbnails from the local files, the core asks nobody.
        let listed = inner[1].child.as_ref().expect("the photo row's child");
        assert_eq!(
            (listed.id, listed.local_path.as_ref()),
            (photo.id, photo.local_path.as_ref())
        );
        assert_eq!(listed.directory_node, Some(inner[1].id));
        assert_eq!(listed.download_status, Some(crate::MobileDownloadStatus::Downloaded));
        assert!(inner[0].child.is_none(), "a directory has no record");
        assert!(
            top[1]
                .child
                .as_ref()
                .and_then(|child| child.local_path.as_ref())
                .is_some()
        );
        assert_eq!(
            session
                .transfer_request_directory_thumbnail(folder_id, inner[1].clone())
                .unwrap(),
            None
        );

        // Opening a file needs no sender.
        let gpx = inner[2].clone();
        let child = session
            .transfer_download_directory_file(vec![3; 16], folder_id, gpx, true)
            .unwrap();
        next(&mut events, |event| match event {
            MobileEvent::TransferAnnouncement {
                record_id,
                auto_open_ready: true,
                ..
            } if record_id == child => Some(()),
            _ => None,
        })
        .await;
        let open = session.take_transfer_auto_open().unwrap().unwrap();
        assert_eq!(std::fs::read(open.path).unwrap(), vec![7u8; 4096]);
        session.clone().shutdown().await.unwrap();
    }

    /// An image sent phone to phone carries its original's displayed size
    /// (`ContentOffer` keys 15/16): the receiving phone's record has it from
    /// its first announcement on, so the placeholder already has the image's
    /// shape and nothing jumps when the file completes (spec 2026-11-11
    /// 「图片尺寸」).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_image_received_from_a_phone_has_the_senders_original_size() {
        use oink_mobile_core::{Peer, PeerKind, PeerState, ProxyChoice, SendItem, SendOrigin, SharePayload};
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let (phone_seed, peer_seed) = ([43; 32], [44; 32]);
        let phone_identity = Identity::from_seed(phone_seed);
        let peer_identity = Identity::from_seed(peer_seed);
        let base = State::create(&phone_identity, "phone", "android", 1).unwrap();
        let roster = base
            .roster()
            .add(&phone_identity, peer_identity.id(), "tablet", "ios", 2)
            .unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        let phone_dir = tempfile::tempdir().unwrap();
        let group_state = crate::device_only::group_state(phone_dir.path());
        std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
        std::fs::write(group_state, &state).unwrap();
        let session = MobileSession::start(
            android_config(phone_dir.path(), SessionProfile::Mobile),
            MobileDiscovery::new(),
            Some(phone_seed.to_vec()),
        )
        .await
        .unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();

        // The tablet, an in-process phone of the same group.
        let peer_dir = tempfile::tempdir().unwrap();
        let peer_boot = Boot {
            device_id: peer_identity.id(),
            nonce: [9; 16],
        };
        let peer = MobileNetwork::start(oink_mobile_core::MobileNetworkConfig {
            identity_seed: peer_seed,
            boot: peer_boot,
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state),
            group_state_path: peer_dir.path().join("group-state.v2"),
            pairing_journal_path: peer_dir.path().join("pairing-recovery.cbor"),
            device_name: "tablet".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: Default::default(),
        })
        .await
        .unwrap();
        let (database, attachments) = crate::transfer_store_paths(peer_dir.path());
        let peer_transfer = Arc::new(MobileV2Session::new(
            TransferStore::open(database, attachments).unwrap(),
            MobileV2Identity::from_parts(peer_identity.id().0, peer.group_id().0, peer_boot.nonce),
            peer.link.clone(),
        ));
        peer.link.attach_clipboard(&peer_transfer);
        let port = session.advertisement().await.unwrap().port;
        peer.dial(phone_identity.id(), vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while peer
                .outbound_stream(phone_identity.id(), oink_protocol::frame::StreamKind::Clipboard)
                .is_none()
                || session
                    .network
                    .outbound_stream(peer_identity.id(), oink_protocol::frame::StreamKind::Clipboard)
                    .is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("clipboard streams open both ways");
        peer_transfer.set_peers(vec![Peer {
            id: phone_identity.id().to_hex(),
            kind: PeerKind::Mobile,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        }]);

        // A portrait photo: its displayed size as the sending host read it.
        let photo = peer_dir.path().join("IMG_0002.HEIC");
        std::fs::write(&photo, vec![5u8; 8192]).unwrap();
        let sent = peer_transfer
            .send_share(
                SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                    items: vec![SendItem::Image {
                        path: photo.to_string_lossy().into_owned(),
                        encoded_size: 0,
                        mime: "image/heic".into(),
                        staged: false,
                        dimensions: Some((3024, 4032)),
                    }],
                    origin: SendOrigin::TransferPage,
                    operation_id: [6; 16],
                    message_id: [7; 16],
                },
                ProxyChoice::Direct {
                    device: phone_identity.id().to_hex(),
                    name: "phone".into(),
                },
            )
            .unwrap();
        let own = peer_transfer.with_store(|store| store.by_id(sent[0])).unwrap().unwrap();
        assert_eq!(own.image_size(), Some((3024, 4032)));

        // The first record the receiver announces already has the size.
        let first = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                if let MobileEvent::TransferDirectIncoming { record_ids, .. } =
                    events.recv().await.expect("listener open")
                {
                    return record_ids[0];
                }
            }
        })
        .await
        .expect("the receive starts");
        let arriving = session.transfer_message(first).unwrap().unwrap();
        assert_eq!(arriving.kind, "Image");
        assert_eq!((arriving.image_width, arriving.image_height), (Some(3024), Some(4032)));
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match events.recv().await.expect("listener open") {
                    MobileEvent::TransferDirectEnded { completed: true, .. } => return,
                    MobileEvent::TransferDirectEnded { failure, .. } => panic!("receive failed: {failure:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("the receive ends");
        let received = session.transfer_message(first).unwrap().unwrap();
        assert_eq!(received.download_status, Some(crate::MobileDownloadStatus::Downloaded));
        assert_eq!(
            (received.image_width, received.image_height),
            (Some(3024), Some(4032)),
            "the shape does not change when the file completes"
        );
        assert_eq!(std::fs::read(received.local_path.unwrap()).unwrap(), vec![5u8; 8192]);
        peer.stop().await;
        session.clone().shutdown().await.unwrap();
    }

    /// User 2026-11-11: sharing to the group's only other device, a phone
    /// that is offline. The share waits unrecorded (`TransferShareWaiting`
    /// after the first connect round, no history record), starts by itself
    /// when that phone comes online (`waiting: false`, then its own record),
    /// and is delivered. A second share while the phone is offline again is
    /// cancelled while it waits: the call fails as Cancelled and leaves no
    /// record.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_share_to_the_offline_only_device_waits_and_starts_when_it_comes_online() {
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let (phone_seed, peer_seed) = ([51; 32], [52; 32]);
        let phone_identity = Identity::from_seed(phone_seed);
        let peer_identity = Identity::from_seed(peer_seed);
        let base = State::create(&phone_identity, "phone", "android", 1).unwrap();
        let roster = base
            .roster()
            .add(&phone_identity, peer_identity.id(), "iPhone", "ios", 2)
            .unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        let phone_dir = tempfile::tempdir().unwrap();
        let group_state = crate::device_only::group_state(phone_dir.path());
        std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
        std::fs::write(group_state, &state).unwrap();
        let session = MobileSession::start(
            android_config(phone_dir.path(), SessionProfile::Mobile),
            MobileDiscovery::new(),
            Some(phone_seed.to_vec()),
        )
        .await
        .unwrap();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        session.subscribe(Box::new(Events(sender))).await.unwrap();
        async fn next<T>(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MobileEvent>,
            mut pick: impl FnMut(MobileEvent) -> Option<T>,
        ) -> T {
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                loop {
                    if let Some(found) = pick(events.recv().await.expect("listener open")) {
                        return found;
                    }
                }
            })
            .await
            .expect("event pushed to the listener")
        }
        let iphone = peer_identity.id().to_hex();
        let file = phone_dir.path().join("photo.jpg");
        std::fs::write(&file, vec![9u8; 2048]).unwrap();
        let share = |operation: u8| {
            let session = session.clone();
            let (path, target) = (file.to_string_lossy().into_owned(), iphone.clone());
            tokio::spawn(async move {
                session
                    .transfer_send_share(
                        vec![operation; 16],
                        vec![operation; 16],
                        vec![MobileShareItem::File {
                            path,
                            name: "photo.jpg".into(),
                            size: 2048,
                            mime: Some("image/jpeg".into()),
                            staged: false,
                        }],
                        "android-share".into(),
                        Some(target),
                        true,
                        None,
                    )
                    .await
            })
        };
        let waiting = |operation: u8, waiting: bool| {
            move |event: MobileEvent| match event {
                MobileEvent::TransferAnnouncement { .. } => panic!("recorded while it waits for its target"),
                MobileEvent::TransferShareWaiting {
                    operation_id,
                    waiting: now,
                } if operation_id == vec![operation; 16] && now == waiting => Some(()),
                _ => None,
            }
        };

        // The iPhone is offline: the share waits, nothing is recorded.
        let first = share(1);
        next(&mut events, waiting(1, true)).await;
        assert!(session.transfer_page(None, None, 10).unwrap().messages.is_empty());
        assert!(!first.is_finished());

        // The iPhone comes online: the share starts by itself.
        let peer_dir = tempfile::tempdir().unwrap();
        let peer_boot = Boot {
            device_id: peer_identity.id(),
            nonce: [9; 16],
        };
        let peer = MobileNetwork::start(oink_mobile_core::MobileNetworkConfig {
            identity_seed: peer_seed,
            boot: peer_boot,
            bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            group_state: Some(state.clone()),
            group_state_path: peer_dir.path().join("group-state.v2"),
            pairing_journal_path: peer_dir.path().join("pairing-recovery.cbor"),
            device_name: "iPhone".into(),
            os: "ios".into(),
            form_factor: None,
            trusted_peers: Vec::new(),
            trust_path: None,
            process: Default::default(),
        })
        .await
        .unwrap();
        let (database, attachments) = crate::transfer_store_paths(peer_dir.path());
        let peer_transfer = Arc::new(MobileV2Session::new(
            TransferStore::open(database, attachments).unwrap(),
            MobileV2Identity::from_parts(peer_identity.id().0, peer.group_id().0, peer_boot.nonce),
            peer.link.clone(),
        ));
        peer.link.attach_clipboard(&peer_transfer);
        let port = session.advertisement().await.unwrap().port;
        peer.dial(phone_identity.id(), vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
            .unwrap();
        next(&mut events, waiting(1, false)).await;
        tokio::time::timeout(std::time::Duration::from_secs(20), first)
            .await
            .expect("the share started")
            .unwrap()
            .expect("the share was recorded");
        let record = next(&mut events, |event| match event {
            MobileEvent::TransferAnnouncement { record_id, .. } => Some(record_id),
            _ => None,
        })
        .await;
        let own = session.transfer_message(record).unwrap().unwrap();
        assert_eq!(own.display_name, "photo.jpg");
        next(&mut events, |event| match event {
            MobileEvent::DeliveryEvent {
                operation_id,
                delivery_state,
                failure,
                ..
            } if operation_id == vec![1; 16] => {
                assert_eq!((delivery_state, failure), (MobileDeliveryState::Sent, None));
                Some(())
            }
            _ => None,
        })
        .await;

        // Offline again: 取消 while waiting leaves nothing.
        peer.stop().await;
        next(&mut events, |event| match event {
            MobileEvent::TransferTargets { targets } if targets.online.is_empty() => Some(()),
            _ => None,
        })
        .await;
        let second = share(2);
        next(&mut events, waiting(2, true)).await;
        session.transfer_cancel_share(vec![2; 16]).unwrap();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(20), second)
            .await
            .expect("the waiting share ended")
            .unwrap();
        assert!(matches!(
            cancelled,
            Err(MobileError::Transfer {
                reason: TransferFailure::Cancelled,
                ..
            })
        ));
        let page = session.transfer_page(None, None, 10).unwrap();
        assert_eq!(page.messages.len(), 1, "only the first share is in the history");
        session.clone().shutdown().await.unwrap();
    }

    /// Two phones in one group: what one phone turns on for its speaker and
    /// microphone shows on the other phone's card for it, the same state the
    /// phone shows on its own card. The microphone has no receiver here, so it
    /// waits for a demand (Starting) and never captures.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_phone_sees_the_other_phones_speaker_and_microphone_state() {
        use crate::{MobileMediaKind, MobileMicrophonePermission};
        use oink_network::{group_state::State, identity::Identity};
        use oink_protocol::v2::schema::WireDto;
        let (android_seed, iphone_seed) = ([51; 32], [52; 32]);
        let android_identity = Identity::from_seed(android_seed);
        let iphone_identity = Identity::from_seed(iphone_seed);
        let base = State::create(&android_identity, "an Android test device", "android", 1).unwrap();
        let roster = base
            .roster()
            .add(&android_identity, iphone_identity.id(), "iPhone", "ios", 2)
            .unwrap();
        let state = base
            .with_roster(&roster)
            .unwrap()
            .wire()
            .encode(oink_protocol::v2::limits::CONFIG_STATE)
            .unwrap();
        async fn start(
            dir: &std::path::Path,
            config: MobileConfig,
            seed: [u8; 32],
            state: &[u8],
        ) -> Arc<MobileSession> {
            let group_state = crate::device_only::group_state(dir);
            std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
            std::fs::write(group_state, state).unwrap();
            MobileSession::start(config, MobileDiscovery::new(), Some(seed.to_vec()))
                .await
                .unwrap()
        }
        let (android_dir, iphone_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let android = start(
            android_dir.path(),
            android_config(android_dir.path(), SessionProfile::Mobile),
            android_seed,
            &state,
        )
        .await;
        let iphone = start(iphone_dir.path(), ios_config(iphone_dir.path()), iphone_seed, &state).await;
        let both = MobilePlatformFacts {
            microphone_send: true,
            speaker_receive: true,
            speaker_send: true,
            privileged_input_ready: false,
        };
        android.update_platform_facts(both);
        iphone.update_platform_facts(both);
        // Both phones already reconnect each other on their own (no address
        // yet); the iPhone's manual member address for the Xiaomi lets that
        // reconnect find it.
        let port = android.advertisement().await.unwrap().port;
        let android_id = android_identity.id().to_hex();
        iphone
            .edit_member_address(&android_id, &format!("127.0.0.1:{port}"))
            .unwrap();

        let audio = |session: &MobileSession, local: bool| {
            session.refresh_presence();
            session
                .cards_view()
                .cards
                .into_iter()
                .find(|card| card.device == android_id && card.is_self == local)
                .and_then(|card| card.mobile_audio)
        };
        async fn until(
            what: &str,
            mut read: impl FnMut() -> Option<MobileAudioCard>,
            found: impl Fn(&MobileAudioCard) -> bool,
        ) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let card = read();
                if card.as_ref().is_some_and(&found) {
                    return;
                }
                assert!(tokio::time::Instant::now() < deadline, "{what}: last card {card:?}");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        until(
            "the iPhone sees the Xiaomi online with both endpoints off",
            || audio(&iphone, false),
            |card| card.speaker == MobileEndpointState::Local && card.microphone == MobileEndpointState::Local,
        )
        .await;

        // Control: the speaker becomes the group exit and the native output
        // starts (what the platform audio owner does on a real phone).
        let media = android.media();
        media.set_enabled(MobileMediaKind::Speaker, true);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let generation = loop {
            let snapshot = media.snapshot();
            if snapshot.start_mask & MobileMediaKind::Speaker.bit() != 0 {
                break snapshot.generation;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "speaker never startable: {snapshot:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(media.claim_start(MobileMediaKind::Speaker, generation));
        // SAFETY: constructor has no pointer preconditions.
        let output = unsafe { crate::audio::oink_audio_output_create(48_000, 2, generation) };
        assert!(!output.is_null());
        assert!(media.attach_audio(MobileMediaKind::Speaker, generation, output as u64));
        assert!(media.acknowledge_started(MobileMediaKind::Speaker, generation));
        assert_eq!(audio(&android, true).unwrap().speaker, MobileEndpointState::Active);
        until(
            "the iPhone sees the Xiaomi speaker as the exit",
            || audio(&iphone, false),
            |card| card.speaker == MobileEndpointState::Active,
        )
        .await;

        // The bug: the microphone turned on for sending, waiting for a
        // receiver's demand, stayed "local" on the other phone.
        media.set_microphone_permission(MobileMicrophonePermission::Granted);
        media.set_enabled(MobileMediaKind::Microphone, true);
        assert_eq!(audio(&android, true).unwrap().microphone, MobileEndpointState::Starting);
        until(
            "the iPhone sees the Xiaomi microphone sending",
            || audio(&iphone, false),
            |card| card.microphone == MobileEndpointState::Starting,
        )
        .await;
        // The source is the global audio configuration both phones hold, not
        // the Xiaomi's capture flag (it captures only on a receiver's demand).
        assert_eq!(
            iphone
                .media()
                .with_audio(|audio| audio.holder(oink_audio::route::Role::Microphone)),
            Some(android_identity.id())
        );

        // Turned off again: back to local on both phones.
        media.set_enabled(MobileMediaKind::Microphone, false);
        assert_eq!(audio(&android, true).unwrap().microphone, MobileEndpointState::Local);
        until(
            "the iPhone sees the Xiaomi microphone local again",
            || audio(&iphone, false),
            |card| card.microphone == MobileEndpointState::Local,
        )
        .await;

        // Source capture on the other phone negotiates the same Source/Receiver
        // session as a PC and traverses real QUIC/M1 before native playout.
        let source_media = iphone.media();
        source_media.set_enabled(MobileMediaKind::Forward, true);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let source_generation = loop {
            let snapshot = source_media.snapshot();
            if snapshot.start_mask & 4 != 0 {
                break snapshot.generation;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "forward negotiation never became startable: {snapshot:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(source_media.claim_start(MobileMediaKind::Forward, source_generation));
        let capture = crate::forward::oink_audio_forward_create(48_000, 2, source_generation);
        assert!(!capture.is_null());
        assert!(source_media.attach_audio(MobileMediaKind::Forward, source_generation, capture as u64));
        assert!(source_media.acknowledge_started(MobileMediaKind::Forward, source_generation));
        let mut audible = false;
        for block in 0..80u64 {
            let signal: Vec<i16> = (0..240)
                .flat_map(|frame| {
                    let value = (8000.0
                        * (((block * 240 + frame) as f32) * 440.0 * std::f32::consts::TAU / 48_000.0).sin())
                        as i16;
                    [value, value]
                })
                .collect();
            let mut rendered = [0.0f32; 480];
            // SAFETY: bounded stereo buffers and live handles, one simulated callback.
            unsafe {
                assert_eq!(
                    crate::forward::oink_audio_forward_push_i16(
                        capture,
                        signal.as_ptr(),
                        240,
                        1_000_000 + block * 5_000_000,
                        0
                    ),
                    240
                );
                crate::audio::oink_audio_output_pull_at(
                    output,
                    rendered.as_mut_ptr(),
                    240,
                    1_000_000 + block * 5_000_000,
                );
            }
            audible |= rendered.iter().any(|sample| sample.abs() > 0.01);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(audible, "platform PCM must reach the existing native exit decoder");
        source_media.set_enabled(MobileMediaKind::Forward, false);
        // SAFETY: the simulated producer has ended; no callback can access capture.
        unsafe {
            crate::forward::oink_audio_forward_destroy(capture);
        }
        source_media.acknowledge_stopped(MobileMediaKind::Forward, source_generation);
        iphone.clone().shutdown().await.unwrap();
        android.clone().shutdown().await.unwrap();
        // SAFETY: the unique handle created above; both sessions are shut down.
        unsafe { crate::audio::oink_audio_output_destroy(output) };
    }
}
