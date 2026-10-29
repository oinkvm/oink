//! Retained settings, geometry, input ownership, and diagnostic records.
//! Extensible values require their owning domain's validation before execution.
use super::core::validate_topology;
use super::*;
use crate::v1::RawMap;
use crate::v1::schema::{defaulted, no_validation, open_enum, required, wire_map};
wire_map!(ScreenId,no_validation,{1=>device:DeviceId=required;2=>display:Text<512>=required;});
wire_map!(DevicePlacement,no_validation,{
    1=>device:DeviceId=required;2=>origin:Point=required;3=>scale:f64=required;
    4=>screens:List<ScreenPlacement,256>=required;
});
wire_map!(ScreenPlacement,no_validation,{
    1=>id:ScreenId=required;2=>name:Text<256>=required;3=>local:Rect=required;
    5=>size_source:SizeSource=required;
    9=>primary:bool=required;10=>builtin:bool=required;
});
open_enum!(SizeSource {Reported=0,Estimated=1,Manual=3});
open_enum!(KeyPreset {WinFeel=0,MacFeel=1,Custom=2,None=3});
wire_map!(KeyMapping,no_validation,{1=>from:u32=required;2=>to:u32=required;});
wire_map!(CapsTapHold,validate_caps,{1=>tap:u32=required;2=>hold:u32=required;});
// Inert editor memory: old readers ignore KeyboardMapping field 4. Keep active
// outputs in fields 1-3 so preset execution stays compatible with those readers.
wire_map!(SavedCustomKeymap,no_validation,{
    1=>table:List<KeyMapping,256>=required;2=>caps_hold:Option<u32>=optional;
    3=>caps_split:bool=required;4=>allow_fn:bool=required;
});
wire_map!(KeyboardMapping,validate_keyboard_mapping,{
    1=>preset:KeyPreset=required;2=>table:List<KeyMapping,256>=required;3=>caps_tap_hold:Option<CapsTapHold>=optional;
    4=>saved_custom:Option<SavedCustomKeymap>=optional;
});
wire_map!(TargetTransform,no_validation,{1=>reverse_wheel:bool=required;2=>keyboard:KeyboardMapping=required;});
wire_map!(TargetOverride,no_validation,{1=>target:DeviceId=required;2=>reverse_wheel:Option<bool>=optional;3=>keyboard:Option<KeyboardMapping>=optional;});
wire_map!(ScreenPreferencesValue,validate_device_gain,{1=>screens:List<ScreenPreference,256>=required;2=>device_gain:f64=1.0;});
wire_map!(ScreenPreference,validate_screen_gain,{1=>display:Text<512>=required;2=>gain:f64=required;3=>enabled:bool=required;4=>remote_gain:f64=1.0;});
wire_map!(SourceInputPolicyValue,no_validation,{
    1=>reference_display:Option<Text<512>>=optional;2=>button_handoff:ButtonHandoff=required;
    3=>forward_gestures:bool=required;4=>return_pointer_on_display_sleep:bool=required;
    5=>wake_displays_on_input:bool=required;6=>dnd_enabled:bool=required;
});
wire_map!(ForwardingControlsValue,validate_forwarding_controls,{1=>volume_percent:u16=required;2=>muted:bool=required;});
open_enum!(ButtonHandoff {Block=0,Allow=1});
wire_map!(EffectiveSettings,validate_topology,{
    6=>gap_travel:bool=false;7=>overlap_tolerance:f64=(5.0 / 686.0);8=>minimum_portal:f64=(10.0 / 686.0);
    9=>portal_snap:f64=(8.0 / 686.0);10=>walls:bool=true;12=>endpoint_reach:f64=0.0;
});
wire_map!(DiagVersion,no_validation,{1=>counter:u64=required;3=>writer_alias:Text<128>=required;});
wire_map!(DiagSettings,no_validation,{1=>effective:EffectiveSettings=required;});
wire_map!(DiagOwnerPreference,no_validation,{
    1=>device_alias:Text<128>=required;2=>option:LocalOption=required;3=>revision:u64=required;4=>effective:DiagOwnerValue=required;
});
wire_map!(DiagLocalPause,no_validation,{1=>paused:bool=required;2=>reason:PauseReason=required;3=>revision:u64=required;});
wire_map!(CompleteLayout,no_validation,{
    1=>sequence:u64=required;2=>version:DiagVersion=required;
    4=>roster_digest:Hash32=required;5=>saved_members:List<Text<128>,16>=required;6=>applied_roster_digest:Hash32=required;
    7=>applied_version:DiagVersion=required;9=>source:LayoutSource=required;
    10=>paused:bool=required;11=>saved:List<DiagSavedScreen,256>=required;12=>settings:DiagSettings=required;
    14=>devices:List<DiagDevice,16>=required;15=>compiled:Compiled=required;
    16=>owner_preferences:List<DiagOwnerPreference,512>=required;17=>local_pause:DiagLocalPause=required;
});
wire_map!(Compiled,no_validation,{
    1=>current_group:bool=required;2=>stage:CompilationStage=required;3=>sequence:u64=required;
    4=>version:DiagVersion=required;6=>settings:EffectiveSettings=required;
    7=>screens:List<DiagTopologyScreen,256>=required;8=>sources:List<DiagSourceScreen,256>=required;9=>source_devices:List<DiagSourceDevice,16>=required;
    10=>input_online:Option<List<Text<128>,16>>=optional;11=>edges:List<DiagEdge,4096>=required;
});
wire_map!(LayoutChange,no_validation,{1=>kinds:List<ChangeKind,10>=required;2=>before:Option<CompleteLayout>=optional;3=>after:CompleteLayout=required;});
open_enum!(ChangeKind {Initialized=0,GroupChanged=1,Arrangement=2,Settings=4,DisplayFacts=5,Membership=6,Availability=7,CompiledTopology=8,Applied=9});
open_enum!(CompilationStage {Prepared=0,Router=1});
wire_map!(Crossing,no_validation,{
    1=>topology_sequence:u64=required;2=>epoch:u64=required;3=>from_screen:Option<Text<128>>=optional;4=>from:Point=required;
    5=>target_screen:Option<Text<128>>=optional;6=>final_position:Point=required;7=>delta:Point=required;
    8=>delta_is_local:bool=required;9=>gap_travel:bool=required;10=>first_exit:Option<Point>=optional;
    11=>first_entry:Option<Point>=optional;12=>first_target_screen:Option<Text<128>>=optional;
    13=>side:Option<Side>=optional;14=>gap_distance:Option<f64>=optional;15=>remaining_reference_motion:Option<Point>=optional;16=>departure_clamp:Option<DepartureClamp>=optional;
    17=>trigger_zone:bool=false;
});
open_enum!(Side {Left=0,Top=1,Right=2,Bottom=3});
wire_map!(Snapshot,super::diagnostic_values::validate_snapshot,{
    1=>group_state:DiagSnapshotExcerpt=required;2=>version:Text<256>=required;3=>source_tree_sha256:Text<128>=required;
    4=>os:Text<256>=required;5=>os_version:Text<256>=required;6=>screens:List<DiagScreenSummary,256>=required;
    7=>permissions:Option<DiagInputPermissions>=optional;8=>settings:List<NamedScalar,65535>=required;9=>routing_numbers:List<NamedScalar,65535>=required;
    10=>button_handoff:Option<DiagButtonHandoffValue>=optional;11=>reference_screen:Option<Text<128>>=optional;12=>injection_failures:u64=required;
    13=>session_locked:Option<bool>=optional;14=>backend_uiaccess:Option<bool>=optional;15=>crash_report:DiagCrashSummary=required;
    16=>layout:Option<CompleteLayout>=optional;
    17=>application_focus:Option<DiagApplicationFocus>=optional;
});
wire_map!(NamedScalar,super::diagnostic_values::validate_scalar,{1=>name:Text<256>=required;2=>value:crate::v1::RawValue=required;});
wire_map!(DiagnosticEvent,super::diagnostic_values::validate_event,{1=>kind:Ascii<96>=required;2=>fields:RawMap=required;});
// Exported protocol violations contain collection-local aliases in identity
// positions. Raw public keys, Boot nonces, payloads and human error text do not
// belong to this DTO. Recording never grants permission to upload diagnostics.
wire_map!(DiagnosticProtocolViolation,no_validation,{
    1=>code:u16=required;2=>scope:ErrorScope=required;3=>peer_alias:Text<128>=required;
    4=>boot_alias:Text<128>=required;5=>path_alias:Option<Text<128>>=optional;
    6=>kind:Option<u16>=optional;7=>type_id:Option<u16>=optional;
    8=>capability:Option<Ascii<96>>=optional;9=>experimental:bool=required;
    10=>isolation:Isolation=required;11=>at_us:u64=required;12=>suppressed:u64=required;
});
wire_map!(ClockStamp,no_validation,{1=>mono_us:u64=required;2=>utc_us:i64=required;3=>clock_id:i64=required;});
wire_map!(ClockSample,no_validation,{
    1=>device:Text<128>=required;2=>instance:Text<128>=required;3=>at_us:i64=required;4=>source_clock:i64=required;
    5=>local_clock:i64=required;6=>local_instance:Text<128>=required;7=>first_source_mono_us:u64=required;
    8=>last_source_mono_us:u64=required;9=>offset_us:i64=required;10=>uncertainty_us:u64=required;
});
wire_map!(DiagnosticRow,no_validation,{1=>mono_us:u64=required;2=>clock_id:i64=required;3=>utc_us:i64=required;4=>event:DiagnosticEvent=required;});
wire_map!(DiagnosticSegment,no_validation,{
    1=>schema:u32=required;2=>device:Text<128>=required;3=>instance:Text<128>=required;4=>seq:u64=required;
    5=>dropped:u64=required;6=>snapshot:Snapshot=required;7=>clock_samples:List<ClockSample,65535>=required;8=>rows:List<DiagnosticRow,65535>=required;
});
wire_map!(Coverage,no_validation,{1=>start_us:i64=required;2=>end_us:i64=required;3=>complete:bool=required;4=>failure:Option<u16>=optional;});
wire_map!(InputLifecycle,no_validation,{
    1=>at_us:u64=required;2=>sequence:u64=required;3=>kind:LifecycleKind=required;4=>holder_alias:Option<Text<128>>=optional;
    5=>epoch:Option<u64>=optional;6=>observation:LifecycleObservation=required;
});
wire_map!(LifecycleObservation,no_validation,{
    1=>success:Option<bool>=optional;2=>reason:Option<u32>=optional;3=>remote:Option<bool>=optional;4=>hidden:Option<bool>=optional;
    5=>duration_us:Option<u64>=optional;6=>count:Option<u64>=optional;7=>native_error:Option<u32>=optional;
    8=>generation:Option<u64>=optional;9=>requested:Option<Point>=optional;10=>observed:Option<Point>=optional;11=>clip:Option<Rect>=optional;
    12=>cursor_showing:Option<bool>=optional;13=>park:Option<Point>=optional;14=>marker:Option<u64>=optional;
    15=>normalized:Option<Point>=optional;16=>desktop:Option<Rect>=optional;17=>chain_result:Option<i64>=optional;
    18=>cursor_before:Option<Point>=optional;19=>cursor_after_move:Option<Point>=optional;
    20=>hidden_before:Option<bool>=optional;21=>steps:Option<u32>=optional;
});
open_enum!(LifecycleKind {
    HookInstall=4,HookRemove=5,HookReinstall=6,Mode=7,Hide=8,Show=9,Park=10,
    ClipApply=11,ClipChanged=12,Registration=13,ModeRoute=17,ModeRequest=18,
    CoverRequest=19,CoverApplied=20,CoverCursor=21,CoverReady=22,ParkCommitted=23,
    UncoverRequest=24,UncoverCommitted=25,ModeDecision=26,InputDesktop=27,Gesture=28,
    PointerTransition=29,ButtonTransition=30,PointerAuthority=31,
    ParkInputSenderUnavailable=32,ParkInputSendRejected=33,ParkInputHookNotObserved=34,
    ParkInputTargetMismatch=35,ParkInputHideLost=36,ParkInputHookChainRejected=37,
    ParkInputReadbackMismatch=38,ParkInputCompletionUnavailable=39,
    ContactCapture=40,ContactInjection=41,
    MarkerSubmission=42,MarkerHookDecision=43,MarkerChainResult=44,MarkerReadback=45,RestoreSafetyReset=46,MouseKeysCompensation=47
});
wire_map!(DepartureClamp,no_validation,{1=>requested:Point=required;2=>applied:Point=required;3=>bounds:Rect=required;4=>display_revision:u64=required;});
wire_map!(FeelProfile,no_validation,{
    1=>key_repeat_delay_ms:u32=required;2=>key_repeat_interval_ms:u32=required;
    3=>wheel_lines_per_notch:Option<f64>=optional;4=>natural_scroll_mouse:bool=required;5=>natural_scroll_touchpad:bool=required;
});
wire_map!(FocusScope,no_validation,{1=>members:List<FocusMember,16>=required;});
wire_map!(FocusMember,no_validation,{1=>device:DeviceId=required;2=>instance:Id16=required;});
wire_map!(FocusLockRecord,no_validation,{
    1=>revision:u64=required;2=>author:DeviceId=required;3=>epoch:u64=required;
    4=>holder:DeviceId=required;5=>session:Id16=required;6=>scope:FocusScope=required;7=>locked:bool=required;
});

