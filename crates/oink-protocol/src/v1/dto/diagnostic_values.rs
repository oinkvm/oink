//! Recursive safe diagnostic values. All identities are already pseudonymous;
//! these schemas never carry ordinary key codes or native process/window data.
use super::*;
use crate::v1::schema::{OpenEnvelope, enum_body, no_validation, open_enum, required, wire_map};

open_enum!(DiagOrigin { Physical=0,Synthetic=1,Remote=2,Injection=3 });
open_enum!(DiagCaptureDisposition { Pass=0,Swallow=1 });
open_enum!(DiagKeyClass { Ordinary=0,Function=1,LeftControl=2,LeftShift=3,LeftAlt=4,LeftMeta=5,RightControl=6,RightShift=7,RightAlt=8,RightMeta=9,Fn=10 });
open_enum!(DiagRawKeyEvent { Indeterminate=0,Down=1,Up=2,ModifierFlags=3,SystemFunction=4 });
open_enum!(DiagStateKind { CaptureMode=0,Injection=1,Layout=2,Permission=3,DisplayPower=4,Clipboard=5,Files=6,DragDrop=7,GameMode=8,Emergency=9,QueueLoss=10,HeldSync=11,SessionLock=12,RecorderTick=13,CapsLock=14,SecureInput=15 });
open_enum!(DiagPermissionState { Granted=0,Denied=1,NotDetermined=2,NotApplicable=3,NotGranted=4 });
wire_map!(DiagInputPermissions,no_validation,{
    1=>accessibility:DiagPermissionState=required;2=>input_monitoring:DiagPermissionState=required;3=>secure_input_active:bool=required;
});
wire_map!(DiagKeyObservation,no_validation,{
    1=>raw:DiagRawKeyEvent=required;2=>backend_generated:Option<bool>=optional;3=>press_id:Option<u64>=optional;4=>pressed_while_held:bool=required;
});
open_enum!(DiagMouseButtonKind { Left=0,Right=1,Middle=2,Back=3,Forward=4,Other=5 });
wire_map!(DiagOtherButton,no_validation,{1=>number:u8=required;});
enum_body!(DiagMouseButtonKind { Other=>DiagOtherButton });
pub type DiagMouseButton = OpenEnvelope<DiagMouseButtonKind>;
open_enum!(DiagScrollUnit { Pixel=0,Line=1 });
open_enum!(DiagGesturePhase { Began=0,Changed=1,Ended=2,Cancelled=3 });
open_enum!(DiagSwipeAxis { Horizontal=0,Vertical=1,Pinch=2 });
open_enum!(DiagGestureKind { DockSwipe=0,Magnify=1,Rotate=2,SmartMagnify=3 });
wire_map!(DiagDockSwipe,no_validation,{
    1=>axis:DiagSwipeAxis=required;2=>phase:DiagGesturePhase=required;3=>progress:f64=required;4=>velocity:f64=required;
});
wire_map!(DiagMagnify,no_validation,{1=>phase:DiagGesturePhase=required;2=>amount:f64=required;});
wire_map!(DiagRotate,no_validation,{1=>phase:DiagGesturePhase=required;2=>degrees:f64=required;});
enum_body!(DiagGestureKind { DockSwipe=>DiagDockSwipe,Magnify=>DiagMagnify,Rotate=>DiagRotate });
pub type DiagGesture = OpenEnvelope<DiagGestureKind>;
open_enum!(DiagSafeInputKind { ContactsOmitted=0,Motion=1,Position=2,Button=3,Scroll=4,Key=5,Gesture=6,ScrollDistance=7,RemoteMotion=8,RelativeTotals=9 });
wire_map!(DiagMotion,no_validation,{1=>dx:f64=required;2=>dy:f64=required;3=>raw:bool=required;});
wire_map!(DiagPosition,no_validation,{1=>at:Point=required;2=>delta:Option<Point>=optional;});
wire_map!(DiagButton,no_validation,{1=>button:DiagMouseButton=required;2=>pressed:bool=required;});
wire_map!(DiagScroll,no_validation,{1=>dx:f64=required;2=>dy:f64=required;3=>unit:DiagScrollUnit=required;4=>phase:ScrollPhase=required;});
wire_map!(DiagKey,no_validation,{
    1=>class:DiagKeyClass=required;2=>pressed:bool=required;3=>repeat:Option<bool>=optional;4=>diagnostic:DiagKeyObservation=required;
});
wire_map!(DiagGestureValue,no_validation,{1=>gesture:DiagGesture=required;});
wire_map!(DiagScrollDistance,no_validation,{1=>dx:f64=required;2=>dy:f64=required;3=>phase:ScrollPhase=required;});
wire_map!(DiagRemoteMotion,no_validation,{1=>seq:u64=required;2=>dx:f64=required;3=>dy:f64=required;});
wire_map!(DiagRelativeTotals,no_validation,{1=>epoch:u64=required;2=>generation:u64=required;3=>seq:u64=required;4=>x:f64=required;5=>y:f64=required;});
enum_body!(DiagSafeInputKind {
    Motion=>DiagMotion,Position=>DiagPosition,Button=>DiagButton,Scroll=>DiagScroll,Key=>DiagKey,Gesture=>DiagGestureValue,
    ScrollDistance=>DiagScrollDistance,RemoteMotion=>DiagRemoteMotion,RelativeTotals=>DiagRelativeTotals
});
pub type DiagSafeInput = OpenEnvelope<DiagSafeInputKind>;
wire_map!(DiagInput,no_validation,{
    1=>origin:DiagOrigin=required;2=>peer:Option<Text<128>>=optional;3=>original_us:Option<u64>=optional;4=>cursor:Option<Point>=optional;
    5=>event:DiagSafeInput=required;6=>success:Option<bool>=optional;7=>modifiers:Option<u16>=optional;8=>disposition:Option<DiagCaptureDisposition>=optional;
});
wire_map!(DiagLog,validate_log,{
    1=>module:Text<256>=required;2=>line:u32=required;3=>level:u8=required;4=>fields:List<NamedScalar,65535>=required;5=>labels:List<NamedScalar,65535>=required;
});
wire_map!(DiagClipboard,no_validation,{1=>kind_mask:u8=required;2=>bytes:u64=required;3=>outcome:u8=required;});
wire_map!(DiagFiles,no_validation,{1=>files:u64=required;2=>files_done:u64=required;3=>bytes:u64=required;4=>bytes_done:u64=required;5=>phase:u8=required;});
wire_map!(DiagGame,no_validation,{1=>holder:Text<128>=required;2=>epoch:u64=required;3=>active:bool=required;4=>detected:bool=required;5=>mode:GameMode=required;});
wire_map!(DiagFocus,no_validation,{1=>holder:Text<128>=required;2=>epoch:u64=required;3=>at:Option<Point>=optional;});
wire_map!(DiagState,no_validation,{1=>kind:DiagStateKind=required;2=>active:bool=required;3=>count:u64=required;4=>value:f64=required;});
wire_map!(DiagLink,no_validation,{
    1=>peer:Text<128>=required;2=>online:bool=required;3=>rtt_us:Option<u64>=optional;4=>sent_packets:Option<u64>=optional;5=>lost_packets:Option<u64>=optional;
});
wire_map!(DiagKeyboardAnomaly,no_validation,{1=>nonmodifier_flags:bool=required;});
wire_map!(DiagHeldSync,no_validation,{
    1=>peer:Text<128>=required;2=>source:Text<128>=required;3=>sent:bool=required;4=>keys:u64=required;5=>buttons:u64=required;
    6=>modifiers:u8=required;7=>press_ids:List<u64,8192>=required;8=>observations:u64=required;
});

