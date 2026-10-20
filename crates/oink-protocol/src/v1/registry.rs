//! Append-only product allocations. Experimental ranges are rejected by stable endpoints.
use super::{
    ErrorCode, RawMap, Result,
    dto::*,
    schema::{Schema, WireDto},
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum Transport {
    Reliable,
    Datagram,
    Low,
    Tcp,
    Discovery,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PausePolicy {
    Allow,
    Suspend,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Route {
    pub kind: u16,
    pub transport: Transport,
    pub extra_capabilities: &'static [&'static str],
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Entry {
    pub id: u16,
    pub name: &'static str,
    pub schema: Schema,
    pub routes: &'static [Route],
    pub capabilities: &'static [&'static str],
    pub pause: PausePolicy,
}
macro_rules! message_registry {
    ($($id:literal => $variant:ident($dto:ty), [$($route:expr),*], [$($cap:literal),*], $pause:ident;)*) => {
        #[derive(Clone, Debug, PartialEq)]
        pub enum Message { $($variant($dto),)* }
        impl Message {
            pub fn preflight(type_id:u16,bytes:&[u8])->Result<()> {
                if (0x0d23..=0x0d2e).contains(&type_id) && bytes.len()+6>super::limits::CONTROL_FRAME {return Err(ErrorCode::LimitExceeded.into());}
                let mut decoder=minicbor::Decoder::new(bytes);
                match type_id { $($id=><$dto as WireDto>::preflight(&mut decoder,0)?,)* _=>return Err(ErrorCode::UnknownMessage.into()) }
                if decoder.position()!=bytes.len() {return Err(ErrorCode::MalformedFrame.into());}Ok(())
            }
            pub fn type_id(&self) -> u16 { match self { $(Self::$variant(_) => $id,)* } }
            pub fn raw(&self) -> &RawMap { match self { $(Self::$variant(v) => v.raw(),)* } }
            /// Structural decoding only. Admission and business safety validation
            /// must succeed before dispatch.
            pub fn from_payload(type_id:u16, raw:RawMap) -> Result<Self> {
                if (0x0d23..=0x0d2e).contains(&type_id) && super::cbor::map_len(&raw)?+6>super::limits::CONTROL_FRAME {return Err(ErrorCode::LimitExceeded.into());}
                let message=match type_id { $($id => <$dto>::from_raw(raw).map(Self::$variant),)* _ => Err(ErrorCode::UnknownMessage.into()) }?;
                if let Self::ConfigInfo(greeting)=&message && greeting.selected()?.is_none() {return Err(ErrorCode::MalformedFrame.into());}
                Ok(message)
            }
        }
        pub const MESSAGES:&[Entry]=&[$(Entry {id:$id,name:stringify!($variant),schema:<$dto>::SCHEMA,routes:&[$($route),*],capabilities:&[$($cap),*],pause:PausePolicy::$pause},)*];
    };
}
include!("registry_entries.rs");
pub fn message(id: u16) -> Option<&'static Entry> {
    MESSAGES.iter().find(|m| m.id == id)
}
/// Dropped development operations/wrappers are permanently unavailable to v1.
pub const TOMBSTONES: &[u16] = &[
    0x0d09, 0x0d0a, 0x0d0b, 0x0200, 0x0206, 0x0301, 0x0302, 0x0303, 0x0304, 0x0305, 0x0306, 0x0307, 0x0308, 0x0404,
    0x0508, 0x0600, 0x0601, 0x0900, 0x0a08, 0x0c12, 0x0e00, 0x0e01, 0x0e02, 0x0e03, 0x1000,
];
pub const BASELINE: &[&str] = &[
    "oink.core/1",
    "oink.group/1",
    "oink.input/1",
    "oink.clipboard/1",
    "oink.low/1",
];
pub const CAPABILITIES: &[&str] = &[
    "oink.core/1",
    "oink.group/1",
    "oink.input/1",
    "oink.clipboard/1",
    "oink.low/1",
    "oink.relay/1",
    "oink.pairing/1",
    "oink.drag/1",
    "oink.input.relative/1",
    "oink.input.gestures/1",
    "oink.input.contacts/1",
    "oink.input.trail/1",
    "oink.audio/1",
    "oink.audio.pcm/1",
    "oink.audio.state/1",
    "oink.aim/1",
    "oink.wallpaper/1",
    "oink.cache/1",
    "oink.diagnostics.collect/1",
    "oink.diagnostics.layout/1",
    "oink.layout.projection/1",
    "oink.input.edge-trigger/1",
    "oink.device-resources/1",
    "oink.device-options/1",
    "oink.audio.exit-volume/1",
    "oink.notify/1",
];
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Stream {
    pub kind: u16,
    pub name: &'static str,
    pub priority: u8,
    pub capabilities: &'static [&'static str],
    pub reliable: bool,
    pub datagram: bool,
    pub low: bool,
}
macro_rules! stream {
    ($kind:literal,$name:literal,$pri:literal,$caps:expr,$r:literal,$d:literal,$l:literal) => {
        Stream {
            kind: $kind,
            name: $name,
            priority: $pri,
            capabilities: $caps,
            reliable: $r,
            datagram: $d,
            low: $l,
        }
    };
}
pub const STREAMS: &[Stream] = &[
    stream!(0, "N0", 0, &["oink.core/1"], true, false, false),
    stream!(1, "N1", 1, &["oink.low/1"], true, false, false),
    stream!(2, "C0", 1, &["oink.group/1"], true, false, false),
    stream!(3, "I0", 0, &["oink.input/1"], true, true, false),
    stream!(4, "B0", 1, &["oink.clipboard/1"], true, false, true),
    stream!(5, "M0", 1, &["oink.audio/1"], true, true, false),
    stream!(7, "A0", 1, &["oink.aim/1"], true, false, false),
    stream!(8, "D0", 1, &["oink.diagnostics.collect/1"], true, false, true),
    stream!(9, "Wallpaper", 1, &["oink.wallpaper/1"], false, false, true),
    stream!(10, "Dnd", 1, &["oink.drag/1"], false, false, true),
    stream!(11, "Cache", 1, &["oink.cache/1"], false, false, true),
    stream!(12, "P0", 1, &["oink.layout.projection/1"], true, false, false),
];
pub fn stream(kind: u16) -> Option<&'static Stream> {
    STREAMS.iter().find(|s| s.kind == kind)
}
pub const fn experimental_message(id: u16) -> bool {
    id >= 0x8000 && id != 0xffff
}
pub const fn experimental_stream(kind: u16) -> bool {
    (kind >= 0xf0 && kind <= 0xff) || (kind >= 0x8000 && kind != 0xffff)
}
pub const fn experimental_field(key: u32) -> bool {
    key >= 0x8000_0000 && key != u32::MAX
}

