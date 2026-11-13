use super::*;
use crate::v2::hot::hot_map;
use crate::v2::schema::{defaulted, no_validation, open_enum, required, wire_map};
use crate::v2::{ErrorCode, RawMap, RawValue, Result};

wire_map!(DeviceFacts, validate_facts, {
    1 => boot: Boot = required;
    2 => fact_sequence: u64 = required;
    3 => protocol: ProtocolVersion = required;
    4 => build: Text<256> = required;
    5 => group: Id16 = required;
    6 => name: Text<256> = required;
    7 => os: Text<256> = required;
    8 => os_version: Text<256> = required;
    9 => app_version: Text<256> = required;
    10 => endpoints: List<EndpointFact,256> = required;
    11 => observed: ObservedState = required;
    12 => displays: List<DisplayFact,256> = required;
    13 => capabilities: FactCapabilities = required;
    14 => local_options: LocalOptionsSummary = required;
    15 => links: List<LinkFact,16> = required;
    16 => resources: Option<ResourceFacts> = optional;
    17 => exit_volume: Option<ExitVolumeState> = optional;
    18 => preferences: Option<List<DevicePreference,32>> = optional;
    19 => pause: Option<LocalPauseState> = optional;
    // Keys 20, 21 and 24 carried the retired per-device exit/microphone claims
    // and their watermarks; they are not reused, so an earlier 0.4.0 build's
    // facts still decode (its claim keys are retained, never read).
    22 => native_arrangement_token: Option<Text<128>> = optional;
    // oink.input.accessibility/1: absent means unknown or not applicable.
    23 => accessibility_granted: Option<bool> = optional;
    25 => device_kind: DeviceKind = required;
    26 => protocol_major: u16 = required;
    27 => protocol_minor: u16 = required;
    // Mobile devices only; absent means unknown. Presentation, never policy.
    31 => form_factor: Option<FormFactor> = optional;
    // Android devices only: the privileged helper is enabled and running (the
    // phone can be driven by a PC's mouse and keyboard). Absent means false.
    // Drives the crowned badge and whether a PC may open the phone's audio
    // endpoints (protocol-v2 7, key 32).
    32 => android_privileged: bool = false;
    // The global audio configuration this device executes: the two group
    // registers (speaker exit, microphone sender) and the forward / receive
    // switches of this device and the devices it links to. Absent while the
    // device has not merged after a start or group switch; see AudioConfig.
    33 => audio_config: Option<AudioConfig> = optional;
});

wire_map!(EndpointFact, validate_endpoint, {
    1 => family: AddressFamily = required;
    2 => address: Bytes<16> = required;
    3 => port: u16 = required;
    4 => interface_scope: u32 = required;
    5 => tcp_port: Option<u16> = optional;
});

wire_map!(ObservedState, no_validation, {
    1 => awake: bool = required;
    2 => intent: RunIntent = required;
    3 => sharing_unavailable: Option<SharingUnavailable> = optional;
    4 => permission_bits: u64 = required;
    6 => problems: List<UserFacingProblem,16> = (List(Vec::new()));
});

wire_map!(DisplayFact, no_validation, {
    1 => key: Text<512> = required;
    2 => name: Text<256> = required;
    3 => bounds: Rect = required;
    5 => pixel_width: u32 = required;
    6 => pixel_height: u32 = required;
    7 => reference_extent: Option<Size> = optional;
    8 => rotation_deg: u16 = required;
    9 => scale: f64 = required;
    10 => is_primary: bool = required;
    11 => is_builtin: bool = required;
    12 => is_virtual: bool = required;
    // System scale of this screen: macOS 1, Windows effective DPI / 96 (§7).
    13 => system_scale: Option<f64> = optional;
});

wire_map!(AudioOutputFact, no_validation, {
    1 => id: Text<512> = required;
    2 => name: Text<256> = required;
    3 => is_default: bool = required;
    4 => reported_latency_ms: Option<f64> = optional;
});

wire_map!(FactCapabilities, no_validation, {
    1 => supported: CapabilityList = required;
    2 => capture_permission: CapturePermission = required;
    3 => audio: List<AudioSupport,16> = required;
    4 => audio_outputs: List<AudioOutputFact,32> = required;
});