open_enum!(DiagSourceKind {Mouse=0,Touchpad=1,Keyboard=2,Indeterminate=3});
open_enum!(DiagInputMode {Shared=0,Local=1});
open_enum!(DiagHandoffAction {FocusEnterSent=0,FocusEnterReceived=1,FocusStateSent=2,FocusStateReceived=3,FocusResumeSent=4,FocusResumeReceived=5,RoutingDecision=6,PairAttached=7,PairDetached=8,ModeRequested=9,ModeStarted=10,ModeCompleted=11,RestoreStarted=12,RestoreCompleted=13,MotionFlush=14,TrailActive=15,TrailDepart=16,TrailResume=17,TrailSent=18,TrailReceived=19});
open_enum!(DiagFlushReason {Due=0,OrderingBarrier=1,TargetChanged=2,FrequencyChanged=3,AttachmentBarrier=4});
open_enum!(DiagDecisionKind {Handoff=0,Refused=1,Lock=2,Unlock=3,ReturnHome=4,FocusAccepted=5,GapDepart=6,GapReturn=7});
open_enum!(DiagDecisionReason {EdgeCrossed=0,Locked=1,Paused=2,GameMode=3,HeldButtons=4,NotControllable=5,Resistance=6,PreviousInstance=7,Stale=8,Accepted=9,UserRequest=10,GapTravel=11,NoTarget=12});
open_enum!(DiagTrailStage {Source=0,Converted=1,Received=2,Resume=3});
open_enum!(DiagGestureStage {Capture=0,WindowsMapper=1,MacSynth=2});
open_enum!(DiagObservedGestureKind {Vertical=0,Horizontal=1,Pinch=2,Magnify=3});
open_enum!(DiagGestureDecision {Observed=0,Pending=1,TaskView=2,TaskViewClose=3,DesktopLeft=4,DesktopRight=5,ShowDesktop=6,BelowThreshold=7,WrongDirection=8,Cancelled=9,CtrlWheel=10,NativeDockControl=11,NativeMagnify=12,NativeCancel=13,SubmissionFailed=14});
open_enum!(DiagContactsPath {NativeContacts=0,CaptureUnavailable=1,AwaitLift=2,Local=3});
open_enum!(DiagVolumeAction {VolumeUp=0,VolumeDown=1,Mute=2,GainUp=3,GainDown=4,ForwardMute=5});
open_enum!(DiagVolumeOriginKind {Physical=0,Synthetic=1,Indeterminate=2});
open_enum!(DiagVolumeDestination {Source=0,Focus=1,Exit=2,Gain=3});
open_enum!(DiagPlacementKind {Owner=0,Tagged=1,NativeButton=2,NativeMotion=3});
open_enum!(DiagButtonLayer {HidTap=0,SessionTap=1,WindowsHook=2});
open_enum!(DiagTimestampUnit {Nanoseconds=0,Milliseconds=1});
open_enum!(DiagCallClass {Motion=0,Button=1,Key=2,Scroll=3,Gesture=4,Mode=5,Release=6,Other=7});
open_enum!(DiagFileProviderReason {Checking=0,DomainEnabled=1,UrlsReady=2,DomainDisabled=3,ExtensionNotRegistered=4,DomainDisconnected=5,RegistrationFailed=6,BoundExpired=7,Cancelled=8});
open_enum!(DiagPreparationPath {Spare=0,NewParent=1,DegradedPromise=2,Failed=3,Placeholder=4});
open_enum!(DiagPlaceholderFallback {Off=0,None=1,NotReady=2,Capacity=3,UnknownSize=4,PreparationFailed=5,PublicationFailed=6,DisabledDuringPublication=7,DeferredToEnter=8});
open_enum!(DiagProviderDragPath {Provider=0,DegradedPromise=1,Failed=2});
open_enum!(DiagNativeHoldEnd {Resume=0,DraggingExited=1,DraggingEnded=2,DropAccepted=3,DropRejected=4,CancelRepositioned=5,HoldRejected=6,HoldUnconfirmed=7});
open_enum!(DiagImageNudgeReason {InitialHide=0,ImageUpdate=1});
open_enum!(DiagOs {MacOS=0,Windows=1,Other=2});
open_enum!(DiagSourceInput {Motion=0,RawMotion=1,Button=2,Keyboard=3,Scroll=4,Gesture=5,Contacts=6});
open_enum!(DiagSourceDestination {Local=0,Forwarded=1,Consumed=2});
wire_map!(DiagDeltaSnapshot,no_validation,{1=>hid_location:Option<Point>=optional;2=>fold:Option<DiagFold>=optional;3=>warp:Option<DiagWarp>=optional;4=>drop_next:bool=required;});
wire_map!(DiagPlacement,no_validation,{1=>kind:DiagPlacementKind=required;2=>at:Point=required;3=>delta:Point=required;4=>timestamp_ns:u64=required;5=>source_state:i64=required;});
wire_map!(DiagPointerObservation,no_validation,{1=>remote:bool=required;2=>mode_cursor:Option<Point>=optional;3=>mode_timestamp_ns:u64=required;4=>placements:u64=required;5=>last_placement:Option<DiagPlacement>=optional;6=>event_position:Point=required;7=>event_timestamp_ns:u64=required;8=>source_state:i64=required;9=>raw_delta:Point=required;10=>corrected_delta:Point=required;11=>before:DiagDeltaSnapshot=required;12=>after:DiagDeltaSnapshot=required;});
wire_map!(DiagButtonSystem,no_validation,{1=>sampled_at_us:u64=required;2=>hid:Option<bool>=optional;3=>combined_session:Option<bool>=optional;4=>asynchronous:Option<bool>=optional;});
wire_map!(DiagTapDown,no_validation,{1=>event_timestamp_ns:u64=required;2=>source_state:Option<i64>=optional;3=>system:DiagButtonSystem=required;});
wire_map!(DiagTapUp,no_validation,{1=>available:bool=required;2=>generation:u64=required;3=>observing_since_ns:u64=required;4=>last_physical_up_ns:Option<u64>=optional;5=>last_external_up_ns:Option<u64>=optional;6=>matching_down:Option<DiagTapDown>=optional;7=>matching_up:Option<bool>=optional;});
wire_map!(DiagButtonSample,no_validation,{1=>system:DiagButtonSystem=required;2=>hid_tap:Option<DiagTapUp>=optional;3=>session_tap:Option<DiagTapUp>=optional;});
wire_map!(DiagButtonSourcePress,no_validation,{1=>physical:bool=required;2=>event_time_us:u64=required;});
wire_map!(DiagHeldButton,no_validation,{1=>source:Text<256>=required;2=>button:DiagMouseButton=required;3=>since_us:Option<u64>=optional;4=>held_for_us:Option<u64>=optional;5=>captured:Option<DiagButtonSourcePress>=optional;6=>holder_sample:DiagButtonSample=required;});
wire_map!(DiagHeldButtonsRefusal,no_validation,{1=>at_us:u64=required;2=>epoch:u64=required;3=>holder:Text<256>=required;4=>holds:List<DiagHeldButton,8192>=required;5=>omitted:u32=required;6=>dnd_leaving:bool=required;});
wire_map!(DiagHandoff,no_validation,{1=>action:DiagHandoffAction=required;2=>peer:Option<Text<256>>=optional;3=>holder:Option<Text<256>>=optional;4=>epoch:Option<u64>=optional;5=>router_instance:Option<Text<256>>=optional;6=>screen:Option<Text<256>>=optional;7=>at:Option<Point>=optional;8=>clip:Option<Rect>=optional;9=>mode:Option<DiagCaptureMode>=optional;10=>sequence:Option<u64>=optional;11=>success:Option<bool>=optional;12=>decision:Option<DiagDecisionKind>=optional;13=>reason:Option<DiagDecisionReason>=optional;14=>count:u64=required;15=>topology_sequence:u64=required;16=>sender:Option<Text<256>>=optional;17=>recipient:Option<Text<256>>=optional;18=>origin:Option<Text<256>>=optional;19=>next_target:Option<Text<256>>=optional;20=>flush_reason:Option<DiagFlushReason>=optional;21=>first_sequence:Option<u64>=optional;22=>message_identity:Option<Text<256>>=optional;23=>scope:List<DiagScopeMember,8192>=required;});
wire_map!(DiagTrailPoint,no_validation,{1=>age_secs:f64=required;2=>pos:Point=required;});
wire_map!(DiagTrail,no_validation,{1=>stage:DiagTrailStage=required;2=>topology_sequence:u64=required;3=>epoch:u64=required;4=>screen:Option<Text<256>>=optional;5=>anchor:Point=required;6=>scale:Point=required;7=>shown_secs:f64=required;8=>size_scale:f64=required;9=>native_units_per_reference_unit:f64=required;10=>emit_carry:f64=required;11=>heading:Point=required;12=>points:List<DiagTrailPoint,8192>=required;});
wire_map!(DiagDisplayBounds,no_validation,{1=>screen:Text<256>=required;2=>bounds:Rect=required;3=>pixel_width:u32=required;4=>pixel_height:u32=required;5=>pixels_per_unit:Point=required;});
wire_map!(DiagDisplays,no_validation,{1=>phase:Text<256>=required;2=>revision:Option<u64>=optional;3=>device:Text<256>=required;4=>topology_sequence:u64=required;5=>screens:List<DiagDisplayBounds,8192>=required;6=>truncated:u64=required;});
wire_map!(DiagPositionEvidence,no_validation,{1=>screen:Option<Text<256>>=optional;2=>at:Option<Point>=optional;3=>native_bounds:Option<Rect>=optional;4=>compiled_bounds:Option<Rect>=optional;});
wire_map!(DiagRouterState,no_validation,{1=>phase:Text<256>=required;2=>epoch:u64=required;3=>display_revision:u64=required;4=>topology_sequence:u64=required;5=>pointer:Option<DiagPositionEvidence>=optional;6=>exit:Option<DiagPositionEvidence>=optional;7=>reference:Option<DiagPositionEvidence>=optional;});
wire_map!(DiagGestureSummary,no_validation,{1=>kind:DiagObservedGestureKind=required;2=>phase:DiagGesturePhase=required;3=>identity:u64=required;4=>begin_identity:u64=required;5=>missing_begin:bool=required;6=>replaced:bool=required;7=>progress:f64=required;8=>velocity:f64=required;9=>peak_abs_progress:f64=required;10=>total_amount:f64=required;11=>duration_us:u64=required;12=>changed_events:u64=required;13=>ctrl_wheel_notches:u64=required;14=>ctrl_wheel_balance:i64=required;15=>decision:DiagGestureDecision=required;16=>threshold:Option<f64>=optional;17=>task_view_showing:Option<bool>=optional;});
wire_map!(DiagGestureEvent,no_validation,{1=>stage:DiagGestureStage=required;2=>source:Option<Text<256>>=optional;3=>summary:DiagGestureSummary=required;4=>contacts_path:Option<DiagContactsPath>=optional;5=>disposition:Option<DiagCaptureDisposition>=optional;});
wire_map!(DiagVolumeRouting,no_validation,{1=>action:DiagVolumeAction=required;2=>key_origin:DiagVolumeOriginKind=required;3=>source_kind:DiagSourceKind=required;4=>source:Text<256>=required;5=>focus:Text<256>=required;6=>decided_target:DiagVolumeDestination=required;7=>target_device:Text<256>=required;8=>reason:Text<256>=required;9=>focus_forwards:bool=required;10=>focus_keep_local_playback:Option<bool>=optional;11=>source_keep_local_playback:Option<bool>=optional;12=>windows_context:bool=required;13=>recent_default_endpoint_change:bool=required;14=>default_endpoint_age_ms:Option<u32>=optional;15=>recent_audio_arrival_removal:bool=required;16=>arrival_removal_age_ms:Option<u32>=optional;});
wire_map!(DiagPlaceholderTiming,no_validation,{1=>enabled:bool=required;2=>used:bool=required;3=>ready_parents:u32=required;4=>preparing:bool=required;5=>prepared_files:u32=required;6=>prepared_folders:u32=required;7=>used_files:u32=required;8=>used_folders:u32=required;9=>preparation_us:Option<u64>=optional;10=>final_urls_us:Option<u64>=optional;11=>fallback:DiagPlaceholderFallback=required;12=>failure:Option<DiagFileProviderReason>=optional;13=>cleanup_pending:u32=required;});
wire_map!(DiagPreparationTiming,no_validation,{1=>spare_ready:bool=required;2=>spare_preparation_us:Option<u64>=optional;3=>spare_failure:Option<DiagFileProviderReason>=optional;4=>registration_start_us:u64=required;5=>registration_us:u64=required;6=>parent_visible_us:Option<u64>=optional;7=>children_enumerated_us:Option<u64>=optional;8=>first_url_us:Option<u64>=optional;9=>native_begin_us:Option<u64>=optional;10=>placeholder:DiagPlaceholderTiming=required;});
wire_map!(DiagProviderAvailability,no_validation,{1=>available:bool=required;2=>reason:DiagFileProviderReason=required;});
wire_map!(DiagJitterDistribution,no_validation,{1=>count:u64=required;2=>min_us:u64=required;3=>median_upper_us:u64=required;4=>p95_upper_us:u64=required;5=>max_us:u64=required;6=>late:List<u64,5>=required;7=>percentile_overflow:bool=required;});
wire_map!(DiagJitterPath,no_validation,{1=>ingress:DiagJitterDistribution=required;2=>dispatch:DiagJitterDistribution=required;3=>baseline_resets:u64=required;4=>source_reordered:u64=required;});
wire_map!(DiagJitter,no_validation,{1=>reliable:DiagJitterPath=required;2=>lossy:DiagJitterPath=required;});
wire_map!(DiagInputTiming,no_validation,{1=>window_us:u64=required;2=>capture_callbacks:u64=required;3=>capture_callback_max_us:u64=required;4=>capture_os_wait_max_us:u64=required;5=>inject_completed:u64=required;6=>inject_queue_wait_max_us:u64=required;7=>inject_native_max_us:List<u64,8>=required;8=>oldest_pending_us:u64=required;9=>pending:u64=required;10=>native_inflight_class:Option<DiagCallClass>=optional;11=>native_inflight_age_us:u64=required;12=>jitter:DiagJitter=required;});
wire_map!(DiagDestinationSnapshot,no_validation,{1=>session_location:Option<Point>=optional;2=>destination_ours:bool=required;3=>catcher:Rect=required;4=>mouse_location:Point=required;5=>mouse_hit_ours:bool=required;6=>visible:bool=required;7=>level:i64=required;});
wire_map!(DiagCancelGeometry,no_validation,{1=>down:Option<Point>=optional;2=>target_plane:Option<Point>=optional;3=>projected:Option<Point>=optional;4=>edge:Option<Point>=optional;5=>fallback:Option<Point>=optional;});
wire_map!(DiagCancelObservation,no_validation,{1=>geometry:DiagCancelGeometry=required;2=>posted:Option<Point>=optional;3=>delivery_ack:Option<bool>=optional;4=>callback_count:u64=required;5=>last_callback:Option<Point>=optional;6=>ack_position:Option<Point>=optional;7=>ack_latency_us:Option<u64>=optional;8=>wait_us:Option<u64>=optional;});
wire_map!(DiagImageNudgePost,no_validation,{1=>at:Point=required;2=>button:DiagMouseButton=required;3=>flags:u64=required;});
wire_map!(DiagImageNudgeObservation,no_validation,{1=>generation:u64=required;2=>reason:DiagImageNudgeReason=required;3=>posted_us:u64=required;4=>post:DiagImageNudgePost=required;5=>image_update_us:Option<u64>=optional;});
wire_map!(DiagNativeHoldSummary,no_validation,{1=>operation:u64=required;2=>native_sequence:i64=required;3=>elapsed_us:u64=required;4=>end:DiagNativeHoldEnd=required;5=>entered:u64=required;6=>updated:u64=required;7=>image_updated:u64=required;8=>exited:u64=required;9=>ended:u64=required;10=>first_exit_us:Option<u64>=optional;11=>last_source_mask:u64=required;12=>last_returned_mask:u64=required;13=>last_is_own:bool=required;14=>returned_none:u64=required;15=>first_none_us:Option<u64>=optional;16=>initial_nil_items:u64=required;17=>update_nil_items:u64=required;18=>providers_on_update:u64=required;19=>show_calls:u64=required;20=>move_calls:u64=required;21=>start:DiagDestinationSnapshot=required;22=>first_image:Option<DiagDestinationSnapshot>=optional;23=>finish:DiagDestinationSnapshot=required;24=>cancel:Option<DiagCancelObservation>=optional;25=>image_nudges_posted:u64=required;26=>image_nudges:List<DiagImageNudgeObservation,8192>=required;});
wire_map!(DiagSourceRouting,no_validation,{1=>mouse_mode:DiagInputMode=required;2=>keyboard_mode:DiagInputMode=required;3=>input:DiagSourceInput=required;4=>destination:DiagSourceDestination=required;5=>swallowed:bool=required;});