include!("schema_entries.rs");
#[derive(Debug, Serialize)]
pub struct Registry {
    pub labels: &'static [(&'static str, &'static [u8])],
    pub reserved_messages: &'static [(u16, &'static str)],
    pub reserved_streams: &'static [(u16, &'static str)],
    pub reserved_capabilities: &'static [&'static str],
    pub limits: &'static [(&'static str, u64)],
    pub open_enums: &'static [OpenEnumAllocation],
    pub revision: &'static str,
    pub reserved_fields: &'static [(&'static str, &'static [u32])],
    pub register_kinds: &'static [(u32, &'static str)],
    pub reserved_registers: &'static [u32],
    pub reserved_enum_values: &'static [(&'static str, &'static [u32])],
    pub units: &'static [(&'static str, &'static str)],
    pub errors: &'static [(u16, &'static str)],
    pub diagnostic_events: &'static [&'static str],
    pub messages: &'static [Entry],
    pub streams: &'static [Stream],
    pub schemas: &'static [Schema],
    pub enums: &'static [(&'static str, &'static [(u32, &'static str)])],
    pub capabilities: &'static [&'static str],
    pub baseline: &'static [&'static str],
    pub tombstones: &'static [u16],
}
pub fn schema_registry() -> Registry {
    Registry {
        labels: super::labels::ALL,
        reserved_messages: RESERVED_MESSAGES,
        reserved_streams: RESERVED_STREAMS,
        reserved_capabilities: RESERVED_CAPABILITIES,
        limits: super::limits::ALL,
        open_enums: OPEN_ENUMS,
        reserved_fields: RESERVED_FIELDS,
        register_kinds: REGISTER_KINDS,
        reserved_registers: RESERVED_REGISTERS,
        reserved_enum_values: RESERVED_ENUM_VALUES,
        units: UNITS,
        errors: ERRORS,
        diagnostic_events: DIAGNOSTIC_EVENTS,
        revision: super::SPEC_REVISION,
        messages: MESSAGES,
        streams: STREAMS,
        schemas: SCHEMAS,
        enums: ENUMS,
        capabilities: CAPABILITIES,
        baseline: BASELINE,
        tombstones: TOMBSTONES,
    }
}
/// Reserved field numbers stay documented even though decoders retain them as
/// unknown data; none of them confer a removed key/proof's former authority.
/// LayoutValue.3 reserves a list of slot maps (screen:1, u16 slot:2,
/// u64 assigned_ms:3). ScreenPlacement.8 reserves a map with quad:1 (four
/// points), homography:2 (nine f64s), rotation_deg:3, keystone:4,
/// reproj_err_px:5, and optional feel_width:6 (all f64). CompleteLayout.13
/// reserves the diagnostic slot list. These positions are omittable and new
/// objects do not emit them, including null/empty placeholders. A retained
/// extension is not permission to transform geometry its owner cannot interpret.
/// ScreenReady's former display:1, pixel_width:2, pixel_height:3, problem:5
/// remain allocated; key4 is retired. These names describe reservations only,
/// never executable DTO schemas.
pub const RESERVED_FIELDS: &[(&str, &[u32])] = &[
    ("ProtocolError", &[9, 10]),
    ("ObservedState", &[5]),
    ("ResourceFault", &[2]),
    ("AudioState", &[1, 2]),
    ("AudioRouteFault", &[2]),
    ("ScreenReady", &[1, 2, 3, 4, 5]),
    ("LowUnavailable", &[3]),
    ("Grant", &[2]),
    ("Request", &[1, 3]),
    ("Reply", &[1, 3]),
    ("Advert", &[4]),
    ("ConfigVersion", &[2]),
    ("Register", &[2]),
    ("MergePlan", &[2]),
    ("GeometryPatch", &[2]),
    ("LayoutValue", &[3]),
    ("ScreenPlacement", &[6, 7, 8]),
    ("CalibSlot", &[1, 2, 3]),
    ("ScreenCalib", &[1, 2, 3, 4, 5, 6]),
    ("TopologySettings", &[1, 2, 3, 4, 5, 11]),
    ("EffectiveSettings", &[1, 2, 3, 4, 5, 11]),
    ("LocalOptionState", &[4]),
    ("ExitVolumeVersion", &[2]),
    ("DiagVersion", &[2]),
    ("DiagSettings", &[2]),
    ("CompleteLayout", &[3, 8, 13]),
    ("Compiled", &[5]),
    ("InputTouchpadContacts", &[5]),
];
/// Allocated operations which this release does not implement. They are not
/// admitted by the endpoint registry, even if a peer advertises their capability.
/// The whole 04xx family remains assigned to calibration; no other family may
/// allocate there. Relays may carry opaque frames without interpreting them.
pub const RESERVED_MESSAGES: &[(u16, &str)] = &[
    (0x0400, "CalibShowPattern"),
    (0x0401, "CalibReady"),
    (0x0402, "CalibStatus"),
    (0x0403, "CalibHide"),
];
pub const RESERVED_STREAMS: &[(u16, &str)] = &[(6, "K0")];
pub const RESERVED_CAPABILITIES: &[&str] = &["oink.calibration/1"];
pub const REGISTER_KINDS: &[(u32, &str)] = &[(2, "LayoutValue")];
pub const RESERVED_REGISTERS: &[u32] = &[1, 3, 4, 5, 6, 7];
pub const RESERVED_ENUM_VALUES: &[(&str, &[u32])] = &[
    ("SizeSource", &[2]),
    ("LayoutSource", &[2]),
    ("ChangeKind", &[3]),
    ("PairLimitKind", &[6]),
    ("PairCode", &[29]),
    ("PauseReason", &[2]),
    ("LocalOption", &[9, 17]),
    ("ControlFailure", &[22]),
];
/// Wire units remain stable across optional fields and nested business records.
/// Native coordinates and image-pixel errors keep their original spaces.
pub const UNITS: &[(&str, &str)] = &[
    (
        "DevicePlacement.origin;ProjectionPreview.screens.rect",
        "reference ratio",
    ),
    ("DevicePlacement.scale", "reference ratio per OS native unit"),
    (
        "TopologySettings.overlap_tolerance;minimum_portal;portal_snap;endpoint_reach",
        "reference ratio",
    ),
    (
        "EffectiveSettings.overlap_tolerance;minimum_portal;portal_snap;endpoint_reach",
        "reference ratio",
    ),
    ("DisplayFact.reference_extent;DiagDisplay.6", "reference ratio"),
    (
        "InputMotion.dx;InputMotion.dy;InputScroll.Reference.dx;InputScroll.Reference.dy",
        "reference motion ratio",
    ),
    (
        "InputTouchpadContacts.surface;InputTouchpadContacts.changes",
        "integer reference quanta: 1/68600; surface bounds 1000..100000",
    ),
    ("TrailContinuation.3", "native_units_per_reference"),
    (
        "DiagTopologyScreen.8;DiagSourceScreen.3",
        "reference_motion_per_native_unit",
    ),
    (
        "Crossing.gap_distance;Crossing.remaining_reference_motion;DiagSegmentKind.Portal.3",
        "reference ratio",
    ),
    ("AimState.radius;AimState.targets;AimClick.pos", "reference ratio"),
];
pub const DIAGNOSTIC_EVENTS: &[&str] = &[
    "protocol-violation/1",
    "volume-refused/1",
    "input/1",
    "log/1",
    "clipboard/1",
    "files/1",
    "game/1",
    "focus/1",
    "state/1",
    "link/1",
    "keyboard-anomaly/1",
    "input-lifecycle/1",
    "handoff/1",
    "topology/1",
    "layout-change/1",
    "crossing/1",
    "trail/1",
    "displays/1",
    "router-state/1",
    "held-sync/1",
    "gesture/1",
    "volume-routing/1",
    "pointer-transition/1",
    "file-provider-availability/1",
    "file-provider-drag/1",
    "file-provider-preparation/1",
    "input-timing/1",
    "dnd-native-hold/1",
    "dnd-integrity/1",
    "button-transition/1",
    "held-buttons-refusal/1",
    "source-routing/1",
];
pub const ERRORS: &[(u16, &str)] = &[
    (0, "Normal"),
    (1, "UnsupportedKind"),
    (2, "NotReady"),
    (3, "StaleOrdinal"),
    (4, "MalformedFrame"),
    (5, "ResendOpening"),
    (6, "UnknownMessage"),
    (7, "UnnegotiatedCapability"),
    (8, "UnsupportedMajor"),
    (9, "MissingCapability"),
    (10, "UnsupportedValue"),
    (11, "AuthenticationFailed"),
    (12, "LimitExceeded"),
    (13, "InvalidChannel"),
    (14, "NegotiationMismatch"),
    (15, "UnsupportedHeaderExtension"),
    (16, "Cancelled"),
    (17, "StateConflict"),
    (18, "InvalidHeader"),
];
pub type OpenEnumAllocation = (&'static str, &'static str, &'static [(u32, Option<&'static str>)]);
pub const OPEN_ENUMS: &[OpenEnumAllocation] = &[
    (
        "MouseButton",
        "MouseButtonKind",
        &[
            (0, None),
            (1, None),
            (2, None),
            (3, None),
            (4, None),
            (5, Some("OtherMouseButton")),
        ],
    ),
    (
        "Gesture",
        "GestureKind",
        &[
            (0, Some("DockSwipe")),
            (1, Some("Magnify")),
            (2, Some("Rotate")),
            (3, None),
        ],
    ),
    (
        "DiagCaptureMode",
        "DiagCaptureModeKind",
        &[(0, None), (1, Some("DiagRemoteCapture"))],
    ),
    (
        "DiagDndNative",
        "DiagDndNativeKind",
        &[
            (0, None),
            (1, None),
            (2, None),
            (3, None),
            (4, None),
            (5, None),
            (6, None),
            (7, None),
            (8, None),
            (9, None),
            (10, Some("DiagDndUnconfirmed")),
            (11, None),
        ],
    ),
    ("DiagEdgeKind", "DiagSegmentKind", &[(0, None), (1, Some("DiagPortal"))]),
    (
        "DiagMouseButton",
        "DiagMouseButtonKind",
        &[
            (0, None),
            (1, None),
            (2, None),
            (3, None),
            (4, None),
            (5, Some("DiagOtherButton")),
        ],
    ),
    (
        "DiagGesture",
        "DiagGestureKind",
        &[
            (0, Some("DiagDockSwipe")),
            (1, Some("DiagMagnify")),
            (2, Some("DiagRotate")),
            (3, None),
        ],
    ),
    (
        "DiagSafeInput",
        "DiagSafeInputKind",
        &[
            (0, None),
            (1, Some("DiagMotion")),
            (2, Some("DiagPosition")),
            (3, Some("DiagButton")),
            (4, Some("DiagScroll")),
            (5, Some("DiagKey")),
            (6, Some("DiagGestureValue")),
            (7, Some("DiagScrollDistance")),
            (8, Some("DiagRemoteMotion")),
            (9, Some("DiagRelativeTotals")),
        ],
    ),
    (
        "AttachmentHeader",
        "AttachmentKind",
        &[(0, Some("OfferAttachment")), (1, Some("DragAttachment"))],
    ),
    (
        "SourceAction",
        "SourceActionKind",
        &[(0, Some("SourceHandoff")), (1, None), (2, None)],
    ),
    (
        "DragState",
        "DragStateKind",
        &[
            (0, None),
            (1, Some("DragHeldState")),
            (2, Some("DragHeldState")),
            (3, Some("DragRevoked")),
            (4, Some("DragEnded")),
        ],
    ),
    (
        "RevokeScope",
        "RevokeScopeKind",
        &[(0, None), (1, Some("RevokeHolder")), (2, None)],
    ),
    (
        "PreviewAvailability",
        "PreviewAvailabilityKind",
        &[(0, None), (1, None), (2, Some("PreviewAvailable"))],
    ),
    (
        "PathReference",
        "PathReferenceKind",
        &[(0, Some("RequestedPath")), (1, Some("InstalledPath"))],
    ),
    ("ContentSize", "ContentSizeKind", &[(0, None), (1, Some("KnownSize"))]),
    (
        "RepresentationData",
        "RepresentationDataKind",
        &[
            (0, Some("RepresentationReader")),
            (1, Some("RepresentationInline")),
            (2, Some("RepresentationFile")),
            (3, Some("RepresentationUnavailable")),
            (4, Some("RepresentationPromise")),
        ],
    ),
    (
        "Demand",
        "DemandKind",
        &[(0, None), (1, Some("HolderDemand")), (2, Some("HolderDemand"))],
    ),
    (
        "PairStep",
        "PairStepKind",
        &[(0, Some("PakeStart")), (1, Some("PakeReply")), (2, Some("PakeFinish"))],
    ),
    (
        "ReplyState",
        "ReplyStateKind",
        &[(0, Some("ReplySame")), (1, Some("ReplySnapshot"))],
    ),
    ("TypedResult", "ResultKind", &[(0, None), (1, Some("ProtocolError"))]),
    (
        "Decision",
        "DecisionKind",
        &[(0, None), (1, Some("DecisionCommitted")), (2, Some("DecisionAborted"))],
    ),
    (
        "ObjectContext",
        "ObjectContextKind",
        &[(0, Some("ObjectPairContext")), (1, Some("ObjectGroupContext"))],
    ),
];
pub fn validate_open(name: &str, raw: &RawMap) -> Result<()> {
    match name {
        "MouseButton" => {
            MouseButton::from_raw(raw.clone())?;
        }
        "Gesture" => {
            Gesture::from_raw(raw.clone())?;
        }
        "DiagCaptureMode" => {
            DiagCaptureMode::from_raw(raw.clone())?;
        }
        "DiagDndNative" => {
            DiagDndNative::from_raw(raw.clone())?;
        }
        "DiagEdgeKind" => {
            DiagEdgeKind::from_raw(raw.clone())?;
        }

        "DiagMouseButton" => {
            DiagMouseButton::from_raw(raw.clone())?;
        }
        "DiagGesture" => {
            DiagGesture::from_raw(raw.clone())?;
        }
        "DiagSafeInput" => {
            DiagSafeInput::from_raw(raw.clone())?;
        }

        "AttachmentHeader" => {
            AttachmentHeader::from_raw(raw.clone())?;
        }
        "SourceAction" => {
            SourceAction::from_raw(raw.clone())?;
        }
        "DragState" => {
            DragState::from_raw(raw.clone())?;
        }
        "RevokeScope" => {
            RevokeScope::from_raw(raw.clone())?;
        }
        "PreviewAvailability" => {
            PreviewAvailability::from_raw(raw.clone())?;
        }
        "PathReference" => {
            PathReference::from_raw(raw.clone())?;
        }
        "ContentSize" => {
            ContentSize::from_raw(raw.clone())?;
        }
        "RepresentationData" => {
            RepresentationData::from_raw(raw.clone())?;
        }
        "Demand" => {
            Demand::from_raw(raw.clone())?;
        }
        "PairStep" => {
            PairStep::from_raw(raw.clone())?;
        }
        "ReplyState" => {
            ReplyState::from_raw(raw.clone())?;
        }
        "TypedResult" => {
            TypedResult::from_raw(raw.clone())?;
        }
        "Decision" => {
            Decision::from_raw(raw.clone())?;
        }
        "ObjectContext" => {
            ObjectContext::from_raw(raw.clone())?;
        }
        _ => return Err(ErrorCode::UnsupportedValue.into()),
    }
    Ok(())
}