wire_map!(LocalOptionsSummary, no_validation, {
    1 => sharing_enabled: bool = required;
    // Key 2 (forwarding_enabled) is retired: the forward switch lives in the
    // group audio configuration (DeviceFacts key 33).
    3 => forwarding_volume: f32 = required;
    4 => input_locked: bool = required;
    5 => keyboard_disabled: bool = required;
    6 => screen_local_input: bool = required;
    // The device the group pointer focus is currently locked to, as observed
    // by this publisher's input owner. Absent when unlocked or unknown.
});

wire_map!(LinkFact, no_validation, {
    1 => peer: DeviceId = required;
    2 => path: FactPath = required;
    3 => next_hop: Option<DeviceId> = optional;
    4 => rtt_us: Option<u64> = optional;
    5 => sample_window_ms: u32 = required;
    6 => lost: u64 = required;
    7 => failures: u64 = required;
    8 => low_available: bool = required;
    9 => route_revision: Option<u64> = optional;
});

wire_map!(ResourceFacts, validate_resources, {
    1 => clipboard_mode: Option<SharingMode> = optional;
    2 => mouse_mode: Option<SharingMode> = optional;
    3 => keyboard_mode: Option<SharingMode> = optional;
    4 => faults: Option<ResourceFaults> = optional;
    5 => speaker: Option<SpeakerObservation> = optional;
    6 => option_revisions: List<OptionRevision,32> = (List(Vec::new()));
    7 => game: Option<GameObservation> = optional;
    8 => microphone: Option<MicrophoneObservation> = optional;
});

wire_map!(ResourceFaults, no_validation, {
    1 => screen: Option<ResourceFault> = optional;
    2 => mouse: Option<ResourceFault> = optional;
    3 => keyboard: Option<ResourceFault> = optional;
    4 => speaker: Option<ResourceFault> = optional;
    5 => clipboard: Option<ResourceFault> = optional;
});

wire_map!(ResourceFault, no_validation, {
    1 => code: ResourceFaultCode = required;
    3 => problem: Option<UserFacingProblem> = optional;
});

wire_map!(SpeakerObservation, validate_speaker, {
    1 => source_session: Option<AudioSessionContext> = optional;
    2 => activity: Option<SpeakerActivity> = optional;
    3 => forwarding_volume_percent: Option<u16> = optional;
    4 => forwarding_muted: Option<bool> = optional;
    5 => receiving_output: Option<ReceiverOutputBinding> = optional;
    6 => active_sources: Option<u8> = optional;
    7 => receive_ready: Option<bool> = optional;
    8 => source_destination: Option<DeviceId> = optional;
    9 => local_output: Option<LocalOutputVolume> = optional;
});

