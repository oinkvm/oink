use super::facts::validate_generation;
use super::input_values::validate_input_contacts;
use super::*;
use crate::v2::hot::hot_map;
use crate::v2::schema::{defaulted, no_validation, open_enum, required, wire_map};
use crate::v2::{Error, ErrorCode, RawMap, RawValue, Result};

wire_map!(ControlFeel,no_validation,{1=>profile:FeelProfile=required;});

wire_map!(ControlFocusEnter, no_validation, {
    1 => epoch: u64 = required;
    2 => display: Text<512> = required;
    3 => pos: Point = required;
    4 => scope: FocusScope = required;
    5 => pointer_source: Option<DeviceId> = optional;
});

wire_map!(ControlFocusState, no_validation, {
    1 => epoch: u64 = required;
    2 => holder: DeviceId = required;
    3 => scope: FocusScope = required;
    4 => session: Id16 = required;
    5 => lock_state: Option<FocusLockRecord> = optional;
});

wire_map!(ControlSetFocusLock, no_validation, {
    1 => epoch: u64 = required;
    2 => locked: Option<bool> = optional;
    3 => target: Option<DeviceId> = optional;
});

wire_map!(ControlFocusLock,no_validation,{1=>state:FocusLockRecord=required;});

wire_map!(ControlGameMode, no_validation, {
    1 => epoch: u64 = required;
    2 => generation: u64 = required;
    3 => mode: GameMode = required;
    4 => detected: bool = required;
    5 => active: bool = required;
    6 => cursor_hiding_cause_available: Option<bool> = optional;
});

wire_map!(ControlSetGameMode, no_validation, {
    1 => epoch: u64 = required;
    2 => mode: GameMode = required;
});

wire_map!(ControlFocusResume, no_validation, {
    1 => epoch: u64 = required;
    2 => holder: DeviceId = required;
    3 => scope: FocusScope = required;
});

wire_map!(ControlCursorTrail, no_validation, {
    1 => active: bool = required;
});

wire_map!(ControlUserActivity, no_validation, {});

wire_map!(ControlCursorTrailHandoff, no_validation, {
    1 => epoch: u64 = required;
    2 => sources: List<DeviceId,16> = required;
    3 => state: TrailContinuation = required;
});

wire_map!(InputCapability, validate_input_capability, {
    1 => controllable: bool = required;
    2 => max_contacts: u8 = 0;
});