fn validate_forwarding_controls(raw: &RawMap) -> crate::v1::Result<()> {
    if crate::v1::schema::required::<u16>(raw, 1)? > 200 {
        return Err(crate::v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

fn validate_keyboard_mapping(raw: &RawMap) -> crate::v1::Result<()> {
    if defaulted::<Option<CapsTapHold>>(raw, 3, || None)?.is_some()
        && (required::<KeyPreset>(raw, 1)? != KeyPreset::Custom
            || required::<List<KeyMapping, 256>>(raw, 2)?
                .0
                .iter()
                .any(|m| m.from() == Ok(oink_foundation::input::KeyCode::CAPS_LOCK.0)))
    {
        return Err(crate::v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
fn validate_device_gain(raw: &RawMap) -> crate::v1::Result<()> {
    if !(0.1..=5.0).contains(&defaulted::<f64>(raw, 2, || 1.0)?) {
        return Err(crate::v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

fn validate_caps(raw: &RawMap) -> crate::v1::Result<()> {
    let supported = oink_foundation::input_transform::candidate_keys();
    for key in [1, 2] {
        if required::<u32>(raw, key)? != 0
            && !supported.contains(&oink_foundation::input::KeyCode(required::<u32>(raw, key)?))
        {
            return Err(crate::v1::ErrorCode::UnsupportedValue.into());
        }
    }
    Ok(())
}
fn validate_screen_gain(raw: &RawMap) -> crate::v1::Result<()> {
    if !(0.1..=5.0).contains(&required::<f64>(raw, 2)?) || !(0.1..=5.0).contains(&defaulted::<f64>(raw, 4, || 1.0)?) {
        return Err(crate::v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

impl LayoutValue {
    /// Flatten OS screen records for identity-only consumers. Canvas geometry
    /// requires the enclosing DevicePlacement's native-origin transform.
    pub fn screens(&self) -> crate::v1::Result<List<ScreenPlacement, 256>> {
        let screens = self
            .devices()?
            .0
            .into_iter()
            .map(|d| d.screens().map(|s| s.0))
            .collect::<crate::v1::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if screens.len() > 256 {
            return Err(crate::v1::ErrorCode::LimitExceeded.into());
        }
        Ok(List(screens))
    }
}

wire_map!(DiagApplicationFocus,no_validation,{
    1=>pointer:Text<128>=required;2=>keyboard:Option<Text<128>>=optional;
    3=>epoch:u64=required;4=>revision:u64=required;
    5=>phase:ApplicationFocusPhase=required;6=>reason:ApplicationFocusReason=required;
});