open_enum!(DiagCaptureModeKind { Local=0,Remote=1 });
wire_map!(DiagRemoteCapture,no_validation,{1=>park:Point=required;});
enum_body!(DiagCaptureModeKind { Remote=>DiagRemoteCapture });
pub type DiagCaptureMode = OpenEnvelope<DiagCaptureModeKind>;
wire_map!(DiagScopeMember,no_validation,{1=>device:Text<128>=required;2=>instance:Text<128>=required;});
wire_map!(DiagFold,no_validation,{1=>delta:Point=required;2=>target:Point=required;});
wire_map!(DiagWarp,no_validation,{1=>target:Point=required;2=>timestamp_ns:u64=required;});
wire_map!(DiagButtonObservation,no_validation,{
    1=>layer:DiagButtonLayer=required;2=>button:DiagMouseButton=required;3=>pressed:bool=required;4=>event_timestamp:u64=required;
    5=>timestamp_unit:DiagTimestampUnit=required;6=>source_state:Option<i64>=optional;7=>physical:bool=required;8=>own:bool=required;9=>system:DiagButtonSystem=required;
});
wire_map!(DiagPointerTransition,no_validation,{1=>sequence:u64=required;2=>holder:Option<Text<128>>=optional;3=>epoch:Option<u64>=optional;4=>accounting:DiagPointerObservation=required;});
wire_map!(DiagButtonTransition,no_validation,{1=>sequence:u64=required;2=>holder:Option<Text<128>>=optional;3=>epoch:Option<u64>=optional;4=>observation:DiagButtonObservation=required;});
wire_map!(DiagProviderDrag,no_validation,{1=>path:DiagProviderDragPath=required;2=>reason:DiagFileProviderReason=required;});
wire_map!(DiagProviderPreparation,no_validation,{1=>drag:Text<128>=required;2=>epoch:u64=required;3=>path:DiagPreparationPath=required;4=>reason:DiagFileProviderReason=required;5=>timing:DiagPreparationTiming=required;6=>items:u64=required;7=>bytes:u64=required;});
wire_map!(DiagDndNativeHold,no_validation,{1=>drag:Text<128>=required;2=>physical:bool=required;3=>observation:DiagNativeHoldSummary=required;});
open_enum!(DiagDndSourceEffect {Hold=0,Anchor=1,Resume=2,Finish=3,Release=4,CancelReposition=5});
open_enum!(DiagDndNativeKind {Observed=0,HoldRequested=1,Held=2,Resumed=3,CancelRepositioning=4,ReleaseSubmitted=5,EndedAccepted=6,EndedRejected=7,CatcherLost=8,Exported=9,Unconfirmed=10,CancelRetired=11});
wire_map!(DiagDndUnconfirmed,no_validation,{1=>effect:DiagDndSourceEffect=required;});
enum_body!(DiagDndNativeKind {Unconfirmed=>DiagDndUnconfirmed});
pub type DiagDndNative = OpenEnvelope<DiagDndNativeKind>;
open_enum!(DiagRolePartition {AllSame=0,SourceInput=1,SourceTarget=2,InputTarget=3,Distinct=4});
open_enum!(DiagPhysicalObservation {Down=0,Up=1,Unobserved=2});
open_enum!(DiagIntegrityOutcome {Completed=0,Cancelled=1,CatcherLost=2,HoldRejected=3,EffectUnconfirmed=4,OriginalRouteReleased=5,ExportFailed=6});
open_enum!(DiagSourceDisposition {OriginalSourceOwns=0,ReleaseSubmitted=1,Released=2,ReturnedToOriginalRoute=3,Unconfirmed=4});
open_enum!(DiagDndSourcePhase {Local=0,Held=1,Released=2,Committing=3,Receiving=4,Cancelling=5,Closed=6});
wire_map!(DiagIntegrity,no_validation,{1=>roles:DiagRolePartition=required;2=>source_os:DiagOs=required;3=>input_os:DiagOs=required;4=>target_os:DiagOs=required;5=>holder_epoch:u64=required;6=>native:DiagDndNative=required;7=>physical:DiagPhysicalObservation=required;8=>disposition:DiagSourceDisposition=required;9=>route:DiagDndSourcePhase=required;10=>outcome:DiagIntegrityOutcome=required;});
wire_map!(DiagDndIntegrity,no_validation,{1=>drag:Text<128>=required;2=>record:DiagIntegrity=required;});
wire_map!(DiagSavedScreen,no_validation,{
    1=>device:Text<128>=required;2=>screen:Text<128>=required;3=>local:Rect=required;4=>plane:Rect=required;5=>size_source:SizeSource=required;
    6=>gain:f64=required;7=>enabled:bool=required;8=>primary:bool=required;9=>builtin:bool=required;
});
wire_map!(DiagDisplay,no_validation,{
    1=>screen:Text<128>=required;2=>bounds:Rect=required;4=>pixel_width:u32=required;5=>pixel_height:u32=required;
    6=>reference_extent:Option<Size>=optional;7=>rotation_deg:u16=required;8=>scale:f64=required;9=>primary:bool=required;10=>builtin:bool=required;11=>virtual_display:bool=required;
});
wire_map!(DiagDevice,no_validation,{1=>device:Text<128>=required;2=>saved_member:bool=required;3=>applied_member:bool=required;4=>online:bool=required;5=>input_capable:Option<bool>=optional;6=>facts_version:Option<u64>=optional;7=>displays:List<DiagDisplay,256>=required;});
wire_map!(DiagTopologyScreen,no_validation,{
    1=>device:Text<128>=required;2=>screen:Text<128>=required;3=>local:Rect=required;4=>plane:Rect=required;5=>gain:f64=required;6=>plane_gain:Option<f64>=optional;
    8=>reference_motion_per_native_unit:Option<f64>=optional;9=>size_source:SizeSource=required;10=>enabled:bool=required;11=>topology_active:bool=required;12=>input_online:bool=required;13=>active:bool=required;
});
wire_map!(DiagSourceScreen,no_validation,{1=>device:Text<128>=required;2=>screen:Text<128>=required;3=>reference_motion_per_native_unit:Option<f64>=optional;});
wire_map!(DiagSourceDevice,no_validation,{1=>device:Text<128>=required;2=>origin:Option<Point>=optional;3=>requires_target:bool=required;});
open_enum!(DiagSegmentKind {Wall=0,Portal=1});
wire_map!(DiagPortal,no_validation,{1=>device:Text<128>=required;2=>screen:Text<128>=required;3=>gap_distance:f64=required;});
enum_body!(DiagSegmentKind {Portal=>DiagPortal});
pub type DiagEdgeKind = OpenEnvelope<DiagSegmentKind>;
wire_map!(DiagEdgeSegment,no_validation,{1=>from:f64=required;2=>to:f64=required;3=>kind:DiagEdgeKind=required;});
wire_map!(DiagEdge,no_validation,{1=>device:Text<128>=required;2=>screen:Text<128>=required;3=>side:Side=required;4=>at:f64=required;5=>segments:List<DiagEdgeSegment,4096>=required;});
wire_map!(DiagScreenSummary,no_validation,{1=>device_alias:Text<128>=required;2=>screen_alias:Text<128>=required;3=>bounds:Rect=required;4=>plane:Rect=required;});
wire_map!(DiagSnapshotExcerpt,no_validation,{1=>configuration_available:bool=required;2=>layout_available:bool=required;3=>saved_members:Option<u64>=optional;4=>online_members:Option<u64>=optional;5=>omitted_details:List<Ascii<96>,16>=required;});
open_enum!(DiagExceptionClass {Panic=0,BadAccess=1,BadInstruction=2,Arithmetic=3,Breakpoint=4,Abort=5,Resource=6,Guard=7,Unhandled=8,Indeterminate=9});
wire_map!(DiagCrashSummary,no_validation,{1=>available:bool=required;2=>timestamp_ms:Option<i64>=optional;3=>exception_class:DiagExceptionClass=required;4=>faulting_thread:Option<u64>=optional;});
open_enum!(DiagButtonHandoff {Block=0,Allow=1});
wire_map!(DiagButtonHandoffValue,no_validation,{1=>mode:DiagButtonHandoff=required;});
wire_map!(DiagTopologySummary,no_validation,{1=>sequence:u64=required;2=>version:DiagVersion=required;3=>screens:List<DiagScreenSummary,256>=required;4=>detail_available:bool=required;});
wire_map!(DiagOwnerScreen,no_validation,{1=>screen_alias:Text<128>=required;2=>gain:Option<f64>=optional;3=>enabled:Option<bool>=optional;});
wire_map!(DiagOwnerValue,no_validation,{1=>scalars:List<NamedScalar,256>=required;2=>screens:List<DiagOwnerScreen,256>=required;3=>omitted:bool=required;});

