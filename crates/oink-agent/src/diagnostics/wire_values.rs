//! Explicit business-to-wire projections; the protocol crate alone encodes CBOR.
use super::privacy::{CaptureDisposition, KeyClass, KeyDiagnostic};
use super::*;
use oink_foundation::{
    Point, Rect, Size,
    input::{Gesture, GesturePhase, MouseButton, ScrollPhase, ScrollUnit, SwipeAxis},
};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto as w,
    schema::{EnumSchema, OpenEnvelope, WireDto, WireValue},
};

pub(super) trait Value {
    fn wire(&self) -> v1::Result<RawValue>;
}
macro_rules! scalar {
    ($($t:ty),*) => { $(impl Value for $t { fn wire(&self)->v1::Result<RawValue> { Ok(self.write()) } })* };
}
scalar!(bool, u8, u16, u32, u64, i32, i64, f32, f64);
impl Value for usize {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(RawValue::Uint(*self as u64))
    }
}
impl Value for String {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(RawValue::Text(self.clone()))
    }
}
impl<T: Value> Value for Option<T> {
    fn wire(&self) -> v1::Result<RawValue> {
        self.as_ref().map_or(Ok(RawValue::Null), Value::wire)
    }
}
impl<T: Value> Value for Vec<T> {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(RawValue::Array(
            self.iter().map(Value::wire).collect::<v1::Result<_>>()?,
        ))
    }
}
impl<const N: usize> Value for [u64; N] {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(RawValue::Array(
            self.iter().map(Value::wire).collect::<v1::Result<_>>()?,
        ))
    }
}
impl<T: Value> Value for Box<T> {
    fn wire(&self) -> v1::Result<RawValue> {
        (**self).wire()
    }
}
impl Value for (f64, f64) {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::Point>([("x", self.0.wire()?), ("y", self.1.wire()?)])?.write())
    }
}
pub(super) fn dto<T: WireDto>(values: impl IntoIterator<Item = (&'static str, RawValue)>) -> v1::Result<T> {
    T::from_raw(raw_fields::<T>(values)?)
}
// Assemble newly projected children without copying them into intermediate
// DTOs. The owning top-level DTO recursively validates the entire tree once.
fn raw_fields<T: WireDto>(values: impl IntoIterator<Item = (&'static str, RawValue)>) -> v1::Result<RawMap> {
    let mut raw = RawMap::new();
    for (name, value) in values {
        let field = T::SCHEMA
            .fields
            .iter()
            .find(|f| f.name == name)
            .ok_or(v1::ErrorCode::UnsupportedValue)?;
        if raw.insert(field.key, value).is_some() {
            return Err(v1::ErrorCode::MalformedFrame.into());
        }
    }
    Ok(raw)
}
fn envelope<K: EnumSchema>(kind: K, body: RawMap) -> v1::Result<RawValue> {
    Ok(RawValue::Map(
        OpenEnvelope::<K>::from_raw([(1, kind.write()), (2, RawValue::Map(body))].into())?.into_raw(),
    ))
}
macro_rules! structure {
    ($domain:ty=>$wire:ty {$($field:ident),*$(,)?}) => {
        impl Value for $domain { fn wire(&self)->v1::Result<RawValue> {
            $(let _ = <$wire>::$field;)*
            Ok(RawValue::Map(dto::<$wire>([$((stringify!($field),self.$field.wire()?),)*])?.into_raw()))
        } }
    };
}
macro_rules! unit {
    ($domain:path=>$wire:path {$($from:ident=>$to:ident),*$(,)?})=>{
        impl Value for $domain { fn wire(&self)->v1::Result<RawValue> {
            use $domain as D; use $wire as W;
            Ok(match self { $(D::$from=>W::$to,)* }.write())
        } }
    };
}
structure!(Point=>w::Point{x,y});
structure!(Rect=>w::Rect{x,y,w,h});
structure!(Size=>w::Size{w,h});
structure!(KeyDiagnostic=>w::DiagKeyObservation{raw,backend_generated,press_id,pressed_while_held});
unit!(Origin=>w::DiagOrigin{Physical=>Physical,Synthetic=>Synthetic,Remote=>Remote,Injection=>Injection});
unit!(CaptureDisposition=>w::DiagCaptureDisposition{Pass=>Pass,Swallow=>Swallow});
unit!(KeyClass=>w::DiagKeyClass{Ordinary=>Ordinary,Function=>Function,LeftControl=>LeftControl,LeftShift=>LeftShift,LeftAlt=>LeftAlt,LeftMeta=>LeftMeta,RightControl=>RightControl,RightShift=>RightShift,RightAlt=>RightAlt,RightMeta=>RightMeta,Fn=>Fn});
unit!(oink_input::diagnostics::RawKeyEvent=>w::DiagRawKeyEvent{Unknown=>Indeterminate,Down=>Down,Up=>Up,ModifierFlags=>ModifierFlags,SystemFunction=>SystemFunction});
unit!(ScrollUnit=>w::DiagScrollUnit{Pixel=>Pixel,Line=>Line});
unit!(ScrollPhase=>w::ScrollPhase{None=>None,Began=>Began,Changed=>Changed,Ended=>Ended,Cancelled=>Cancelled,MomentumBegan=>MomentumBegan,Momentum=>Momentum,MomentumEnded=>MomentumEnded});
unit!(GesturePhase=>w::DiagGesturePhase{Began=>Began,Changed=>Changed,Ended=>Ended,Cancelled=>Cancelled});
unit!(SwipeAxis=>w::DiagSwipeAxis{Horizontal=>Horizontal,Vertical=>Vertical,Pinch=>Pinch});
unit!(StateKind=>w::DiagStateKind{CaptureMode=>CaptureMode,Injection=>Injection,Layout=>Layout,Permission=>Permission,DisplayPower=>DisplayPower,Clipboard=>Clipboard,Files=>Files,DragDrop=>DragDrop,GameMode=>GameMode,Emergency=>Emergency,QueueLoss=>QueueLoss,HeldSync=>HeldSync,SessionLock=>SessionLock,RecorderTick=>RecorderTick,CapsLock=>CapsLock,SecureInput=>SecureInput});
unit!(oink_routing::GameMode=>w::GameMode{Auto=>Auto,On=>On,Off=>Off});
impl Value for MouseButton {
    fn wire(&self) -> v1::Result<RawValue> {
        use w::DiagMouseButtonKind as K;
        let (kind, body) = match self {
            Self::Left => (K::Left, RawMap::new()),
            Self::Right => (K::Right, RawMap::new()),
            Self::Middle => (K::Middle, RawMap::new()),
            Self::Back => (K::Back, RawMap::new()),
            Self::Forward => (K::Forward, RawMap::new()),
            Self::Other(number) => (
                K::Other,
                dto::<w::DiagOtherButton>([("number", number.wire()?)])?.into_raw(),
            ),
        };
        envelope(kind, body)
    }
}
impl Value for Gesture {
    fn wire(&self) -> v1::Result<RawValue> {
        use w::DiagGestureKind as K;
        let (kind, body) = match self {
            Self::DockSwipe {
                axis,
                phase,
                progress,
                velocity,
            } => (
                K::DockSwipe,
                dto::<w::DiagDockSwipe>([
                    ("axis", axis.wire()?),
                    ("phase", phase.wire()?),
                    ("progress", progress.wire()?),
                    ("velocity", velocity.wire()?),
                ])?
                .into_raw(),
            ),
            Self::Magnify { phase, amount } => (
                K::Magnify,
                dto::<w::DiagMagnify>([("phase", phase.wire()?), ("amount", amount.wire()?)])?.into_raw(),
            ),
            Self::Rotate { phase, degrees } => (
                K::Rotate,
                dto::<w::DiagRotate>([("phase", phase.wire()?), ("degrees", degrees.wire()?)])?.into_raw(),
            ),
            Self::SmartMagnify => (K::SmartMagnify, RawMap::new()),
        };
        envelope(kind, body)
    }
}
impl Value for SafeInput {
    fn wire(&self) -> v1::Result<RawValue> {
        use w::DiagSafeInputKind as K;
        let (kind, body) = match self {
            Self::ContactsOmitted => return Err(v1::ErrorCode::UnsupportedValue.into()),
            Self::Motion { dx, dy, raw } => (
                K::Motion,
                dto::<w::DiagMotion>([("dx", dx.wire()?), ("dy", dy.wire()?), ("raw", raw.wire()?)])?.into_raw(),
            ),
            Self::Position { at, delta } => (
                K::Position,
                dto::<w::DiagPosition>([("at", at.wire()?), ("delta", delta.wire()?)])?.into_raw(),
            ),
            Self::Button { button, pressed } => (
                K::Button,
                dto::<w::DiagButton>([("button", button.wire()?), ("pressed", pressed.wire()?)])?.into_raw(),
            ),
            Self::Scroll { dx, dy, unit, phase } => (
                K::Scroll,
                dto::<w::DiagScroll>([
                    ("dx", dx.wire()?),
                    ("dy", dy.wire()?),
                    ("unit", unit.wire()?),
                    ("phase", phase.wire()?),
                ])?
                .into_raw(),
            ),
            Self::Key {
                class,
                pressed,
                repeat,
                diagnostic,
            } => (
                K::Key,
                dto::<w::DiagKey>([
                    ("class", class.wire()?),
                    ("pressed", pressed.wire()?),
                    ("repeat", repeat.wire()?),
                    ("diagnostic", diagnostic.wire()?),
                ])?
                .into_raw(),
            ),
            Self::Gesture(gesture) => (
                K::Gesture,
                dto::<w::DiagGestureValue>([("gesture", gesture.wire()?)])?.into_raw(),
            ),
            Self::ScrollDistance { dx, dy, phase } => (
                K::ScrollDistance,
                dto::<w::DiagScrollDistance>([("dx", dx.wire()?), ("dy", dy.wire()?), ("phase", phase.wire()?)])?
                    .into_raw(),
            ),
            Self::RemoteMotion { seq, dx, dy } => (
                K::RemoteMotion,
                dto::<w::DiagRemoteMotion>([("seq", seq.wire()?), ("dx", dx.wire()?), ("dy", dy.wire()?)])?.into_raw(),
            ),
            Self::RelativeTotals {
                epoch,
                generation,
                seq,
                x,
                y,
            } => (
                K::RelativeTotals,
                dto::<w::DiagRelativeTotals>([
                    ("epoch", epoch.wire()?),
                    ("generation", generation.wire()?),
                    ("seq", seq.wire()?),
                    ("x", x.wire()?),
                    ("y", y.wire()?),
                ])?
                .into_raw(),
            ),
        };
        envelope(kind, body)
    }
}

unit!(oink_foundation::input::SourceKind=>w::DiagSourceKind{Mouse=>Mouse,Touchpad=>Touchpad,Keyboard=>Keyboard,Unknown=>Indeterminate});
impl Value for oink_foundation::input::InputMode {
    fn wire(&self) -> v1::Result<RawValue> {
        use oink_foundation::input::InputMode;
        Ok(match self {
            InputMode::Shared => w::DiagInputMode::Shared,
            InputMode::Local => w::DiagInputMode::Local,
            InputMode::Disabled => w::DiagInputMode::Unknown(2),
        }
        .write())
    }
}
unit!(super::handoff::HandoffAction=>w::DiagHandoffAction{FocusEnterSent=>FocusEnterSent,FocusEnterReceived=>FocusEnterReceived,FocusStateSent=>FocusStateSent,FocusStateReceived=>FocusStateReceived,FocusResumeSent=>FocusResumeSent,FocusResumeReceived=>FocusResumeReceived,RoutingDecision=>RoutingDecision,PairAttached=>PairAttached,PairDetached=>PairDetached,ModeRequested=>ModeRequested,ModeStarted=>ModeStarted,ModeCompleted=>ModeCompleted,RestoreStarted=>RestoreStarted,RestoreCompleted=>RestoreCompleted,MotionFlush=>MotionFlush,TrailActive=>TrailActive,TrailDepart=>TrailDepart,TrailResume=>TrailResume,TrailSent=>TrailSent,TrailReceived=>TrailReceived});
unit!(super::handoff::FlushReason=>w::DiagFlushReason{Due=>Due,OrderingBarrier=>OrderingBarrier,TargetChanged=>TargetChanged,FrequencyChanged=>FrequencyChanged,AttachmentBarrier=>AttachmentBarrier});
unit!(oink_routing::router::DecisionKind=>w::DiagDecisionKind{Handoff=>Handoff,Refused=>Refused,Lock=>Lock,Unlock=>Unlock,ReturnHome=>ReturnHome,FocusAccepted=>FocusAccepted,GapDepart=>GapDepart,GapReturn=>GapReturn});
unit!(oink_routing::router::DecisionReason=>w::DiagDecisionReason{EdgeCrossed=>EdgeCrossed,Locked=>Locked,Paused=>Paused,GameMode=>GameMode,HeldButtons=>HeldButtons,NotControllable=>NotControllable,Resistance=>Resistance,PreviousInstance=>PreviousInstance,Stale=>Stale,Accepted=>Accepted,UserRequest=>UserRequest,GapTravel=>GapTravel,NoTarget=>NoTarget});
unit!(super::handoff::TrailStage=>w::DiagTrailStage{Source=>Source,Converted=>Converted,Received=>Received,Resume=>Resume});
unit!(oink_input::gesture_trace::Stage=>w::DiagGestureStage{Capture=>Capture,WindowsMapper=>WindowsMapper,MacSynth=>MacSynth});
unit!(oink_input::gesture_trace::GestureKind=>w::DiagObservedGestureKind{Vertical=>Vertical,Horizontal=>Horizontal,Pinch=>Pinch,Magnify=>Magnify});
unit!(oink_input::gesture_trace::Decision=>w::DiagGestureDecision{Observed=>Observed,Pending=>Pending,TaskView=>TaskView,TaskViewClose=>TaskViewClose,DesktopLeft=>DesktopLeft,DesktopRight=>DesktopRight,ShowDesktop=>ShowDesktop,RestoreDesktop=>RestoreDesktop,DesktopNoop=>DesktopNoop,StartOpen=>StartOpen,StartClose=>StartClose,StartNoop=>StartNoop,BelowThreshold=>BelowThreshold,WrongDirection=>WrongDirection,Cancelled=>Cancelled,CtrlWheel=>CtrlWheel,NativeDockControl=>NativeDockControl,NativeMagnify=>NativeMagnify,NativeCancel=>NativeCancel,SubmissionFailed=>SubmissionFailed});
unit!(oink_routing::router::ContactsPath=>w::DiagContactsPath{NativeContacts=>NativeContacts,CaptureUnavailable=>CaptureUnavailable,AwaitLift=>AwaitLift,Local=>Local});
unit!(oink_routing::router::VolumeAction=>w::DiagVolumeAction{VolumeUp=>VolumeUp,VolumeDown=>VolumeDown,Mute=>Mute,GainUp=>GainUp,GainDown=>GainDown,ForwardMute=>ForwardMute});
unit!(oink_routing::router::VolumeOriginKind=>w::DiagVolumeOriginKind{Physical=>Physical,Synthetic=>Synthetic,Unknown=>Indeterminate});
unit!(oink_routing::router::VolumeDestination=>w::DiagVolumeDestination{Source=>Source,Focus=>Focus,Exit=>Exit,Gain=>Gain});
unit!(oink_input::pointer_transition::PlacementKind=>w::DiagPlacementKind{Owner=>Owner,Tagged=>Tagged,NativeButton=>NativeButton,NativeMotion=>NativeMotion});
unit!(oink_input::button_diagnostics::Layer=>w::DiagButtonLayer{HidTap=>HidTap,SessionTap=>SessionTap,WindowsHook=>WindowsHook});
unit!(oink_input::button_diagnostics::TimestampUnit=>w::DiagTimestampUnit{Nanoseconds=>Nanoseconds,Milliseconds=>Milliseconds});
unit!(oink_input::timing::CallClass=>w::DiagCallClass{Motion=>Motion,Button=>Button,Key=>Key,Scroll=>Scroll,Gesture=>Gesture,Mode=>Mode,Release=>Release,Other=>Other});
unit!(oink_clipboard::FileProviderReason=>w::DiagFileProviderReason{Checking=>Checking,DomainEnabled=>DomainEnabled,UrlsReady=>UrlsReady,DomainDisabled=>DomainDisabled,ExtensionNotRegistered=>ExtensionNotRegistered,DomainDisconnected=>DomainDisconnected,RegistrationFailed=>RegistrationFailed,BoundExpired=>BoundExpired,Cancelled=>Cancelled,ItemMissing=>ItemMissing,ItemUnsupportedType=>ItemUnsupportedType,ItemKindMismatch=>ItemKindMismatch,ItemTargetMismatch=>ItemTargetMismatch,ItemIdentityMismatch=>ItemIdentityMismatch,ItemMetadataMismatch=>ItemMetadataMismatch,ItemAccessDenied=>ItemAccessDenied,ItemMetadataUnreadable=>ItemMetadataUnreadable});
unit!(oink_clipboard::provider_preparation::PreparationPath=>w::DiagPreparationPath{Spare=>Spare,NewParent=>NewParent,DegradedPromise=>DegradedPromise,Failed=>Failed,Placeholder=>Placeholder});
unit!(oink_clipboard::provider_preparation::PlaceholderFallback=>w::DiagPlaceholderFallback{Off=>Off,None=>None,NotReady=>NotReady,Capacity=>Capacity,UnknownSize=>UnknownSize,PreparationFailed=>PreparationFailed,PublicationFailed=>PublicationFailed,DisabledDuringPublication=>DisabledDuringPublication,DeferredToEnter=>DeferredToEnter});
unit!(super::ProviderDragPath=>w::DiagProviderDragPath{Provider=>Provider,DegradedPromise=>DegradedPromise,Failed=>Failed});
unit!(oink_foundation::dnd::NativeHoldEnd=>w::DiagNativeHoldEnd{Resume=>Resume,DraggingExited=>DraggingExited,DraggingEnded=>DraggingEnded,DropAccepted=>DropAccepted,DropRejected=>DropRejected,CancelRepositioned=>CancelRepositioned,HoldRejected=>HoldRejected,HoldUnconfirmed=>HoldUnconfirmed});
unit!(oink_foundation::dnd::ImageNudgeReason=>w::DiagImageNudgeReason{InitialHide=>InitialHide,ImageUpdate=>ImageUpdate});
unit!(oink_routing::Os=>w::DiagOs{MacOS=>MacOS,Windows=>Windows,Other=>Other});
unit!(oink_routing::router::SourceInput=>w::DiagSourceInput{Motion=>Motion,RawMotion=>RawMotion,Button=>Button,Keyboard=>Keyboard,Scroll=>Scroll,Gesture=>Gesture,Contacts=>Contacts});
unit!(oink_routing::router::SourceDestination=>w::DiagSourceDestination{Local=>Local,Forwarded=>Forwarded,Consumed=>Consumed});
structure!(oink_input::warp_delta::DeltaSnapshot=>w::DiagDeltaSnapshot{hid_location,fold,warp,drop_next});
structure!(oink_input::pointer_transition::Placement=>w::DiagPlacement{kind,at,delta,timestamp_ns,source_state});
structure!(oink_input::pointer_transition::Observation=>w::DiagPointerObservation{remote,mode_cursor,mode_timestamp_ns,placements,last_placement,event_position,event_timestamp_ns,source_state,raw_delta,corrected_delta,before,after});
structure!(oink_input::button_diagnostics::SystemState=>w::DiagButtonSystem{sampled_at_us,hid,combined_session,asynchronous});
structure!(oink_input::button_diagnostics::TapDown=>w::DiagTapDown{event_timestamp_ns,source_state,system});
structure!(oink_input::button_diagnostics::TapUp=>w::DiagTapUp{available,generation,observing_since_ns,last_physical_up_ns,last_external_up_ns,matching_down,matching_up});
structure!(oink_input::button_diagnostics::Sample=>w::DiagButtonSample{system,hid_tap,session_tap});
structure!(oink_routing::router::ButtonSourcePress=>w::DiagButtonSourcePress{physical,event_time_us});
structure!(super::buttons::HeldButton=>w::DiagHeldButton{source,button,since_us,held_for_us,captured,holder_sample});
structure!(super::buttons::Refusal=>w::DiagHeldButtonsRefusal{at_us,epoch,holder,holds,omitted,dnd_leaving});
structure!(super::handoff::HandoffEvent=>w::DiagHandoff{action,peer,holder,epoch,router_instance,screen,at,clip,mode,sequence,success,decision,reason,count,topology_sequence,sender,recipient,origin,next_target,flush_reason,first_sequence,message_identity,scope});
structure!(oink_routing::control::TrailPoint=>w::DiagTrailPoint{age_secs,pos});
impl Value for super::handoff::TrailEvent {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::DiagTrail>([
            ("stage", self.stage.wire()?),
            ("topology_sequence", self.topology_sequence.wire()?),
            ("epoch", self.epoch.wire()?),
            ("screen", self.screen.wire()?),
            ("anchor", self.anchor.wire()?),
            ("scale", self.scale.wire()?),
            ("shown_secs", self.shown_secs.wire()?),
            ("size_scale", self.size_scale.wire()?),
            (
                "native_units_per_reference_unit",
                self.native_units_per_reference.wire()?,
            ),
            ("emit_carry", self.emit_carry.wire()?),
            ("heading", self.heading.wire()?),
            ("points", self.points.wire()?),
        ])?
        .write())
    }
}
structure!(super::handoff::DisplayBounds=>w::DiagDisplayBounds{screen,bounds,pixel_width,pixel_height,pixels_per_unit});
structure!(super::handoff::DisplaysEvent=>w::DiagDisplays{phase,revision,device,topology_sequence,screens,truncated});
structure!(super::handoff::PositionEvidence=>w::DiagPositionEvidence{screen,at,native_bounds,compiled_bounds});
structure!(super::handoff::RouterStateEvent=>w::DiagRouterState{phase,epoch,display_revision,topology_sequence,pointer,exit,reference});
structure!(oink_input::gesture_trace::Summary=>w::DiagGestureSummary{kind,phase,identity,begin_identity,missing_begin,replaced,progress,velocity,peak_abs_progress,total_amount,duration_us,changed_events,ctrl_wheel_notches,ctrl_wheel_balance,decision,threshold,task_view_showing,desktop_showing,start_open});
structure!(super::gesture::GestureEvent=>w::DiagGestureEvent{stage,source,summary,contacts_path,disposition});
structure!(super::volume::VolumeRoutingEvent=>w::DiagVolumeRouting{action,key_origin,source_kind,source,focus,decided_target,target_device,reason,focus_forwards,focus_keep_local_playback,source_keep_local_playback,windows_context,recent_default_endpoint_change,default_endpoint_age_ms,recent_audio_arrival_removal,arrival_removal_age_ms});
structure!(oink_clipboard::provider_preparation::PlaceholderTiming=>w::DiagPlaceholderTiming{enabled,used,ready_parents,preparing,prepared_files,prepared_folders,used_files,used_folders,preparation_us,final_urls_us,fallback,failure,cleanup_pending});
structure!(oink_clipboard::provider_preparation::PreparationTiming=>w::DiagPreparationTiming{spare_ready,spare_preparation_us,spare_failure,registration_start_us,registration_us,parent_visible_us,children_enumerated_us,first_url_us,native_begin_us,placeholder});
structure!(super::ProviderAvailability=>w::DiagProviderAvailability{available,reason});
structure!(oink_input::jitter::Distribution=>w::DiagJitterDistribution{count,min_us,median_upper_us,p95_upper_us,max_us,late,percentile_overflow});
structure!(oink_input::jitter::PathSample=>w::DiagJitterPath{ingress,dispatch,baseline_resets,source_reordered});
structure!(oink_input::jitter::Sample=>w::DiagJitter{reliable,lossy});
structure!(oink_input::timing::Sample=>w::DiagInputTiming{window_us,capture_callbacks,capture_callback_max_us,capture_os_wait_max_us,inject_completed,inject_queue_wait_max_us,inject_native_max_us,oldest_pending_us,pending,native_inflight_class,native_inflight_age_us,jitter});
structure!(oink_foundation::dnd::DestinationSnapshot=>w::DiagDestinationSnapshot{session_location,destination_ours,catcher,mouse_location,mouse_hit_ours,visible,level});
structure!(oink_foundation::dnd::CancelGeometry=>w::DiagCancelGeometry{down,target_plane,projected,edge,fallback});
structure!(oink_foundation::dnd::CancelObservation=>w::DiagCancelObservation{geometry,posted,delivery_ack,callback_count,last_callback,ack_position,ack_latency_us,wait_us});
structure!(oink_foundation::dnd::ImageNudgePost=>w::DiagImageNudgePost{at,button,flags});
structure!(oink_foundation::dnd::ImageNudgeObservation=>w::DiagImageNudgeObservation{generation,reason,posted_us,post,image_update_us});
structure!(oink_foundation::dnd::NativeHoldSummary=>w::DiagNativeHoldSummary{operation,native_sequence,elapsed_us,end,entered,updated,image_updated,exited,ended,first_exit_us,last_source_mask,last_returned_mask,last_is_own,returned_none,first_none_us,initial_nil_items,update_nil_items,providers_on_update,show_calls,move_calls,start,first_image,finish,cancel,image_nudges_posted,image_nudges});
structure!(oink_routing::router::SourceDecision=>w::DiagSourceRouting{mouse_mode,keyboard_mode,input,destination,swallowed});