hot_map!(InputMotion, no_validation, {
    1 => seq: u64 = required;
    2 => dx: f64 = required;
    3 => dy: f64 = required;
    4 => unit: MotionUnit = (MotionUnit::Canvas);
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

// Without physical_code, code names a physical key and the target applies its
// transform. With physical_code, code is already resolved by the source: track
// held state under physical_code, inject code without another transform, and
// do not start target repetition. Caps tap/hold uses this to keep Caps->Shift
// distinct from a concurrently held physical Shift from the same source.
hot_map!(InputKey, no_validation, {
    1 => code: u32 = required;
    2 => pressed: bool = required;
    3 => physical_code: Option<u32> = optional;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputButton, no_validation, {
    1 => button: MouseButton = required;
    2 => pressed: bool = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputScroll, no_validation, {
    1 => source: SourceKind = required;
    2 => dx: f64 = required;
    3 => dy: f64 = required;
    4 => unit: ScrollAmount = required;
    5 => phase: ScrollPhase = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
    103 => wheel_steps: Option<Point> = optional;
});

hot_map!(InputHeldSync, no_validation, {
    1 => keys: List<u32,256> = required;
    2 => buttons: List<MouseButton,32> = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputKeyRepeat, no_validation, {
    1 => code: u32 = required;
    2 => n: u32 = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputGesture, no_validation, {
    1 => source: SourceKind = required;
    2 => gesture: Gesture = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputRelativeMotion, no_validation, {
    1 => epoch: u64 = required;
    2 => generation: u64 = required;
    3 => seq: u64 = required;
    4 => x: f64 = required;
    5 => y: f64 = required;
    6 => unit: MotionUnit = (MotionUnit::Canvas);
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputVolume, validate_volume, {
    1 => code: u32 = required;
    2 => pressed: bool = required;
    3 => repeat: bool = required;
    4 => modifiers: u8 = required;
    5 => target_boot: Boot = required;
    6 => session: AudioSessionContext = required;
    7 => press_id: u64 = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputTouchpadContacts, validate_input_contacts, {
    1 => device: u32 = required;
    2 => time_us: u64 = required;
    3 => phase: ContactPhase = required;
    4 => surface: ContactSurface = required;
    6 => changes: List<ContactChange,10> = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

hot_map!(InputVolumeKeyOrigin, no_validation, {
    1 => action: VolumeKey = required;
    2 => provenance: VolumeKeyOrigin = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
    102 => capture_time_us: Option<u32> = optional;
});

// Committed IME text (`0x020d`, C0). The sender splits longer input on
// character boundaries; each message is injected as one Unicode insertion.
// Backspace, Return, Tab and modifier chords remain `InputKey`; therefore
// control characters are rejected here. Debug output never shows the text.
wire_map!(InputText, validate_input_text, redacted, {
    1 => seq: u64 = required;
    2 => text: Text<4096> = required;
    100 => origin: Option<Boot> = optional;
    101 => forwarded_hops: u8 = 0;
});

/// UTF-8 byte ceiling of one `InputText.text` (the `Text<4096>` bound above).
pub const INPUT_TEXT_MAX_BYTES: usize = 4096;

fn validate_input_text(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 1)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let text: Text<INPUT_TEXT_MAX_BYTES> = required(raw, 2)?;
    if text.0.is_empty() || text.0.chars().any(char::is_control) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

wire_map!(ClipboardOffer, validate_clipboard_offer, {
    1 => id: OfferId = required;
    2 => source_change_count: u64 = required;
    3 => clock: u64 = required;
    4 => sensitivity: Sensitivity = required;
    6 => inventory: Inventory = required;
    7 => time_ms: u64 = required;
    8 => real_sender: DeviceId = required;
    9 => proxy: Option<DeviceId> = optional;
    10 => operation_id: Id16 = required;
    11 => message_id: Id16 = required;
    12 => item_count: u32 = required;
    13 => digests: List<ContentDigest, 64> = required;
});

wire_map!(ContentBeginDelivery, no_validation, {
    1 => delivery: DeliveryId = required;
    2 => demand: Demand = required;
});

wire_map!(ContentDeliveryReady, no_validation, {
    1 => offer: OfferId = required;
    2 => delivery: DeliveryId = required;
});

wire_map!(ContentReadRepresentation, no_validation, {
    1 => offer: OfferId = required;
    2 => representation: u32 = required;
    3 => target: Ascii<32> = required;
    4 => demand: Demand = required;
    5 => item: u32 = required;
    6 => accepted_format: FormatId = required;
});

wire_map!(ContentRepresentationBytes, no_validation, {
    1 => offer: OfferId = required;
    2 => representation: u32 = required;
    3 => offset: u64 = required;
    4 => bytes: Bytes<1048576> = required;
});

wire_map!(ContentRepresentationEnd, no_validation, {
    1 => offer: OfferId = required;
    2 => representation: u32 = required;
    3 => size: u64 = required;
});

wire_map!(ContentRepresentationFailed, no_validation, {
    1 => offer: OfferId = required;
    2 => representation: u32 = required;
    3 => reason: ContentUnavailable = required;
});

wire_map!(ContentListDirectory, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => cursor: Option<DirectoryCursor> = optional;
    4 => demand: Demand = required;
});

wire_map!(ContentDirectory, no_validation, {
    1 => offer: OfferId = required;
    2 => page: DirectoryPage = required;
});

wire_map!(ContentPreparing, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
});

wire_map!(ContentDirectoryFailed, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => reason: ContentUnavailable = required;
});

wire_map!(ContentSelectFile, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => demand: Demand = required;
});

wire_map!(ContentFileSelected, no_validation, {
    1 => offer: OfferId = required;
    2 => entry: FileNode = required;
});

wire_map!(ContentReadFile, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => version: u64 = required;
    4 => offset: u64 = required;
    6 => demand: Demand = required;
});

wire_map!(ContentFileBlock, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => version: u64 = required;
    4 => offset: u64 = required;
    6 => bytes: Bytes<1048576> = required;
});

wire_map!(ContentFileEnd, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => version: u64 = required;
    4 => size: u64 = required;
});

wire_map!(ContentFileFailed, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => reason: ContentUnavailable = required;
});

wire_map!(ContentTargetStored, no_validation, {
    1 => delivery: DeliveryId = required;
});

wire_map!(ContentFailed, no_validation, {
    1 => delivery: DeliveryId = required;
    2 => reason: ContentUnavailable = required;
});

wire_map!(ContentTerminalAck, no_validation, {
    1 => delivery: DeliveryId = required;
});

wire_map!(DndNotice, super::drag::validate_dnd_notice, {
    1 => key: DragKey = required;
    2 => revision: u64 = required;
    3 => input: DeviceId = required;
    4 => state: DragState = required;
    5 => metadata: MetadataVersion = required;
    6 => preview: PreviewAvailability = required;
    7 => inventory: Inventory = required;
});

wire_map!(DndNoticeRequest, no_validation, {
    1 => id: ClipId = required;
    2 => holder: Holder = required;
    3 => previous: DeviceId = required;
});

wire_map!(DndRefused, no_validation, {
    1 => id: ClipId = required;
    2 => holder: Holder = required;
    3 => reason: RevokeReason = required;
});

wire_map!(DndSourceButton, no_validation, {
    1 => key: DragKey = required;
    2 => revision: u64 = required;
    3 => disposition: SourceButtonDisposition = required;
});

wire_map!(DndSource, no_validation, {
    1 => offer: OfferId = required;
    2 => holder: Holder = required;
    3 => action: SourceAction = required;
    4 => cancel_at: Option<Point> = optional;
});

wire_map!(DndCaps, no_validation, {
    1 => enabled: bool = required;
});

wire_map!(DndEnter, no_validation, {
    1 => id: ClipId = required;
    2 => epoch: u64 = required;
});

wire_map!(DndPreview, no_validation, {
    1 => id: ClipId = required;
    2 => epoch: u64 = required;
    3 => image: Option<DragImage> = optional;
    4 => key: DragKey = required;
    5 => metadata_revision: u64 = required;
    6 => preview_revision: u64 = required;
    7 => format: FormatId = required;
});

wire_map!(DndPreviewRequest, no_validation, {
    1 => id: ClipId = required;
    2 => epoch: u64 = required;
    3 => key: DragKey = required;
    4 => metadata_revision: u64 = required;
    5 => preview_revision: u64 = required;
});

// Reliable source intent complements M1: a lost wake packet cannot leave an
// exit believing the previous burst is asleep forever. Identity is the admitted
// per-kind stream, never the link alone; owners ignore retired generations.
open_enum!(AudioMediaKind { Speaker=0, Microphone=1 });
wire_map!(AudioMediaState, no_validation, {
    1 => kind: AudioMediaKind = required;
    2 => stream_id: u64 = required;
    3 => burst: u64 = required;
    4 => sleeping: bool = required;
});

wire_map!(AudioState, validate_audio_state, {
    3 => permission: CapturePermission = required;
    4 => source_muted: bool = required;
    5 => forwarding_enabled: bool = required;
    6 => forwarding_off_reason: Option<ForwardingOffReason> = optional;
    7 => forwarding_muted: bool = required;
    8 => forwarding_volume_percent: u16 = required;
    9 => mute_release_unconfirmed: bool = required;
    10 => sent: u64 = required;
    11 => send_dropped: u64 = required;
    12 => outputs: List<OutputLatency,32> = required;
    13 => receivers: List<ReceiverReport,16> = required;
    14 => phase: AudioPhase = required;
    15 => problem: Option<UserFacingProblem> = optional;
});

// Backend-reported latency excludes transport and application queues. Absence
// means unknown, never an inferred zero-latency device.
wire_map!(OutputLatency,validate_output_latency,{
    1=>output:Text<512>=required;
    2=>reported_latency_ms:Option<f64>=optional;
});
wire_map!(ReceiverReport,validate_receiver_report,{
    1=>source:DeviceId=required;2=>received:u64=required;3=>concealed:u64=required;
    4=>late:u64=required;5=>buffered_ms:f64=required;
});

fn validate_audio_state(raw: &RawMap) -> Result<()> {
    // Per-source forwarding gain may amplify to 200%; receiver exit volume
    // uses its separate 0..100 control and must not share this bound.
    if required::<u16>(raw, 8)? > 200 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
fn validate_output_latency(raw: &RawMap) -> Result<()> {
    if defaulted::<Option<f64>>(raw, 2, || None)?.is_some_and(|value| value < 0.0) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
fn validate_receiver_report(raw: &RawMap) -> Result<()> {
    if required::<f64>(raw, 5)? < 0.0 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

wire_map!(AudioPrepared, no_validation, {
    1 => session: AudioSessionContext = required;
    2 => stream_id: u64 = required;
    3 => descriptor: AudioDescriptor = required;
});

hot_map!(AudioPacket, validate_packet, {
    1 => stream_id: u64 = required;
    2 => sequence: u64 = required;
    3 => burst: u64 = required;
    4 => source_time_ns: u64 = required;
    5 => silent: bool = required;
    6 => payload: Bytes<960> = required;
});

wire_map!(AudioRouteFault, no_validation, {
    1 => session: AudioSessionContext = required;
    3 => problem: UserFacingProblem = required;
});

hot_map!(AudioSleep, no_validation, {
    1 => stream_id: u64 = required;
    2 => burst: u64 = required;
    3 => sequence: u64 = required;
});

wire_map!(AudioPreparedAck, no_validation, {
    1 => stream_id: u64 = required;
    2 => result: TypedResult = required;
});

wire_map!(SourceRouteOffer, validate_generation, {
    1 => source: Boot = required;
    2 => source_route_generation: u64 = required;
    3 => receiver: Boot = required;
    4 => route_version: AudioRouteVersion = required;
    5 => preferred_descriptor: Option<AudioDescriptor> = optional;
});
wire_map!(AudioPrepareRefused, no_validation, {
    1 => offer: SourceRouteOffer = required;
    2 => reason: ControlFailure = required;
    3 => current_output: Option<ReceiverOutputBinding> = optional;
});

wire_map!(WallpaperSet, no_validation, {
    1 => revision: u64 = required;
    2 => screens: List<ScreenWallpaper,16> = required;
    3 => format: FormatId = required;
});

wire_map!(DiscoveryQuery, no_validation, {
    1 => nonce: Id16 = required;
});

wire_map!(DiscoveryReply, no_validation, {
    1 => advert: Advert = required;
    2 => nonce: Id16 = required;
});

wire_map!(DiagnosticsHello, no_validation, {
    1 => instance: Text<128> = required;
    2 => enabled: bool = required;
    3 => reply: bool = required;
    4 => previous_unclean: bool = required;
    // Mobile diagnostics are ephemeral on the phone and are identified by
    // source/session so multiple proxy stores can merge them at export.
    5 => source_device: Option<DeviceId> = optional;
    6 => proxy_device: Option<DeviceId> = optional;
    7 => session_id: Option<Id16> = optional;
    8 => mobile: bool = false;
    9 => aggregate_window_ms: Option<u32> = optional;
});

wire_map!(DiagnosticsRequest, no_validation, {
    1 => t1: ClockStamp = required;
    2 => start_us: i64 = required;
    3 => end_us: i64 = required;
    4 => request_id: Id16 = required;
    5 => accepted_formats: List<Ascii<96>,16> = required;
    6 => accepted_compressions: List<Ascii<96>,16> = (List(vec![Ascii("none".into())]));
    7 => mobile_sources: Option<List<DeviceId,64>> = optional;
    8 => export_id: Option<Id16> = optional;
    9 => include_mobile: bool = false;
});

wire_map!(DiagnosticsResponse, no_validation, {
    1 => instance: Text<128> = required;
    2 => evicted_segments: u64 = required;
    3 => t2: ClockStamp = required;
    4 => t3: ClockStamp = required;
    5 => request_id: Id16 = required;
    6 => format: Ascii<96> = required;
    7 => compression: Ascii<96> = required;
    8 => source_device: Option<DeviceId> = optional;
    9 => proxy_device: Option<DeviceId> = optional;
    10 => session_id: Option<Id16> = optional;
    11 => store_bytes: Option<u64> = optional;
    12 => partial: bool = false;
});

wire_map!(DiagnosticsChunk, no_validation, {
    1 => bytes: Bytes<16384> = required;
    2 => request_id: Id16 = required;
    3 => segment_id: u64 = required;
    4 => offset: u32 = required;
    5 => source_device: Option<DeviceId> = optional;
    6 => proxy_device: Option<DeviceId> = optional;
    7 => session_id: Option<Id16> = optional;
    8 => batch_id: Option<Id16> = optional;
    9 => sequence: Option<u64> = optional;
});

wire_map!(DiagnosticsSegmentEnd, no_validation, {
    1 => request_id: Id16 = required;
    2 => segment_id: u64 = required;
    3 => compressed_length: u32 = required;
    4 => raw_length: u32 = required;
    5 => source_device: Option<DeviceId> = optional;
    6 => proxy_device: Option<DeviceId> = optional;
    7 => session_id: Option<Id16> = optional;
    8 => batch_id: Option<Id16> = optional;
    9 => sequence: Option<u64> = optional;
});

wire_map!(DiagnosticsEnd, no_validation, {
    1 => request_id: Id16 = required;
    2 => coverage: List<Coverage,65535> = required;
    3 => partial: bool = required;
    4 => evicted_segments: u64 = required;
    5 => mobile_partial: bool = false;
    6 => mobile_evicted_bytes: u64 = 0;
    7 => mobile_sources: Option<List<DeviceId,64>> = optional;
});

wire_map!(DiagnosticsUnavailable, no_validation, {
    1 => reason: CollectionFailure = required;
    2 => request_id: Id16 = required;
    3 => source_device: Option<DeviceId> = optional;
    4 => proxy_device: Option<DeviceId> = optional;
});

wire_map!(DiagnosticsEnvironment, no_validation, {
    1 => snapshot: Snapshot = required;
    2 => request_id: Id16 = required;
    3 => format: Ascii<96> = required;
});

wire_map!(AttachmentHead, no_validation, {
    1 => id: u64 = required;
    2 => header: AttachmentHeader = required;
    3 => remaining: u32 = required;
});

wire_map!(AttachmentItems, super::attachment::validate_attachment_items, {
    1 => id: u64 = required;
    2 => items: InventoryRemainder = required;
    3 => family: AttachmentKind = required;
    4 => identity: RawMap = required;
    5 => remaining: u32 = required;
});

wire_map!(AttachmentResult, super::attachment::validate_attachment_result, {
    1 => id: u64 = required;
    2 => kind: AttachmentKind = required;
    3 => outcome: AttachmentOutcome = required;
    4 => identity: RawMap = required;
    5 => remaining: u32 = required;
});

wire_map!(LinkConnectionSelect, no_validation, {
    1 => boot_pair: BootPair = required;
    2 => connection_id: ConnectionId = required;
    3 => choice_counter: u64 = required;
});

wire_map!(LinkConnectionSelected, no_validation, {
    1 => boot_pair: BootPair = required;
    2 => connection_id: ConnectionId = required;
    3 => choice_counter: u64 = required;
});

wire_map!(LinkConnectionReject, no_validation, {
    1 => id: ConnectionId = required;
    2 => reason: Refusal = required;
});

wire_map!(LinkPathRequest, no_validation, {
    1 => reservation: Option<RelayReservation> = optional;
    2 => request_id: PathRequestId = required;
    3 => expected_target_boot: Option<Boot> = optional;
    4 => target: DeviceId = required;
    5 => round: u64 = required;
});

wire_map!(LinkPathNominate, no_validation, {
    1 => request_id: PathRequestId = required;
    2 => reservation: RelayReservation = required;
    3 => endpoint_boot: Boot = required;
});

wire_map!(LinkPathInstall, no_validation, {
    1 => request_id: PathRequestId = required;
    2 => path_id: PathId = required;
    3 => route_revision: u64 = required;
    4 => reservation: Option<RelayReservation> = optional;
    5 => next_hop_binding: NextHopBinding = required;
});

wire_map!(LinkPathInstalled, no_validation, {
    1 => request_id: PathRequestId = required;
    2 => path_id: PathId = required;
    3 => route_revision: u64 = required;
    4 => reservation: Option<RelayReservation> = optional;
    5 => next_hop_binding: NextHopBinding = required;
});

wire_map!(LinkPathCancel, no_validation, {
    1 => reference: PathReference = required;
    2 => reason: Refusal = required;
});

wire_map!(LinkPathDown, no_validation, {
    1 => path_id: PathId = required;
});

wire_map!(LinkPulse, no_validation, {
    1 => path_id: PathId = required;
    2 => route_revision: u64 = required;
    3 => nonce: u64 = required;
});

wire_map!(LinkPong, no_validation, {
    1 => path_id: PathId = required;
    2 => route_revision: u64 = required;
    3 => nonce: u64 = required;
});

wire_map!(LinkHopProbe, no_validation, {
    1 => nonce: u64 = required;
});

wire_map!(LinkHopPong, no_validation, {
    1 => nonce: u64 = required;
});

wire_map!(LinkMigrateBegin, no_validation, {
    1 => path_id: PathId = required;
    2 => from_revision: u64 = required;
    3 => to_revision: u64 = required;
    4 => connection_id: ConnectionId = required;
    5 => migration_id: MigrationId = required;
});

wire_map!(LinkMigrateDrained, no_validation, {
    1 => migration_id: MigrationId = required;
    2 => old_path: PathId = required;
    3 => new_connection: ConnectionId = required;
});

wire_map!(LinkMigrateCommit, no_validation, {
    1 => migration_id: MigrationId = required;
    2 => old_path: PathId = required;
    3 => new_connection: ConnectionId = required;
});

wire_map!(LinkMigrateCommitted, no_validation, {
    1 => migration_id: MigrationId = required;
    2 => old_path: PathId = required;
    3 => new_connection: ConnectionId = required;
});

wire_map!(LowInfo, no_validation, {
    1 => paced: bool = required;
    2 => port: Option<u16> = optional;
    3 => failure: Option<ProtocolError> = optional;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowOpen, no_validation, {
    1 => request: Id16 = required;
    2 => connection: Id16 = required;
    3 => options: Option<LowOptions> = optional;
    4 => extra: bool = required;
    5 => keep_alive: bool = required;
    6 => purpose: LowTcpPurpose = required;
    7 => paced: bool = required;
    8 => binding: LowBinding = required;
    9 => route_tag: u64 = required;
    10 => kind: u16 = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowReady, no_validation, {
    1 => request: Id16 = required;
    2 => result: TypedResult = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowReverse, no_validation, {
    1 => request: Id16 = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowCancel, no_validation, {
    1 => request: Id16 = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowBind, no_validation, {
    1 => request: LowRequest = required;
    2 => reverse: bool = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowRecord, no_validation, {
    1 => bytes: Bytes<16384> = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowEnd, no_validation, {
    1 => request: Id16 = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowEndAck, no_validation, {
    1 => request: Id16 = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowRaceWinner, no_validation, {
    1 => request: Id16 = required;
    2 => reverse: bool = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowRefresh, no_validation, {
    100 => lease_binding: LowBinding = required;
});

wire_map!(LowRelayUnavailable, no_validation, {
    1 => request: Id16 = required;
    2 => failure: LowUnavailable = required;
    100 => lease_binding: LowBinding = required;
});

wire_map!(CacheUsage, no_validation, {
    1 => revision: u64 = required;
    2 => usage: CacheUsageValue = required;
});

wire_map!(CacheRequest, no_validation, {
    1 => clear: bool = required;
    2 => request_id: Id16 = required;
    3 => target: Boot = required;
});

wire_map!(CacheResult, no_validation, {
    1 => usage: CacheUsageValue = required;
    2 => clear: Option<CacheClear> = optional;
    3 => request_id: Id16 = required;
});

wire_map!(ProjectionPreview, validate_preview, {
    1 => sequence: u64 = required;
    2 => screens: Option<List<ScreenRect,64>> = optional;
    3 => affected: List<ScreenRef,64> = required;
});

wire_map!(ScreenRect,no_validation,{1=>device:DeviceId=required;2=>display:Text<512>=required;3=>rect:Rect=required;});
wire_map!(ScreenRef,no_validation,{1=>device:DeviceId=required;2=>display:Text<512>=required;});
open_enum!(SourceKind { Mouse=0,Touchpad=1,Keyboard=2,Indeterminate=3 });
open_enum!(GameMode { Auto=0,On=1,Off=2 });
open_enum!(ScrollAmount { Reference=0,Lines=1,Pixels=2 });
// Canvas: layout canvas units (a desktop source). Pointer: one logical point on
// the pointer base computer's primary screen (a phone source, protocol §14.3).
open_enum!(MotionUnit { Canvas=0,Pointer=1 });
open_enum!(EdgeState { Clear=0,Blocked=1,Approaching=2 });
open_enum!(EdgeSide { Left=0,Right=1,Top=2,Bottom=3 });
open_enum!(TextRefusal { Paused=0,NotJoined=1,Duplicate=2,Unsupported=3 });
open_enum!(ScrollPhase { None=0,Began=1,Changed=2,Ended=3,Cancelled=4,MomentumBegan=5,Momentum=6,MomentumEnded=7 });
open_enum!(VolumeKey { Up=0,Down=1,Mute=2 });
open_enum!(ContactPhase { Begin=0,Move=1,End=2,Cancel=3 });
open_enum!(Sensitivity { Normal=0,Concealed=1,Transient=2,AutoGenerated=3 });
open_enum!(ContentUnavailable { Unsupported=0,LimitExceeded=1,Superseded=2,ReadFailed=3,Cancelled=4,NoSuchItem=5,Permission=6,NoProgress=7,StorageFull=8,InvalidData=9,Paused=10,SendDisabled=11 });
open_enum!(SourceButtonDisposition { OriginalSourceOwns=0,ReleaseSubmitted=1,Released=2,ReturnedToOriginalRoute=3,Unconfirmed=4 });
open_enum!(ForwardingOffReason { LocalVolumeChanged=0,DefaultOutputChanged=1 });
open_enum!(CollectionFailure { Disabled=0,Recovering=1,Unsupported=2 });
open_enum!(AttachmentKind { Offer=0,Drag=1 });
open_enum!(AttachmentOutcome { Busy=0,Superseded=1,Expired=2,Invalid=3 });
open_enum!(Refusal { NotReady=0,Paused=1,Closed=2,Busy=3,Invalid=4,NotMember=5,DifferentGroups=6,Superseded=7,Expired=8,Cancelled=9 });
open_enum!(LowTcpPurpose { Transfer=0,Warm=1,Race=2 });
fn validate_input_capability(raw: &RawMap) -> Result<()> {
    if defaulted::<u8>(raw, 2, || 0)? > 5 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}
pub(super) fn validate_packet(raw: &RawMap) -> Result<()> {
    if RawValue::Map(raw.clone()).encoded_len()? + 10 > crate::v2::limits::DATAGRAM {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}
fn validate_preview(raw: &RawMap) -> Result<()> {
    let screens: Option<List<ScreenRect, 64>> = defaulted(raw, 2, || None)?;
    let affected: List<ScreenRef, 64> = required(raw, 3)?;
    let mut bytes = 0;
    if let Some(screens) = screens {
        for screen in screens.0 {
            bytes += screen.display()?.0.len();
            let rect = screen.rect()?;
            if rect.x()?.abs() > 1_000_000.0 / 686.0
                || rect.y()?.abs() > 1_000_000.0 / 686.0
                || !(0.0..=4000.0 / 686.0).contains(&rect.w()?)
                || !(0.0..=4000.0 / 686.0).contains(&rect.h()?)
                || rect.w()? == 0.0
                || rect.h()? == 0.0
            {
                return Err(ErrorCode::UnsupportedValue.into());
            }
        }
    }
    for screen in affected.0 {
        bytes += screen.display()?.0.len();
    }
    if bytes > 8000 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}

fn validate_volume(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 7)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}

fn validate_clipboard_offer(raw: &RawMap) -> Result<()> {
    let inventory = super::content::map_field(raw, 6)?;
    if required::<OfferId>(raw, 1)? != required::<OfferId>(inventory, 1)? {
        return Err(ErrorCode::StateConflict.into());
    }
    if super::content::inventory_inline_bytes(inventory)? > 4096 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    // protocol-v2 §4.1: a direct offer names its origin as the real sender and
    // has no proxy; a proxied offer is announced by the proxy itself on behalf
    // of a different real sender. Every other combination is contradictory.
    let origin = required::<OfferId>(raw, 1)?.clip()?.origin()?;
    let real_sender = required::<DeviceId>(raw, 8)?;
    let consistent = match defaulted::<Option<DeviceId>>(raw, 9, || None)? {
        None => real_sender == origin,
        Some(proxy) => proxy == origin && real_sender != proxy,
    };
    if !consistent {
        return Err(Error::from(ErrorCode::StateConflict).at_field(8));
    }
    Ok(())
}

// Confirms the current real clipboard file version before any payload bytes.
// Size and mtime belong to this version, never to an earlier placeholder.
wire_map!(ContentFileVersion, no_validation, {
    1 => offer: OfferId = required;
    2 => entry: FileNode = required;
});

// Optional focus request, interpreted only by the current holder at this epoch.
// Focus holder -> the source of its latest pointer-unit motion (protocol §14.4).
wire_map!(ControlEdgeReport, no_validation, {
    1 => epoch: u64 = required;
    2 => state: EdgeState = required;
    3 => edge: EdgeSide = required;
    4 => target: Option<DeviceId> = optional;
});
// Focus holder -> text source: this InputText was not injected, and why (protocol §12.1).
wire_map!(ControlTextRefused, no_validation, {
    1 => seq: u64 = required;
    2 => reason: TextRefusal = required;
});
// Focus holder -> phone sources: the holder's text-input focus (protocol
// §14.7). `seq` increases once per keyboard focus into an editable element (an
// input-focus episode); `editable` / `secure` say whether an editable element,
// and whether a password field, is focused now; `revision` increases on every
// change of these, so a receiver drops records that are not newer; `epoch` is
// the sender's focus epoch when it sent the record. Sent on every change while
// the sender holds focus and repeated on the 2 s heartbeat; never carries the
// element's text, label or application.
wire_map!(ControlTextFocus, no_validation, {
    1 => epoch: u64 = required;
    2 => seq: u64 = required;
    3 => editable: bool = required;
    4 => secure: bool = required;
    5 => revision: u64 = required;
});
// Retained group register value (kind 0x1304, protocol §14.3).
wire_map!(PointerBase, no_validation, {
    1 => device: DeviceId = required;
});
wire_map!(ControlMovePointer, no_validation, {
    1 => epoch: u64 = required;
    2 => target: DeviceId = required;
});
// Retained group register value (kind 0x1301); unknown to baseline peers.
wire_map!(PointerShortcutSlots, no_validation, {
    1 => entries: List<PointerShortcutSlot,16> = required;
});
wire_map!(PointerShortcutSlot, no_validation, {
    1 => device: DeviceId = required;
    2 => membership_epoch: u64 = required;
    3 => slot: Option<u8> = optional;
});
// Retained group register value (kind 0x1302, shipped in 0.3.6 with this
// exact shape): whole-list LWW device list order.
wire_map!(DeviceOrder, no_validation, {
    1 => entries: List<DeviceOrderEntry,16> = required;
});
wire_map!(DeviceOrderEntry, no_validation, {
    1 => device: DeviceId = required;
    2 => membership_epoch: u64 = required;
    3 => order: i64 = required;
});

// Optional physical-source hold state; never changes the device lock preference.
wire_map!(ControlLockEscape, no_validation, {1 => held: bool = required;});

// Device-owned entry policy, independent of the current focus epoch.
wire_map!(ControlEntryProtection, no_validation, {
    1 => locked: bool = required;
    2 => game: bool = required;
    3 => passive_displays: List<Text<512>,64> = required;
});

wire_map!(ControlPowerState, validate_power_state, {
    1 => available: bool = required;
    2 => display: bool = required;
    3 => system: bool = required;
    4 => activity: bool = required;
});
fn validate_power_state(raw: &RawMap) -> Result<()> {
    if !required::<bool>(raw, 1)?
        && (required::<bool>(raw, 2)? || required::<bool>(raw, 3)? || required::<bool>(raw, 4)?)
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

// Appended optional I0 action. Boots fence process reincarnations; epoch fences focus.
wire_map!(ControlLockRemoteWindows, no_validation, {
    1 => source: Boot = required;
    2 => target: Boot = required;
    3 => epoch: u64 = required;
    4 => request_id: u64 = required;
});

wire_map!(ControlLocalClick, validate_local_click, {
    1 => epoch: u64 = required;
    2 => holder: DeviceId = required;
    3 => scope: FocusScope = required;
    4 => phase: u8 = required;
});

fn validate_local_click(raw: &RawMap) -> Result<()> {
    if required::<u8>(raw, 4)? > 5 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

// Source answers about its own current console; no credential or credential status.
wire_map!(ControlUnlockQuery,no_validation,{1=>epoch:u64=required;});
// `paused`: the source is paused and answers at once instead of not at all.
wire_map!(ControlUnlockReply,no_validation,{1=>epoch:u64=required;2=>unlocked:bool=required;3=>paused:bool=false;});
// An unlocked source's per-edge hint. The target never trusts it: it challenges
// the source with ControlUnlockQuery on the same session before acting.
wire_map!(ControlSyncUnlock,no_validation,{1=>epoch:u64=required;});

#[cfg(test)]
mod input_text_tests {
    use super::*;
    use crate::v2::{
        Message, RawValue as V,
        admission::{Agreement, Context, Phase},
        frame::OpaqueFrame,
        registry::Transport,
        schema::WireDto,
    };

    fn text(seq: u64, value: &str) -> Result<InputText> {
        InputText::from_raw([(1, V::Uint(seq)), (2, V::Text(value.into()))].into())
    }
    fn context(kind: u16, agreement: &Agreement) -> Context<'_> {
        Context {
            kind,
            transport: Transport::Reliable,
            phase: Phase::Member,
            agreement,
            direct: true,
            old_member_lane: false,
        }
    }

    #[test]
    fn input_text_round_trips_on_i0_with_input_negotiated() {
        let sample = "中文 English 😀 \u{1F468}\u{200D}\u{1F469}";
        let message = Message::InputText(text(7, sample).unwrap());
        assert_eq!(message.type_id(), 0x020d);
        let bytes = crate::v2::frame::encode(&message, crate::v2::limits::RELIABLE_FRAME).unwrap();
        let frame = OpaqueFrame::parse_exact(&bytes, crate::v2::limits::RELIABLE_FRAME).unwrap();
        // Protocol §12: an I0 input message, like a key press.
        let input = Agreement::new(
            ["oink.core/2", "oink.group/2", "oink.input/2"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let Message::InputText(decoded) = context(3, &input).decode(frame).unwrap() else {
            panic!("decoded another message");
        };
        assert_eq!(decoded.seq().unwrap(), 7);
        assert_eq!(decoded.text().unwrap().0, sample);
        // C0 is no longer a route of this message.
        assert_eq!(
            context(2, &input).decode(frame).unwrap_err().code,
            crate::v2::ErrorCode::InvalidChannel
        );
    }

    #[test]
    fn input_text_bounds_and_rejects_keys_disguised_as_text() {
        assert!(text(1, &"a".repeat(INPUT_TEXT_MAX_BYTES)).is_ok());
        // 1366 three-byte characters = 4098 bytes.
        assert_eq!(
            text(1, &"中".repeat(1366)).unwrap_err().code,
            crate::v2::ErrorCode::LimitExceeded
        );
        assert_eq!(text(0, "a").unwrap_err().code, crate::v2::ErrorCode::MalformedFrame);
        for invalid in ["", "a\nb", "\r", "\t", "\u{8}", "\u{7f}", "\u{85}"] {
            assert_eq!(
                text(1, invalid).unwrap_err().code,
                crate::v2::ErrorCode::UnsupportedValue,
                "{invalid:?}"
            );
        }
        // Format characters (ZWJ, variation selectors) are text, not controls.
        assert!(text(1, "\u{200D}\u{FE0F}").is_ok());
        assert_eq!(format!("{:?}", text(2, "secret").unwrap()), "InputText");
    }
}

#[cfg(test)]
mod sync_unlock_tests {
    use super::*;
    use crate::v2::{
        Message, RawValue as V,
        admission::{Agreement, Context, Phase},
        frame::OpaqueFrame,
        registry::Transport,
        schema::WireDto,
    };

    #[test]
    fn sync_unlock_is_an_i0_message_that_needs_input_and_device_on_both_ends() {
        let message = Message::ControlSyncUnlock(ControlSyncUnlock::from_raw([(1, V::Uint(1))].into()).unwrap());
        assert_eq!(message.type_id(), 0x1305);
        let bytes = crate::v2::frame::encode(&message, crate::v2::limits::CONFIG_FRAME).unwrap();
        let fixture: Vec<u8> = include_str!("../../../tests/fixtures/v2/1305.hex")
            .trim()
            .as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(bytes, fixture);
        let frame = OpaqueFrame::parse_exact(&bytes, crate::v2::limits::CONFIG_FRAME).unwrap();
        for (capabilities, accepted) in [
            (
                &["oink.core/2", "oink.group/2", "oink.input/2", "oink.device/2"][..],
                true,
            ),
            (&["oink.core/2", "oink.group/2", "oink.input/2"][..], false),
            (&["oink.core/2", "oink.group/2", "oink.device/2"][..], false),
        ] {
            let agreement = Agreement::new(capabilities.iter().map(|c| (*c).to_owned())).unwrap();
            // Input admission does not depend on whether the logical path is
            // direct or relayed; only pairing is restricted to direct links.
            for direct in [true, false] {
                let context = Context {
                    kind: 3,
                    transport: Transport::Reliable,
                    phase: Phase::Member,
                    agreement: &agreement,
                    direct,
                    old_member_lane: false,
                };
                assert_eq!(context.decode(frame).is_ok(), accepted, "{capabilities:?}");
            }
        }
    }
}
