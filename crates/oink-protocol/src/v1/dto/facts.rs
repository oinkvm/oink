use super::*;
use crate::v1::hot::hot_map;
use crate::v1::schema::{defaulted, no_validation, open_enum, required, wire_map};
use crate::v1::{ErrorCode, RawMap, RawValue, Result};

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
    20 => audio_exit_claim: Option<AudioExitClaim> = optional;
    21 => audio_claim_watermark: Option<AudioExitClaim> = optional;
    22 => native_arrangement_token: Option<Text<128>> = optional;
    // oink.input.accessibility/1: absent means unknown or not applicable.
    23 => accessibility_granted: Option<bool> = optional;
    24 => microphone_claim_watermark: Option<AudioExitClaim> = optional;
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
    2 => forwarding_enabled: bool = required;
    3 => forwarding_volume: f32 = required;
    4 => input_locked: bool = required;
    5 => keyboard_disabled: bool = required;
    6 => screen_local_input: bool = required;
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
    4 => exit_claim: ExitClaimStamp = required;
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
    if let Some(claim) = defaulted::<Option<AudioExitClaim>>(raw, 20, || None)?
        && claim.stamp()?.owner()? != required::<Boot>(raw, 1)?.device_id()?
    {
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
    if RawValue::Map(raw.clone()).encoded_len()? > crate::v1::limits::RESOURCE_FACTS {
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
    4=>output_generation:u64=required;5=>revision:u64=required;6=>exit_claim:ExitClaimStamp=required;
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
open_enum!(LocalOption {MouseMode=1,KeyboardMode=2,ClipboardEnabled=3,ForwardingEnabled=4,KeyboardDisabled=5,
    ScreenShared=6,ScreenPreferences=7,SpeedMode=8,CursorTrail=10,CursorTrailDuration=11,
    RemoteScrollSpeed=12,InputTransform=13,SourceInputPolicy=14,ForwardingControls=15,DeviceName=16,GameModePreference=18,AcceptContacts=19,AudioExitClaim=20,SyncSystemArrangement=21,TouchpadInjection=26,NativeMultifinger=27,MacScrollFeel=28,MicrophoneShareClaim=22,MicrophoneControls=23,MicrophoneReceiveMuted=24,MicrophoneInput=25});
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

// The stamp is global ordering data, but the enclosing fact/option has one
// authenticated writer. Withdrawals retain their stamp to prevent fallback.
hot_map!(ExitClaimStamp,validate_claim_stamp,{
    1=>counter:u64=required;2=>owner:DeviceId=required;
});
wire_map!(AudioExitClaim,no_validation,{
    1=>stamp:ExitClaimStamp=required;2=>claimed:bool=required;
});
wire_map!(GameObservation,no_validation,{1=>active:bool=required;2=>detected:bool=required;});
wire_map!(PauseDevice,no_validation,{
    1=>request_id:Id16=required;2=>target:Boot=required;3=>paused:bool=required;4=>expected_revision:u64=required;
});
wire_map!(PauseDeviceResult,no_validation,{
    1=>request_id:Id16=required;2=>target:Boot=required;3=>outcome:ControlOutcome=required;
    4=>reason:ControlFailure=required;5=>current:Option<LocalPauseState>=optional;6=>fact_sequence:Option<u64>=optional;
});
pub(super) fn validate_claim_stamp(raw: &RawMap) -> Result<()> {
    if required::<u64>(raw, 1)? == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