impl Value for (String, String) {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::DiagScopeMember>([("device", self.0.wire()?), ("instance", self.1.wire()?)])?.write())
    }
}
impl Value for ((f64, f64), Point) {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::DiagFold>([("delta", self.0.wire()?), ("target", self.1.wire()?)])?.write())
    }
}
impl Value for (Point, u64) {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::DiagWarp>([("target", self.0.wire()?), ("timestamp_ns", self.1.wire()?)])?.write())
    }
}
impl Value for oink_foundation::input::CaptureMode {
    fn wire(&self) -> v1::Result<RawValue> {
        use w::DiagCaptureModeKind as K;
        let (kind, body) = match self {
            Self::Local => (K::Local, RawMap::new()),
            Self::Remote { park } => (
                K::Remote,
                dto::<w::DiagRemoteCapture>([("park", park.wire()?)])?.into_raw(),
            ),
        };
        envelope(kind, body)
    }
}
structure!(oink_input::button_diagnostics::Transition=>w::DiagButtonObservation{layer,button,pressed,event_timestamp,timestamp_unit,source_state,physical,own,system});
structure!(oink_foundation::input::InputPermissions=>w::DiagInputPermissions{accessibility,input_monitoring,secure_input_active});
unit!(oink_foundation::input::PermissionState=>w::DiagPermissionState{Granted=>Granted,Denied=>Denied,NotDetermined=>NotDetermined,NotApplicable=>NotApplicable,NotGranted=>NotGranted});
unit!(oink_input::native_lifecycle::Kind=>w::LifecycleKind{
    HookInstall=>HookInstall,HookRemove=>HookRemove,HookReinstall=>HookReinstall,Mode=>Mode,Hide=>Hide,Show=>Show,Park=>Park,
    ClipApply=>ClipApply,ClipChanged=>ClipChanged,Registration=>Registration,ModeRoute=>ModeRoute,ModeRequest=>ModeRequest,
    CoverRequest=>CoverRequest,CoverApplied=>CoverApplied,CoverCursor=>CoverCursor,CoverReady=>CoverReady,ParkCommitted=>ParkCommitted,
    UncoverRequest=>UncoverRequest,UncoverCommitted=>UncoverCommitted,ModeDecision=>ModeDecision,InputDesktop=>InputDesktop,Gesture=>Gesture,
    PointerTransition=>PointerTransition,ButtonTransition=>ButtonTransition,PointerAuthority=>PointerAuthority,
    ParkInputSenderUnavailable=>ParkInputSenderUnavailable,ParkInputSendRejected=>ParkInputSendRejected,ParkInputHookNotObserved=>ParkInputHookNotObserved,
    ParkInputTargetMismatch=>ParkInputTargetMismatch,ParkInputHideLost=>ParkInputHideLost,ParkInputHookChainRejected=>ParkInputHookChainRejected,
    ParkInputReadbackMismatch=>ParkInputReadbackMismatch,ParkInputCompletionUnavailable=>ParkInputCompletionUnavailable,
    ContactCapture=>ContactCapture,ContactInjection=>ContactInjection,
    MarkerSubmission=>MarkerSubmission,MarkerHookDecision=>MarkerHookDecision,MarkerChainResult=>MarkerChainResult,MarkerReadback=>MarkerReadback,RestoreSafetyReset=>RestoreSafetyReset,MouseKeysCompensation=>MouseKeysCompensation
});
fn named_scalars<T: Value>(values: impl IntoIterator<Item = (String, T)>) -> v1::Result<RawValue> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    if values.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(v1::ErrorCode::MalformedFrame.into());
    }
    Ok(RawValue::Array(
        values
            .into_iter()
            .map(|(name, value)| {
                dto::<w::NamedScalar>([("name", name.wire()?), ("value", value.wire()?)]).map(|v| v.write())
            })
            .collect::<v1::Result<_>>()?,
    ))
}
fn body<T: WireDto>(values: impl IntoIterator<Item = (&'static str, RawValue)>) -> v1::Result<RawMap> {
    Ok(dto::<T>(values)?.into_raw())
}

impl Value for [u8; 32] {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(RawValue::Bytes(self.to_vec()))
    }
}
structure!(super::layout::Version=>w::DiagVersion{counter,writer_alias});
structure!(super::layout::SavedScreen=>w::DiagSavedScreen{device,screen,local,plane,size_source,gain,enabled,primary,builtin});
structure!(super::layout::Display=>w::DiagDisplay{screen,bounds,pixel_width,pixel_height,reference_extent,rotation_deg,scale,primary,builtin,virtual_display});
structure!(super::layout::Device=>w::DiagDevice{device,saved_member,applied_member,online,input_capable,facts_version,displays});
structure!(super::layout::SourceDevice=>w::DiagSourceDevice{device,origin,requires_target});
structure!(super::layout::Edge=>w::DiagEdge{device,screen,side,at,segments});
structure!(super::layout::Segment=>w::DiagEdgeSegment{from,to,kind});
structure!(super::layout::Settings=>w::DiagSettings{effective});
structure!(super::layout::Compiled=>w::Compiled{current_group,stage,sequence,version,settings,screens,sources,source_devices,input_online,edges});
structure!(super::layout::CompleteLayout=>w::CompleteLayout{sequence,version,roster_digest,saved_members,applied_roster_digest,applied_version,source,paused,saved,settings,devices,compiled,owner_preferences,local_pause});
structure!(super::layout::LayoutChange=>w::LayoutChange{kinds,before,after});
structure!(super::layout::OwnerScreen=>w::DiagOwnerScreen{screen_alias,gain,enabled});
structure!(super::layout::OwnerPreference=>w::DiagOwnerPreference{device_alias,option,revision,effective});
structure!(super::layout::LocalPause=>w::DiagLocalPause{paused,reason,revision});
structure!(super::handoff::DepartureClamp=>w::DepartureClamp{requested,applied,bounds,display_revision});
structure!(super::handoff::CrossingEvent=>w::Crossing{topology_sequence,epoch,from_screen,from,target_screen,final_position,delta,delta_is_local,gap_travel,first_exit,first_entry,first_target_screen,side,gap_distance,remaining_reference_motion,departure_clamp,trigger_zone});
structure!(store::ClockSample=>w::ClockSample{device,instance,at_us,source_clock,local_clock,local_instance,first_source_mono_us,last_source_mono_us,offset_us,uncertainty_us});
structure!(clock::Stamp=>w::ClockStamp{mono_us,utc_us,clock_id});
unit!(oink_display::layout::SizeSource=>w::SizeSource{Reported=>Reported,Estimated=>Estimated,Manual=>Manual});
unit!(oink_display::layout::Side=>w::Side{Left=>Left,Top=>Top,Right=>Right,Bottom=>Bottom});
unit!(super::layout::CompilationStage=>w::CompilationStage{Prepared=>Prepared,Router=>Router});
unit!(super::layout::ChangeKind=>w::ChangeKind{Initialized=>Initialized,GroupChanged=>GroupChanged,Arrangement=>Arrangement,Settings=>Settings,DisplayFacts=>DisplayFacts,Membership=>Membership,Availability=>Availability,CompiledTopology=>CompiledTopology,Applied=>Applied});
structure!(oink_display::layout::EffectiveSettings=>w::EffectiveSettings{gap_travel,overlap_tolerance,minimum_portal,portal_snap,walls,endpoint_reach});
impl Value for super::layout::Scalar {
    fn wire(&self) -> v1::Result<RawValue> {
        match self {
            Self::Flag(v) => v.wire(),
            Self::Integer(v) => v.wire(),
            Self::Number(v) => v.wire(),
        }
    }
}
impl Value for super::layout::OwnerValue {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::DiagOwnerValue>([
            (
                "scalars",
                named_scalars(self.scalars.iter().map(|(n, v)| (n.clone(), v.clone())))?,
            ),
            ("screens", self.screens.wire()?),
            ("omitted", self.omitted.wire()?),
        ])?
        .write())
    }
}
impl Value for super::layout::TopologyScreen {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::DiagTopologyScreen>([
            ("device", self.device.wire()?),
            ("screen", self.screen.wire()?),
            ("local", self.local.wire()?),
            ("plane", self.plane.wire()?),
            ("gain", self.gain.wire()?),
            ("plane_gain", self.plane_gain.wire()?),
            (
                "reference_motion_per_native_unit",
                self.reference_per_native_unit.wire()?,
            ),
            ("size_source", self.size_source.wire()?),
            ("enabled", self.enabled.wire()?),
            ("topology_active", self.topology_active.wire()?),
            ("input_online", self.input_online.wire()?),
            ("active", self.active.wire()?),
        ])?
        .write())
    }
}
impl Value for super::layout::SourceScreen {
    fn wire(&self) -> v1::Result<RawValue> {
        Ok(dto::<w::DiagSourceScreen>([
            ("device", self.device.wire()?),
            ("screen", self.screen.wire()?),
            (
                "reference_motion_per_native_unit",
                self.reference_per_native_unit.wire()?,
            ),
        ])?
        .write())
    }
}
impl Value for super::layout::SegmentKind {
    fn wire(&self) -> v1::Result<RawValue> {
        let (kind, body) = match self {
            Self::Wall => (w::DiagSegmentKind::Wall, RawMap::new()),
            Self::Portal {
                device,
                screen,
                gap_distance,
            } => (
                w::DiagSegmentKind::Portal,
                body::<w::DiagPortal>([
                    ("device", device.wire()?),
                    ("screen", screen.wire()?),
                    ("gap_distance", gap_distance.wire()?),
                ])?,
            ),
        };
        envelope(kind, body)
    }
}
unit!(oink_routing::router::RolePartition=>w::DiagRolePartition{AllSame=>AllSame,SourceInput=>SourceInput,SourceTarget=>SourceTarget,InputTarget=>InputTarget,Distinct=>Distinct});
unit!(oink_routing::router::PhysicalObservation=>w::DiagPhysicalObservation{Down=>Down,Up=>Up,Unobserved=>Unobserved});
unit!(oink_routing::router::DndSourceEffect=>w::DiagDndSourceEffect{Hold=>Hold,Anchor=>Anchor,Resume=>Resume,Finish=>Finish,Release=>Release,CancelReposition=>CancelReposition});
unit!(oink_routing::router::SourcePhase=>w::DiagDndSourcePhase{Local=>Local,Held=>Held,Released=>Released,Committing=>Committing,Receiving=>Receiving,Cancelling=>Cancelling,Closed=>Closed});
unit!(oink_routing::router::IntegrityOutcome=>w::DiagIntegrityOutcome{Completed=>Completed,Cancelled=>Cancelled,CatcherLost=>CatcherLost,HoldRejected=>HoldRejected,EffectUnconfirmed=>EffectUnconfirmed,OriginalRouteReleased=>OriginalRouteReleased,ExportFailed=>ExportFailed});
unit!(oink_routing::dnd::SourceButtonDisposition=>w::DiagSourceDisposition{OriginalSourceOwns=>OriginalSourceOwns,ReleaseSubmitted=>ReleaseSubmitted,Released=>Released,ReturnedToOriginalRoute=>ReturnedToOriginalRoute,Unconfirmed=>Unconfirmed});
structure!(oink_routing::DndIntegrity=>w::DiagIntegrity{roles,source_os,input_os,target_os,holder_epoch,native,physical,disposition,route,outcome});
impl Value for oink_routing::router::NativeObservation {
    fn wire(&self) -> v1::Result<RawValue> {
        use w::DiagDndNativeKind as K;
        let (kind, body) = match self {
            Self::Observed => (K::Observed, RawMap::new()),
            Self::HoldRequested => (K::HoldRequested, RawMap::new()),
            Self::Held => (K::Held, RawMap::new()),
            Self::Resumed => (K::Resumed, RawMap::new()),
            Self::CancelRepositioning => (K::CancelRepositioning, RawMap::new()),
            Self::ReleaseSubmitted => (K::ReleaseSubmitted, RawMap::new()),
            Self::EndedAccepted => (K::EndedAccepted, RawMap::new()),
            Self::EndedRejected => (K::EndedRejected, RawMap::new()),
            Self::CatcherLost => (K::CatcherLost, RawMap::new()),
            Self::Exported => (K::Exported, RawMap::new()),
            Self::Unconfirmed(effect) => (
                K::Unconfirmed,
                body::<w::DiagDndUnconfirmed>([("effect", effect.wire()?)])?,
            ),
            Self::CancelRetired => (K::CancelRetired, RawMap::new()),
        };
        envelope(kind, body)
    }
}
fn screen_summaries(layout: &super::layout::CompleteLayout) -> v1::Result<RawValue> {
    Ok(RawValue::Array(
        layout
            .saved
            .iter()
            .map(|s| {
                dto::<w::DiagScreenSummary>([
                    ("device_alias", s.device.wire()?),
                    ("screen_alias", s.screen.wire()?),
                    ("bounds", s.local.wire()?),
                    ("plane", s.plane.wire()?),
                ])
                .map(|s| s.write())
            })
            .collect::<v1::Result<_>>()?,
    ))
}
fn topology_summary(layout: &super::layout::CompleteLayout) -> v1::Result<RawMap> {
    body::<w::DiagTopologySummary>([
        ("sequence", layout.sequence.wire()?),
        ("version", layout.version.wire()?),
        ("screens", screen_summaries(layout)?),
        ("detail_available", false.wire()?),
    ])
}
impl Event {
    #[cfg(test)]
    pub(super) fn network(&self, rich: bool) -> v1::Result<w::DiagnosticEvent> {
        w::DiagnosticEvent::from_raw(self.network_raw(rich)?)
    }
    fn network_raw(&self, rich: bool) -> v1::Result<RawMap> {
        macro_rules! fields {($t:ty,$($name:ident),*$(,)?)=>{body::<$t>([$((stringify!($name),$name.wire()?),)*])?};}
        macro_rules! projected {
            ($kind:literal,$value:expr) => {
                (
                    $kind,
                    match $value.wire()? {
                        RawValue::Map(fields) => fields,
                        _ => return Err(v1::ErrorCode::MalformedFrame.into()),
                    },
                )
            };
        }
        let (kind, fields) = match self {
            Self::Input {
                origin,
                peer,
                original_us,
                cursor,
                event,
                success,
                modifiers,
                disposition,
            } => (
                "input/1",
                fields!(
                    w::DiagInput,
                    origin,
                    peer,
                    original_us,
                    cursor,
                    event,
                    success,
                    modifiers,
                    disposition
                ),
            ),
            Self::Log {
                module,
                line,
                level,
                fields,
                labels,
            } => (
                "log/1",
                body::<w::DiagLog>([
                    ("module", module.wire()?),
                    ("line", line.wire()?),
                    ("level", level.wire()?),
                    ("fields", named_scalars(fields.clone())?),
                    ("labels", named_scalars(labels.clone())?),
                ])?,
            ),
            Self::Clipboard {
                kind_mask,
                bytes,
                outcome,
            } => ("clipboard/1", fields!(w::DiagClipboard, kind_mask, bytes, outcome)),
            Self::Files {
                files,
                files_done,
                bytes,
                bytes_done,
                phase,
            } => (
                "files/1",
                fields!(w::DiagFiles, files, files_done, bytes, bytes_done, phase),
            ),
            Self::Game {
                holder,
                epoch,
                active,
                detected,
                mode,
            } => ("game/1", fields!(w::DiagGame, holder, epoch, active, detected, mode)),
            Self::Focus { holder, epoch, at } => ("focus/1", fields!(w::DiagFocus, holder, epoch, at)),
            Self::State {
                kind,
                active,
                count,
                value,
            } => ("state/1", fields!(w::DiagState, kind, active, count, value)),
            Self::Link {
                peer,
                online,
                rtt_us,
                sent_packets,
                lost_packets,
            } => (
                "link/1",
                fields!(w::DiagLink, peer, online, rtt_us, sent_packets, lost_packets),
            ),
            Self::KeyboardAnomaly { nonmodifier_flags } => {
                ("keyboard-anomaly/1", fields!(w::DiagKeyboardAnomaly, nonmodifier_flags))
            }
            Self::NativeInputLifecycle {
                at_us,
                sequence,
                kind,
                data,
                holder,
                epoch,
            } => {
                let point = |p: Option<[i32; 2]>| p.map(|p| Point::new(f64::from(p[0]), f64::from(p[1])));
                let rect = |bounds: Option<[i32; 4]>| {
                    bounds.map(|r| {
                        Rect::new(
                            f64::from(r[0]),
                            f64::from(r[1]),
                            (i64::from(r[2]) - i64::from(r[0])) as f64,
                            (i64::from(r[3]) - i64::from(r[1])) as f64,
                        )
                    })
                };
                let observation = dto::<w::LifecycleObservation>([
                    ("success", data.success.wire()?),
                    ("reason", data.reason.wire()?),
                    ("remote", data.remote.wire()?),
                    ("hidden", data.hidden.wire()?),
                    ("count", data.reentries.wire()?),
                    ("native_error", data.flags.wire()?),
                    ("generation", data.extra.wire()?),
                    ("requested", point(data.point).wire()?),
                    ("observed", point(data.cursor).wire()?),
                    ("clip", rect(data.clip).wire()?),
                    ("cursor_showing", data.cursor_showing.wire()?),
                    ("park", point(data.park).wire()?),
                    ("marker", data.marker.wire()?),
                    ("normalized", point(data.normalized).wire()?),
                    ("desktop", rect(data.desktop).wire()?),
                    ("chain_result", data.chain_result.wire()?),
                    ("cursor_before", point(data.cursor_before).wire()?),
                    ("cursor_after_move", point(data.cursor_after_move).wire()?),
                    ("hidden_before", data.hidden_before.wire()?),
                    ("steps", data.steps.wire()?),
                ])?;
                (
                    "input-lifecycle/1",
                    body::<w::InputLifecycle>([
                        ("at_us", at_us.wire()?),
                        ("sequence", sequence.wire()?),
                        ("kind", kind.wire()?),
                        ("holder_alias", holder.wire()?),
                        ("epoch", epoch.wire()?),
                        ("observation", observation.write()),
                    ])?,
                )
            }
            Self::Handoff(v) => projected!("handoff/1", v),
            Self::Topology(v) => {
                if rich {
                    projected!("layout-change/1", v)
                } else {
                    ("topology/1", topology_summary(&v.after)?)
                }
            }
            Self::Crossing(v) => projected!("crossing/1", v),
            Self::Trail(v) => projected!("trail/1", v),
            Self::Displays(v) => projected!("displays/1", v),
            Self::RouterState(v) => projected!("router-state/1", v),
            Self::HeldSync {
                peer,
                source,
                sent,
                keys,
                buttons,
                modifiers,
                press_ids,
                observations,
            } => (
                "held-sync/1",
                fields!(
                    w::DiagHeldSync,
                    peer,
                    source,
                    sent,
                    keys,
                    buttons,
                    modifiers,
                    press_ids,
                    observations
                ),
            ),
            Self::Gesture(v) => projected!("gesture/1", v),
            Self::VolumeRouting(v) => projected!("volume-routing/1", v),
            Self::VolumeRefused(v) => projected!("volume-refused/1", v),
            Self::PointerTransition {
                sequence,
                holder,
                epoch,
                accounting,
            } => (
                "pointer-transition/1",
                fields!(w::DiagPointerTransition, sequence, holder, epoch, accounting),
            ),
            Self::FileProviderAvailability(v) => projected!("file-provider-availability/1", v),
            Self::FileProviderDrag { path, reason } => {
                ("file-provider-drag/1", fields!(w::DiagProviderDrag, path, reason))
            }
            Self::FileProviderPreparation {
                drag,
                epoch,
                path,
                reason,
                timing,
                items,
                bytes,
            } => (
                "file-provider-preparation/1",
                fields!(
                    w::DiagProviderPreparation,
                    drag,
                    epoch,
                    path,
                    reason,
                    timing,
                    items,
                    bytes
                ),
            ),
            Self::InputTiming(v) => projected!("input-timing/1", v),
            Self::DndNativeHold {
                drag,
                physical,
                observation,
            } => (
                "dnd-native-hold/1",
                fields!(w::DiagDndNativeHold, drag, physical, observation),
            ),
            Self::DndIntegrity { drag, record } => ("dnd-integrity/1", fields!(w::DiagDndIntegrity, drag, record)),
            Self::ButtonTransition {
                sequence,
                holder,
                epoch,
                observation,
            } => (
                "button-transition/1",
                fields!(w::DiagButtonTransition, sequence, holder, epoch, observation),
            ),
            Self::HeldButtonsRefusal(v) => projected!("held-buttons-refusal/1", v),
            Self::SourceRouting(v) => projected!("source-routing/1", v),
            Self::ProtocolViolation(v) => projected!("protocol-violation/1", v),
        };
        raw_fields::<w::DiagnosticEvent>([("kind", RawValue::from(kind)), ("fields", RawValue::Map(fields))])
    }
}
unit!(oink_routing::application_focus::Phase=>w::ApplicationFocusPhase{Off=>Off,Preparing=>Preparing,Active=>Active,Suspended=>Suspended});
unit!(oink_routing::application_focus::Reason=>w::ApplicationFocusReason{Disabled=>Disabled,Ready=>Ready,NoPointer=>NoPointer,PointerNotParticipating=>PointerNotParticipating,AwaitingApplication=>AwaitingApplication,DesktopUnavailable=>DesktopUnavailable,Drag=>Drag,Game=>Game,CounterExhausted=>CounterExhausted});
structure!(super::store::ApplicationFocus=>w::DiagApplicationFocus{pointer,keyboard,epoch,revision,phase,reason});