// Read-only observation of this owner's default output, independent of exit selection.
wire_map!(LocalOutputVolume, validate_local_output, {
    1 => volume_percent: u8 = required;
    2 => muted: bool = required;
});
fn validate_local_output(raw: &RawMap) -> Result<()> {
    if required::<u8>(raw, 1)? > 100 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

// These contexts also occur in realtime directed Volume. Their nested typed
// projections must borrow the input bytes rather than allocate retaining DTOs.
hot_map!(ReceiverOutputBinding, validate_generation, {
    1 => receiver: Boot = required;
    2 => generation: u64 = required;
    3 => output_id: Option<Text<512>> = optional;
});
hot_map!(AudioSessionContext, validate_generation, {
    1 => source: Boot = required;
    2 => source_route_generation: u64 = required;
    3 => receiver_output: ReceiverOutputBinding = required;
    4 => route_version: AudioRouteVersion = required;
});
wire_map!(DevicePreference, validate_option_revision, {
    1 => option: LocalOption = required;
    2 => revision: u64 = required;
    3 => value: RawValue = required;
});
wire_map!(LocalPauseState, validate_pause, {
    1 => paused: bool = required;
    2 => reason: PauseReason = required;
    3 => revision: u64 = required;
    4 => emergency_origin: Option<DeviceId> = optional;
});
wire_map!(EmergencyEventId, validate_generation, {
    1 => origin: Boot = required;
    2 => sequence: u64 = required;
});
wire_map!(EmergencyPause, no_validation, {
    1 => event: EmergencyEventId = required;
    2 => target: Boot = required;
});
wire_map!(EmergencyPauseAck, validate_emergency_ack, {
    1 => event: EmergencyEventId = required;
    2 => target: Boot = required;
    3 => outcome: EmergencyOutcome = required;
    4 => pause_revision: u64 = required;
    5 => cleanup_pending: bool = required;
});
open_enum!(PauseReason { None=0,LocalUser=1,Emergency=3,RemoteUser=4,InputBacklog=5 });
open_enum!(EmergencyOutcome { EnteredPause=0,AlreadyPaused=1,AlreadySeen=2 });
open_enum!(AddressFamily { Ipv4=0,Ipv6=1 });
open_enum!(RunIntent { Starting=0,Running=1,Stopping=2 });
open_enum!(SharingUnavailable { Paused=0,Joining=1,Sleeping=2,Permission=3,CleanupFailed=4,Disabled=5 });
open_enum!(FactPath { None=0,Direct=1,Relayed=2 });
// Semantic Unknown=1 is distinct from an unrecognised numeric id.
open_enum!(CapturePermission { NotRequested=0,Indeterminate=1,Authorized=2,Denied=3,NotRequired=4 });
open_enum!(SharingMode { Shared=0,Local=1 });
open_enum!(ResourceFaultCode { None=0,PermissionRequired=1,PermissionDenied=2,CaptureUnavailable=3,InjectionUnavailable=4,SecureInput=5,DeviceUnavailable=6,CleanupFailed=7,Unsupported=8,BackendFailed=9,MicrophoneLoopback=10 });
open_enum!(SpeakerActivity { Unused=0,ExitReady=1,ExitPlaying=2,SourceForwarding=3,SourceSoundSleep=4,Preparing=5,WaitingForExit=6,LocalPaused=7,SystemSleeping=8,Failed=9 });
fn validate_facts(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 2)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let mut previous = None;
    if let Some(items) = defaulted::<Option<List<DevicePreference, 32>>>(raw, 18, || None)? {
        for item in items.0 {
            let option = item.option()?.number();
            if previous.is_some_and(|n| n >= option) {
                return Err(ErrorCode::MalformedFrame.into());
            }
            previous = Some(option);
        }
    }
    Ok(())
}
fn validate_endpoint(raw: &RawMap) -> Result<()> {
    let length = required::<Bytes<16>>(raw, 2)?.0.len();
    match required::<AddressFamily>(raw, 1)? {
        AddressFamily::Ipv4 if length != 4 => return Err(ErrorCode::MalformedFrame.into()),
        AddressFamily::Ipv6 if length != 16 => return Err(ErrorCode::MalformedFrame.into()),
        _ => {}
    }
    if required::<u16>(raw, 3)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_resources(raw: &RawMap) -> Result<()> {
    let mut previous = None;
    for entry in defaulted::<List<OptionRevision, 32>>(raw, 6, || List(Vec::new()))?.0 {
        let option = entry.option()?.number();
        if previous.is_some_and(|n| n >= option) {
            return Err(ErrorCode::MalformedFrame.into());
        }
        previous = Some(option);
    }
    if RawValue::Map(raw.clone()).encoded_len()? > crate::v2::limits::RESOURCE_FACTS {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}
fn validate_speaker(raw: &RawMap) -> Result<()> {
    if defaulted::<Option<u16>>(raw, 3, || None)?.is_some_and(|v| v > 200) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    if defaulted::<Option<u8>>(raw, 6, || None)?.is_some_and(|v| v > 16) {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}

wire_map!(OptionRevision,validate_option_revision,{1=>option:LocalOption=required;2=>revision:u64=required;});
wire_map!(SetLocalOption,no_validation,{
    1=>request_id:Id16=required;2=>target:Boot=required;3=>option:LocalOption=required;
    4=>value:RawValue=required;5=>expected_revision:u64=required;
    6=>defer_persistence:bool=false;
});
wire_map!(LocalOptionState,validate_option_state,{
    1=>option:LocalOption=required;2=>value:RawValue=required;3=>revision:u64=required;
});
wire_map!(LocalEditResult,no_validation,{
    1=>request_id:Id16=required;2=>target:Boot=required;3=>outcome:ControlOutcome=required;
    4=>reason:ControlFailure=required;5=>current:Option<LocalOptionState>=optional;
    6=>fact_sequence:Option<u64>=optional;7=>persisted:bool=required;
});
wire_map!(ExitVolumeVersion,validate_exit_version,{
    1=>exit:Boot=required;3=>output_id:Option<Text<512>>=optional;
    4=>output_generation:u64=required;5=>revision:u64=required;6=>route_version:AudioRouteVersion=required;
});
wire_map!(ExitVolumeState,validate_exit_state,{
    1=>version:ExitVolumeVersion=required;2=>volume_percent:Option<u8>=optional;
    3=>muted:Option<bool>=optional;4=>status:ExitVolumeStatus=required;
});
wire_map!(SetExitVolume,no_validation,{
    1=>request_id:Id16=required;2=>expected:ExitVolumeVersion=required;3=>volume_percent:u8=required;4=>muted:bool=required;
});
wire_map!(ExitVolumeResult,no_validation,{
    1=>request_id:Id16=required;2=>exit:Boot=required;3=>outcome:ControlOutcome=required;
    4=>reason:ControlFailure=required;5=>current:Option<ExitVolumeState>=optional;
});
wire_map!(ExitVolumeChanged,no_validation,{1=>state:ExitVolumeState=required;});
wire_map!(VolumeRefused,no_validation,{
    1=>press_id:u64=required;2=>reason:ControlFailure=required;3=>source_alias:Text<128>=required;
    4=>target_alias:Text<128>=required;5=>action:VolumeKey=required;
});
// 4 (ForwardingEnabled) and 24 (MicrophoneReceiveMuted) are retired: they
// became the forward / receive switches of the global audio configuration
// (DeviceFacts key 33). A ConfigSetLocalOption for them is UnsupportedOption.
open_enum!(LocalOption {MouseMode=1,KeyboardMode=2,ClipboardEnabled=3,KeyboardDisabled=5,
    ScreenShared=6,ScreenPreferences=7,SpeedMode=8,CursorTrail=10,CursorTrailDuration=11,
    RemoteScrollSpeed=12,InputTransform=13,SourceInputPolicy=14,ForwardingControls=15,DeviceName=16,GameModePreference=18,AcceptContacts=19,SyncSystemArrangement=21,TouchpadInjection=26,NativeMultifinger=27,MacScrollFeel=28,MicrophoneControls=23,MicrophoneInput=25,AudioQuality=29,DisableHiddenDisplays=30});
open_enum!(ControlOutcome {Applied=0,NoChange=1,Refused=2,Failed=3});
open_enum!(ControlFailure {None=0,Offline=1,Unsupported=2,StaleBoot=3,StaleRevision=4,NotMember=5,ResourceFault=6,InvalidValue=7,Busy=8,PersistenceFailed=9,ApplyFailed=10,LinkLost=11,OutcomeUnknown=12,NotExit=13,RouteChanged=14,OutputChanged=15,OutputUnavailable=16,VolumeUnsupported=17,PermissionDenied=18,Superseded=19,UnsupportedOption=20,StateConflict=21});
open_enum!(ExitVolumeStatus {Ready=0,Unsupported=1,Unavailable=2,PermissionDenied=3,BackendFailed=4});
fn validate_option_revision(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 2)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_option_state(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 3)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_exit_version(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 4)? == 0 || required::<u64>(raw, 5)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_exit_state(raw: &RawMap) -> Result<()> {
    let volume = defaulted::<Option<u8>>(raw, 2, || None)?;
    let muted = defaulted::<Option<bool>>(raw, 3, || None)?;
    if volume.is_some_and(|n| n > 100) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    if required::<ExitVolumeStatus>(raw, 4)? == ExitVolumeStatus::Ready
        && (volume.is_none() || muted.is_none() || required::<ExitVolumeVersion>(raw, 1)?.output_id()?.is_none())
    {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}

pub(super) fn validate_generation(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 2)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_pause(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 3)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_emergency_ack(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 4)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}

// Version of a group audio register or switch: a hybrid logical clock stamp
// taken when the choice was made (high 48 bits: the author's Unix
// milliseconds, low 16 bits: a logical counter) and the author. Every device
// executes the newest version, compared by (stamp, author). Media sessions
// and exit volume bind the register version, so an X->Y->X change cannot
// revive an old session or volume press.
hot_map!(AudioRouteVersion,validate_route_version,{
    1=>stamp:u64=required;2=>author:DeviceId=required;
});
// One group-wide register: the speaker exit or the microphone sender. An
// absent target is an explicit "none". The author need not be the target.
wire_map!(AudioRoute,no_validation,{
    1=>version:AudioRouteVersion=required;2=>target:Option<DeviceId>=optional;
});
// One non-exclusive switch of one device: speaker "forward" or microphone
// "receive". It only applies while that device holds no register.
wire_map!(AudioFlag,no_validation,{
    1=>on:bool=required;2=>version:AudioRouteVersion=required;
});
// The switches of one online device. An absent switch is not known yet (a
// setter wrote only the other one) and reads as off.
wire_map!(AudioDeviceModes,no_validation,{
    1=>device:DeviceId=required;2=>forward:Option<AudioFlag>=optional;3=>receive:Option<AudioFlag>=optional;
});
// The global audio configuration (DeviceFacts key 33): the two registers,
// each with its own version so changing one never retires the other's
// sessions, and the switches of the publisher and the devices it links to
// (one hop), so only online devices are ever listed. An absent register means
// this device has seen no choice for it.
wire_map!(AudioConfig,no_validation,{
    1=>speaker_exit:Option<AudioRoute>=optional;2=>microphone_source:Option<AudioRoute>=optional;
    3=>devices:List<AudioDeviceModes,64>=(List(Vec::new()));
});
wire_map!(GameObservation,no_validation,{1=>active:bool=required;2=>detected:bool=required;});
wire_map!(PauseDevice,no_validation,{
    1=>request_id:Id16=required;2=>target:Boot=required;3=>paused:bool=required;4=>expected_revision:u64=required;
});
wire_map!(PauseDeviceResult,no_validation,{
    1=>request_id:Id16=required;2=>target:Boot=required;3=>outcome:ControlOutcome=required;
    4=>reason:ControlFailure=required;5=>current:Option<LocalPauseState>=optional;6=>fact_sequence:Option<u64>=optional;
});
pub(super) fn validate_route_version(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 1)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::{
        Message, frame,
        limits::CONFIG_FRAME,
        schema::{WireDto, WireValue},
    };

    fn map<const N: usize>(items: [(u32, RawValue); N]) -> RawValue {
        RawValue::Map(items.into())
    }

    /// A minimal valid mobile DeviceFacts as an Android phone publishes it.
    fn android_facts() -> RawMap {
        let id = map([(1, RawValue::from("ed25519")), (2, RawValue::Bytes(vec![3; 32]))]);
        [
            (1, map([(1, id), (2, RawValue::Bytes(vec![4; 16]))])),
            (2, 1u64.write()),
            (3, map([(1, crate::v2::MAJOR.write()), (2, crate::v2::MINOR.write())])),
            (4, RawValue::from("build")),
            (5, RawValue::Bytes(vec![1; 16])),
            (6, RawValue::from("Pixel")),
            (7, RawValue::from("android")),
            (8, RawValue::from("")),
            (9, RawValue::from("build")),
            (10, RawValue::Array(Vec::new())),
            (
                11,
                map([(1, true.write()), (2, RunIntent::Running.write()), (4, 0u64.write())]),
            ),
            (12, RawValue::Array(Vec::new())),
            (
                13,
                map([
                    (1, RawValue::Array(Vec::new())),
                    (2, CapturePermission::Indeterminate.write()),
                    (3, RawValue::Array(Vec::new())),
                    (4, RawValue::Array(Vec::new())),
                ]),
            ),
            (
                14,
                map([
                    (1, true.write()),
                    (3, 1f32.write()),
                    (4, false.write()),
                    (5, false.write()),
                    (6, false.write()),
                ]),
            ),
            (15, RawValue::Array(Vec::new())),
            (25, DeviceKind::Mobile.write()),
            (26, crate::v2::MAJOR.write()),
            (27, crate::v2::MINOR.write()),
        ]
        .into()
    }

    fn round_trip(raw: RawMap) -> DeviceFacts {
        let facts = DeviceFacts::from_raw(raw).unwrap();
        let bytes = frame::encode(&Message::ConfigFacts(facts), CONFIG_FRAME).unwrap();
        match frame::OpaqueFrame::parse_exact(&bytes, CONFIG_FRAME)
            .unwrap()
            .decode()
            .unwrap()
        {
            Message::ConfigFacts(facts) => facts,
            other => panic!("decoded {other:?}"),
        }
    }

    #[test]
    fn android_privileged_is_false_when_absent_and_round_trips_when_set() {
        assert_eq!(round_trip(android_facts()).android_privileged(), Ok(false));
        let mut raw = android_facts();
        raw.insert(32, true.write());
        assert_eq!(round_trip(raw).android_privileged(), Ok(true));
        let mut raw = android_facts();
        raw.insert(32, false.write());
        assert_eq!(round_trip(raw).android_privileged(), Ok(false));
    }

    #[test]
    fn android_privileged_must_be_a_bool() {
        let mut raw = android_facts();
        raw.insert(32, 1u64.write());
        assert!(DeviceFacts::from_raw(raw).is_err());
    }

    fn device(byte: u8) -> RawValue {
        map([(1, RawValue::from("ed25519")), (2, RawValue::Bytes(vec![byte; 32]))])
    }
    fn version(stamp: u64, author: u8) -> RawValue {
        map([(1, stamp.write()), (2, device(author))])
    }

    #[test]
    fn audio_config_round_trips_with_registers_and_device_switches() {
        let flag = |on: bool, stamp| map([(1, on.write()), (2, version(stamp, 9))]);
        let config = map([
            (1, map([(1, version(7 << 16, 2)), (2, device(2))])),
            (2, map([(1, version(5 << 16, 3))])),
            (
                3,
                RawValue::Array(vec![
                    map([(1, device(2)), (2, flag(true, 6 << 16)), (3, flag(false, 6 << 16))]),
                    map([(1, device(3)), (2, flag(true, 8 << 16))]),
                ]),
            ),
        ]);
        let mut raw = android_facts();
        raw.insert(33, config);
        let facts = round_trip(raw);
        let config = facts.audio_config().unwrap().expect("key 33");
        let exit = config.speaker_exit().unwrap().unwrap();
        assert_eq!(exit.version().unwrap().stamp().unwrap(), 7 << 16);
        assert!(exit.target().unwrap().is_some());
        assert!(config.microphone_source().unwrap().unwrap().target().unwrap().is_none());
        let devices = config.devices().unwrap().0;
        assert_eq!(devices.len(), 2);
        assert!(devices[0].forward().unwrap().unwrap().on().unwrap());
        assert!(!devices[0].receive().unwrap().unwrap().on().unwrap());
        assert!(
            devices[1].receive().unwrap().is_none(),
            "an unknown switch stays absent"
        );
        // A register version is never zero.
        let mut raw = android_facts();
        raw.insert(33, map([(1, map([(1, version(0, 2))]))]));
        assert!(DeviceFacts::from_raw(raw).is_err());
    }

    #[test]
    fn retired_forward_and_receive_options_and_summary_key_are_gone() {
        assert!(matches!(LocalOption::from_number(4), LocalOption::Unknown(4)));
        assert!(matches!(LocalOption::from_number(24), LocalOption::Unknown(24)));
        assert!(!LocalOptionsSummary::SCHEMA.fields.iter().any(|field| field.key == 2));
        // An earlier 0.4.0 build's summary with key 2 still decodes (retained, never read).
        let mut raw = android_facts();
        let RawValue::Map(summary) = raw.get_mut(&14).unwrap() else {
            unreachable!()
        };
        summary.insert(2, false.write());
        assert!(DeviceFacts::from_raw(raw).is_ok());
    }
}
