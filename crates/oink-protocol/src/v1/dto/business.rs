use super::facts::validate_generation;
use super::input_values::validate_input_contacts;
use super::*;
use crate::v1::hot::hot_map;
use crate::v1::schema::{defaulted, no_validation, open_enum, required, wire_map};
use crate::v1::{ErrorCode, RawMap, RawValue, Result};

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

wire_map!(ControlIdentifyScreens, no_validation, {
    1 => id: u64 = required;
});

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

wire_map!(ClipboardOffer, validate_clipboard_offer, {
    1 => id: OfferId = required;
    2 => source_change_count: u64 = required;
    3 => clock: u64 = required;
    4 => sensitivity: Sensitivity = required;
    5 => content_digest: Digest = required;
    6 => inventory: Inventory = required;
    7 => time_ms: u64 = required;
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
    4 => hash: Digest = required;
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
    5 => check: Option<Digest> = optional;
    6 => demand: Demand = required;
});

wire_map!(ContentFileBlock, no_validation, {
    1 => offer: OfferId = required;
    2 => node: u64 = required;
    3 => version: u64 = required;
    4 => offset: u64 = required;
    5 => hash: Digest = required;
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
    2 => receipt: Digest = required;
});

wire_map!(ContentFailed, no_validation, {
    1 => delivery: DeliveryId = required;
    2 => reason: ContentUnavailable = required;
});

wire_map!(ContentTerminalAck, no_validation, {
    1 => delivery: DeliveryId = required;
    2 => receipt: Option<Digest> = optional;
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
    4 => exit_claim: ExitClaimStamp = required;
});
wire_map!(AudioPrepareRefused, no_validation, {
    1 => offer: SourceRouteOffer = required;
    2 => reason: ControlFailure = required;
    3 => current_output: Option<ReceiverOutputBinding> = optional;
});

wire_map!(AimOpen, no_validation, {
    1 => event: GameEvent = required;
});

wire_map!(AimJoined, no_validation, {
    1 => event: GameEvent = required;
});

wire_map!(AimDeclined, no_validation, {
    1 => event: GameEvent = required;
});

wire_map!(AimState, super::auxiliary::validate_aim_state, {
    1 => game: GameInstance = required;
    2 => seq: u64 = required;
    3 => phase: AimPhase = required;
    4 => remaining_ms: u32 = required;
    5 => score: u32 = required;
    6 => clicks: u32 = required;
    7 => radius: f32 = required;
    8 => native_input: bool = required;
    9 => targets: List<AimTarget,8> = required;
});

wire_map!(AimClick, super::auxiliary::validate_aim_click, {
    1 => event: GameEvent = required;
    2 => display: Text<512> = required;
    3 => pos: Point = required;
});

wire_map!(AimClose, no_validation, {
    1 => event: GameEvent = required;
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
});

wire_map!(DiagnosticsGoodbye, no_validation, {
    1 => instance: Text<128> = required;
});

wire_map!(DiagnosticsRequest, no_validation, {
    1 => t1: ClockStamp = required;
    2 => start_us: i64 = required;
    3 => end_us: i64 = required;
    4 => request_id: Id16 = required;
    5 => accepted_formats: List<Ascii<96>,16> = required;
    6 => accepted_compressions: List<Ascii<96>,16> = (List(vec![Ascii("none".into())]));
});

wire_map!(DiagnosticsResponse, no_validation, {
    1 => instance: Text<128> = required;
    2 => evicted_segments: u64 = required;
    3 => t2: ClockStamp = required;
    4 => t3: ClockStamp = required;
    5 => request_id: Id16 = required;
    6 => format: Ascii<96> = required;
    7 => compression: Ascii<96> = required;
});

wire_map!(DiagnosticsChunk, no_validation, {
    1 => bytes: Bytes<16384> = required;
    2 => request_id: Id16 = required;
    3 => segment_id: u64 = required;
    4 => offset: u32 = required;
});

wire_map!(DiagnosticsSegmentEnd, no_validation, {
    1 => request_id: Id16 = required;
    2 => segment_id: u64 = required;
    3 => compressed_length: u32 = required;
    4 => raw_length: u32 = required;
    5 => digest: Digest = required;
});

wire_map!(DiagnosticsEnd, no_validation, {
    1 => request_id: Id16 = required;
    2 => coverage: List<Coverage,65535> = required;
    3 => partial: bool = required;
    4 => evicted_segments: u64 = required;
});

wire_map!(DiagnosticsUnavailable, no_validation, {
    1 => reason: CollectionFailure = required;
    2 => request_id: Id16 = required;
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

wire_map!(ProjectionMode, no_validation, {
    1 => id: ModeId = required;
    2 => enabled: bool = required;
});

wire_map!(ProjectionGesture, no_validation, {
    1 => id: ModeId = required;
    2 => active: bool = required;
});
wire_map!(ModeId,no_validation,{1=>owner:DeviceId=required;2=>serial:u64=required;3=>boot_nonce:Id16=required;});
wire_map!(ScreenRect,no_validation,{1=>device:DeviceId=required;2=>display:Text<512>=required;3=>rect:Rect=required;});
wire_map!(ScreenRef,no_validation,{1=>device:DeviceId=required;2=>display:Text<512>=required;});
open_enum!(SourceKind { Mouse=0,Touchpad=1,Keyboard=2,Indeterminate=3 });
open_enum!(GameMode { Auto=0,On=1,Off=2 });
open_enum!(ScrollAmount { Reference=0,Lines=1 });
open_enum!(ScrollPhase { None=0,Began=1,Changed=2,Ended=3,Cancelled=4,MomentumBegan=5,Momentum=6,MomentumEnded=7 });
open_enum!(VolumeKey { Up=0,Down=1,Mute=2 });
open_enum!(ContactPhase { Begin=0,Move=1,End=2,Cancel=3 });
open_enum!(Sensitivity { Normal=0,Concealed=1,Transient=2,AutoGenerated=3 });
open_enum!(ContentUnavailable { Unsupported=0,LimitExceeded=1,Superseded=2,ReadFailed=3,Cancelled=4,NoSuchItem=5,Permission=6,NoProgress=7,StorageFull=8,InvalidData=9 });
open_enum!(SourceButtonDisposition { OriginalSourceOwns=0,ReleaseSubmitted=1,Released=2,ReturnedToOriginalRoute=3,Unconfirmed=4 });
open_enum!(ForwardingOffReason { LocalVolumeChanged=0,DefaultOutputChanged=1 });
open_enum!(AimPhase { Countdown=0,Running=1,Finished=2 });
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
fn validate_packet(raw: &RawMap) -> Result<()> {
    if RawValue::Map(raw.clone()).encoded_len()? + 10 > crate::v1::limits::DATAGRAM {
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
    let inventory = required::<Inventory>(raw, 6)?;
    if required::<OfferId>(raw, 1)? != inventory.id()? {
        return Err(ErrorCode::StateConflict.into());
    }
    if inventory.inline_bytes()? > 4096 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}

// Confirms the current real clipboard file version before any payload bytes.
// Size and mtime belong to this version, never to an earlier placeholder.
wire_map!(ContentFileVersion, no_validation, {
    1 => offer: OfferId = required;
    2 => entry: FileNode = required;
});