pub(super) fn snapshot(value: &Snapshot, rich: bool) -> v1::Result<w::Snapshot> {
    let layout = value.layout.as_ref();
    let omitted = if rich {
        Vec::new()
    } else {
        vec![RawValue::from("oink.diagnostics.layout/1")]
    };
    let excerpt = dto::<w::DiagSnapshotExcerpt>([
        ("configuration_available", value.group_state.is_some().wire()?),
        ("layout_available", layout.is_some().wire()?),
        ("saved_members", layout.map(|l| l.saved_members.len()).wire()?),
        (
            "online_members",
            layout.map(|l| l.devices.iter().filter(|d| d.online).count()).wire()?,
        ),
        ("omitted_details", RawValue::Array(omitted)),
    ])?;
    let handoff = value
        .button_handoff
        .map(|mode| {
            dto::<w::DiagButtonHandoffValue>([(
                "mode",
                match mode {
                    oink_routing::ButtonHandoff::Block => w::DiagButtonHandoff::Block.write(),
                    oink_routing::ButtonHandoff::Allow => w::DiagButtonHandoff::Allow.write(),
                },
            )])
        })
        .transpose()?;
    let mut fields = vec![
        ("group_state", excerpt.write()),
        ("application_focus", value.application_focus.wire()?),
        ("version", value.version.wire()?),
        ("source_tree_sha256", value.source_tree_sha256.wire()?),
        ("os", value.os.wire()?),
        ("os_version", value.os_version.wire()?),
        (
            "screens",
            layout
                .map(screen_summaries)
                .transpose()?
                .unwrap_or_else(|| RawValue::Array(vec![])),
        ),
        ("permissions", value.permissions.wire()?),
        (
            "settings",
            named_scalars(value.settings.iter().map(|(n, v)| (n.clone(), *v)))?,
        ),
        (
            "routing_numbers",
            named_scalars(value.routing_numbers.iter().map(|(n, v)| (n.clone(), *v)))?,
        ),
        ("button_handoff", handoff.map_or(RawValue::Null, |v| v.write())),
        ("reference_screen", value.reference_screen.wire()?),
        ("injection_failures", value.injection_failures.wire()?),
        ("session_locked", value.session_locked.wire()?),
        ("backend_uiaccess", value.backend_uiaccess.wire()?),
        ("crash_report", value.crash_report.network()?.write()),
    ];
    if rich && let Some(layout) = &value.layout {
        fields.push(("layout", layout.wire()?));
    }
    dto::<w::Snapshot>(fields)
}
pub(super) fn segment(value: &store::Segment, rich: bool) -> v1::Result<w::DiagnosticSegment> {
    let rows = value
        .rows
        .iter()
        .map(|row| {
            raw_fields::<w::DiagnosticRow>([
                ("mono_us", row.mono_us.wire()?),
                ("clock_id", row.clock_id.wire()?),
                ("utc_us", row.utc_us.wire()?),
                ("event", RawValue::Map(row.event.network_raw(rich)?)),
            ])
            .map(RawValue::Map)
        })
        .collect::<v1::Result<_>>()?;
    dto::<w::DiagnosticSegment>([
        ("schema", value.schema.wire()?),
        ("device", value.device.wire()?),
        ("instance", value.instance.wire()?),
        ("seq", value.seq.wire()?),
        ("dropped", value.dropped.wire()?),
        ("snapshot", RawValue::Map(snapshot(&value.snapshot, rich)?.into_raw())),
        ("clock_samples", value.clock_samples.wire()?),
        ("rows", RawValue::Array(rows)),
    ])
}

structure!(super::protocol_violations::Issue=>w::DiagnosticProtocolViolation{code,scope,peer_alias,boot_alias,path_alias,kind,type_id,capability,experimental,isolation,at_us,suppressed});

structure!(super::volume::VolumeRefused=>w::VolumeRefused{press_id,reason,source_alias,target_alias,action});