/// A known record validates recursively. An unrecognised kind remains a retained
/// tree so an older collector can export it without inventing an interpretation.
pub fn diagnostic_event_schema(kind: &str) -> Option<crate::v1::schema::Schema> {
    use crate::v1::schema::WireDto;
    Some(match kind {
        "protocol-violation/1" => DiagnosticProtocolViolation::SCHEMA,
        "volume-refused/1" => VolumeRefused::SCHEMA,
        "input/1" => DiagInput::SCHEMA,
        "log/1" => DiagLog::SCHEMA,
        "clipboard/1" => DiagClipboard::SCHEMA,
        "files/1" => DiagFiles::SCHEMA,
        "game/1" => DiagGame::SCHEMA,
        "focus/1" => DiagFocus::SCHEMA,
        "state/1" => DiagState::SCHEMA,
        "link/1" => DiagLink::SCHEMA,
        "keyboard-anomaly/1" => DiagKeyboardAnomaly::SCHEMA,
        "input-lifecycle/1" => InputLifecycle::SCHEMA,
        "handoff/1" => DiagHandoff::SCHEMA,
        "topology/1" => DiagTopologySummary::SCHEMA,
        "layout-change/1" => LayoutChange::SCHEMA,
        "crossing/1" => Crossing::SCHEMA,
        "trail/1" => DiagTrail::SCHEMA,
        "displays/1" => DiagDisplays::SCHEMA,
        "router-state/1" => DiagRouterState::SCHEMA,
        "held-sync/1" => DiagHeldSync::SCHEMA,
        "gesture/1" => DiagGestureEvent::SCHEMA,
        "volume-routing/1" => DiagVolumeRouting::SCHEMA,
        "pointer-transition/1" => DiagPointerTransition::SCHEMA,
        "file-provider-availability/1" => DiagProviderAvailability::SCHEMA,
        "file-provider-drag/1" => DiagProviderDrag::SCHEMA,
        "file-provider-preparation/1" => DiagProviderPreparation::SCHEMA,
        "input-timing/1" => DiagInputTiming::SCHEMA,
        "dnd-native-hold/1" => DiagDndNativeHold::SCHEMA,
        "dnd-integrity/1" => DiagDndIntegrity::SCHEMA,
        "button-transition/1" => DiagButtonTransition::SCHEMA,
        "held-buttons-refusal/1" => DiagHeldButtonsRefusal::SCHEMA,
        "source-routing/1" => DiagSourceRouting::SCHEMA,
        _ => return None,
    })
}
pub(super) fn validate_event(raw: &crate::v1::RawMap) -> crate::v1::Result<()> {
    let kind = required::<Ascii<96>>(raw, 1)?;
    let fields = required::<crate::v1::RawMap>(raw, 2)?;
    if let Some(schema) = diagnostic_event_schema(&kind.0) {
        crate::v1::registry::validate_schema(schema.name, &fields)?;
    }
    Ok(())
}
pub(super) fn validate_scalar(raw: &crate::v1::RawMap) -> crate::v1::Result<()> {
    use crate::v1::RawValue;
    match required::<RawValue>(raw, 2)? {
        RawValue::Uint(_) | RawValue::Negative(_) | RawValue::Text(_) | RawValue::Bool(_) | RawValue::Null => Ok(()),
        RawValue::F32(n) if n.is_finite() => Ok(()),
        RawValue::F64(n) if n.is_finite() => Ok(()),
        _ => Err(crate::v1::ErrorCode::MalformedFrame.into()),
    }
}
pub(super) fn sorted_scalars(raw: &crate::v1::RawMap, key: u32) -> crate::v1::Result<()> {
    let values = required::<List<NamedScalar, 65535>>(raw, key)?;
    let mut previous = None;
    for value in values.0 {
        let name = value.name()?.0;
        if previous
            .as_ref()
            .is_some_and(|p: &String| p.as_bytes() >= name.as_bytes())
        {
            return Err(crate::v1::ErrorCode::MalformedFrame.into());
        }
        previous = Some(name);
    }
    Ok(())
}
fn validate_log(raw: &crate::v1::RawMap) -> crate::v1::Result<()> {
    sorted_scalars(raw, 4)?;
    sorted_scalars(raw, 5)
}
pub(super) fn validate_snapshot(raw: &crate::v1::RawMap) -> crate::v1::Result<()> {
    sorted_scalars(raw, 8)?;
    sorted_scalars(raw, 9)
}
