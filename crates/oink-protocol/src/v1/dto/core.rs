use super::*;
use crate::v1::hot::hot_map;
use crate::v1::schema::{
    OpenEnvelope, WireDto, WireValue, defaulted, enum_body, no_validation, open_enum, required, wire_map,
};
use crate::v1::{ErrorCode, RawMap, RawValue, Result};

hot_map!(DeviceId, validate_identity, {
    1 => algorithm: AlgorithmId = required;
    2 => public_key: Bytes<4096> = required;
});

wire_map!(Signature, validate_signature, {
    1 => algorithm: AlgorithmId = required;
    2 => value: Bytes<1024> = required;
});

hot_map!(Boot, no_validation, {
    1 => device_id: DeviceId = required;
    2 => nonce: Id16 = required;
});

wire_map!(BootPair, validate_boot_pair, {
    1 => coordinator: Boot = required;
    2 => follower: Boot = required;
});

wire_map!(ConnectionId, no_validation, {
    1 => initiator_boot: Boot = required;
    2 => random: Id16 = required;
});

wire_map!(PairId, no_validation, {
    1 => initiator_boot: Boot = required;
    2 => random: Id16 = required;
});

wire_map!(PathId, no_validation, {
    1 => boot_pair: BootPair = required;
    2 => coordinator_counter: u64 = required;
});

wire_map!(ProtocolVersion, no_validation, {
    1 => major: u16 = required;
    2 => minor: u16 = required;
});

wire_map!(ProtocolOffer, no_validation, {
    1 => major: u16 = required;
    2 => highest_minor: u16 = required;
});

wire_map!(CryptoOffers, no_validation, {
    1 => pairing_suites: List<AlgorithmId,16> = required;
    2 => admission_suites: List<AlgorithmId,16> = required;
    3 => identity_algorithms: List<AlgorithmId,4> = required;
});

wire_map!(Limits, validate_limits, {
    1 => reliable_frame: u32 = required;
    2 => config_frame: u32 = required;
    3 => low_frame: u32 = required;
    4 => datagram: u32 = required;
    5 => config_state: u32 = required;
    6 => low_chunk: u32 = required;
});

wire_map!(FormatSupport, no_validation, {
    1 => portable_receive: List<FormatId,64> = required;
    2 => portable_send: List<FormatId,64> = required;
    3 => native_namespaces: List<Ascii<64>,16> = required;
    4 => media: List<Medium,16> = required;
});

wire_map!(AudioSupport, no_validation, {
    1 => codec: AlgorithmId = required;
    2 => sample_rates: List<u32,16> = required;
    3 => channels: List<u8,16> = required;
    4 => frame_us: List<u32,16> = required;
    5 => max_payload: u16 = required;
});

wire_map!(AudioDescriptor, no_validation, {
    1 => codec: AlgorithmId = required;
    2 => sample_rate: u32 = required;
    3 => channels: u8 = required;
    4 => frame_us: u32 = required;
    5 => max_payload: u16 = required;
    6 => parameters: RawMap = required;
});

wire_map!(RelayContext, no_validation, {
    1 => path_id: PathId = required;
    2 => route_revision: u64 = required;
    3 => initiator: Boot = required;
    4 => responder: Boot = required;
});

wire_map!(ConnectionOffer, validate_offer, {
    1 => boot: Boot = required;
    2 => connection_id: ConnectionId = required;
    3 => protocols: List<ProtocolOffer,8> = required;
    4 => capabilities: CapabilityList = required;
    5 => required_capabilities: CapabilityList = required;
    6 => limits: Limits = required;
    7 => crypto: CryptoOffers = required;
    8 => nonce: Hash32 = required;
    9 => app_version: Text<256> = required;
    10 => formats: FormatSupport = required;
    11 => audio: List<AudioSupport,16> = required;
    12 => relay_context: Option<RelayContext> = optional;
});

wire_map!(Negotiated, validate_selected, {
    1 => major: u16 = required;
    2 => minor: u16 = required;
    3 => enabled: CapabilityList = required;
    4 => limits: Limits = required;
    5 => pairing_suite: Option<AlgorithmId> = optional;
    6 => admission_suite: AlgorithmId = required;
    7 => offer_hash: Hash32 = required;
});

wire_map!(StreamOpened, no_validation, {
    1 => path_id: PathId = required;
    2 => route_revision: u64 = required;
    3 => kind: u16 = required;
    4 => ordinal: u32 = required;
});

wire_map!(Roster, no_validation, {
    1 => group: Id16 = required;
    2 => root_id: Hash32 = required;
    3 => records: List<SignedRecord,256> = required;
});

wire_map!(SignedRecord, validate_record, {
    1 => body: RawMap = required;
    2 => signature: Signature = required;
});

wire_map!(RecordBody, no_validation, {
    1 => kind: RecordKind = required;
    2 => group: Id16 = required;
    3 => subject: DeviceId = required;
    4 => epoch: u64 = required;
    5 => issuer: DeviceId = required;
    6 => issuer_ref: Option<Hash32> = optional;
    7 => name: Option<Text<256>> = optional;
    8 => os: Option<Text<256>> = optional;
    9 => issued_ms: u64 = required;
});

hot_map!(ConfigVersion, no_validation, {
    1 => counter: u64 = required;
    3 => writer: DeviceId = required;
});

wire_map!(GroupState, validate_state, {
    1 => group: Id16 = required;
    2 => registers: List<Register,256> = required;
    3 => roster: Roster = required;
});

wire_map!(Greeting, no_validation, {
    1 => advert: Advert = required;
    2 => os: Text<256> = required;
    3 => nonce: Hash32 = required;
    4 => offer: ConnectionOffer = required;
    5 => selected: Option<Negotiated> = optional;
    6 => root_id: Option<Hash32> = optional;
    7 => roster_digest: Option<Hash32> = optional;
});

wire_map!(Admission, no_validation, {
    1 => identity: DeviceId = required;
    2 => group: Id16 = required;
    3 => root_id: Hash32 = required;
    4 => roster_digest: Hash32 = required;
    5 => negotiation_hash: Hash32 = required;
});

wire_map!(Done, no_validation, {
    1 => accepted_state_digest: Hash32 = required;
    2 => negotiation_hash: Hash32 = required;
    3 => roster_digest: Hash32 = required;
});

wire_map!(Advert, validate_advert, {
    1 => id: DeviceId = required;
    2 => name: Text<200> = required;
    3 => group: Id16 = required;
    5 => count: u8 = required;
    6 => port: u16 = required;
    7 => protocols: List<ProtocolOffer,8> = required;
    8 => identity_algorithms: List<AlgorithmId,4> = required;
    9 => os: Ascii<32> = required;
});

wire_map!(ReplySame, no_validation, {
    1 => request_state_digest: Hash32 = required;
});

wire_map!(ReplySnapshot, no_validation, {
    1 => state: GroupState = required;
});

wire_map!(Point, no_validation, {
    1 => x: f64 = required;
    2 => y: f64 = required;
});

wire_map!(Size, no_validation, {
    1 => w: f64 = required;
    2 => h: f64 = required;
});

wire_map!(Rect, no_validation, {
    1 => x: f64 = required;
    2 => y: f64 = required;
    3 => w: f64 = required;
    4 => h: f64 = required;
});

wire_map!(LayoutValue, no_validation, {
    1 => devices: List<DevicePlacement,256> = required;
    2 => source: LayoutSource = required;
    4 => topology: TopologySettings = required;
});

wire_map!(InputTransformValue, no_validation, {
    1 => macos: TargetTransform = required;
    2 => windows: TargetTransform = required;
    3 => overrides: List<TargetOverride,16> = required;
});

wire_map!(Digest, validate_digest, {
    1 => algorithm: AlgorithmId = required;
    2 => bytes: Bytes<64> = required;
});

wire_map!(Grant, no_validation, {
    1 => group: Id16 = required;
    3 => state: GroupState = required;
});

wire_map!(Register, validate_register, {
    1 => kind: u32 = required;
    3 => version: ConfigVersion = required;
    4 => value: RawMap = required;
    5 => deleted: bool = false;
    6 => digest: Hash32 = required;
});

wire_map!(Request, validate_request, {
    2 => connected_members: u8 = required;
    4 => state: Option<GroupState> = optional;
    5 => grant: Option<Grant> = optional;
    6 => roster: Option<Roster> = optional;
    7 => admission: Admission = required;
});

wire_map!(Reply, validate_reply, {
    2 => connected_members: u8 = required;
    4 => state: ReplyState = required;
    5 => grant: Option<Grant> = optional;
    6 => roster: Option<Roster> = optional;
    7 => admission: Admission = required;
});

wire_map!(ProtocolError, no_validation, {
    1 => code: u16 = required;
    2 => scope: ErrorScope = required;
    3 => offending_type: Option<u16> = optional;
    4 => offending_kind: Option<u16> = optional;
    5 => field_path: Option<List<u32,32>> = optional;
    6 => operation: Option<Bytes<256>> = optional;
    7 => required_capability: Option<CapabilityId> = optional;
    8 => peer_major: Option<u16> = optional;
});

wire_map!(TopologySettings, validate_topology, {
    6 => gap_travel: bool = false;
    7 => overlap_tolerance: f64 = (5.0 / 686.0);
    8 => minimum_portal: f64 = (10.0 / 686.0);
    9 => portal_snap: f64 = (8.0 / 686.0);
    10 => walls: bool = true;
    12 => endpoint_reach: f64 = 0.0;
});
open_enum!(ErrorScope { Connection=0, LogicalLink=1, Stream=2, LowOperation=3, Message=4 });
open_enum!(Isolation { Stream=0, LowOperation=1, LogicalLink=2, PhysicalConnection=3 });
open_enum!(RecordKind { Create=0, Add=1, Remove=2 });
open_enum!(LayoutSource { Default=0, Manual=1 });
open_enum!(SpeedMode { Physical=0, ScreenProportional=1 });
open_enum!(Medium { Bytes=0,HGlobal=1,Stream=2,Storage=3,Bitmap=4,Palette=5,Metafile=6,EnhancedMetafile=7,File=8 });
open_enum!(ReplyStateKind { Same=0, State=1 });
enum_body!(ReplyStateKind { Same => ReplySame, State => ReplySnapshot });
pub type ReplyState = OpenEnvelope<ReplyStateKind>;
open_enum!(ResultKind { Success=0, Failure=1 });
enum_body!(ResultKind { Failure => ProtocolError });
pub type TypedResult = OpenEnvelope<ResultKind>;
impl TypedResult {
    pub fn is_success(&self) -> bool {
        matches!(self.variant(), Ok(crate::v1::schema::Open::Known(ResultKind::Success)))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatId(pub String);
impl WireValue for FormatId {
    fn read(raw: &RawValue) -> Result<Self> {
        let text = Text::<4096>::read(raw)?.0;
        let Some((namespace, rest)) = text.split_once(':') else {
            return Err(ErrorCode::MalformedFrame.into());
        };
        if namespace.is_empty()
            || namespace.len() > 64
            || !namespace.is_ascii()
            || rest.is_empty()
            || text.contains('\0')
        {
            return Err(ErrorCode::MalformedFrame.into());
        }
        if namespace == "win-cf" && !rest.parse::<u8>().is_ok_and(|n| (1..=17).contains(&n)) {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        Ok(Self(text))
    }
    fn write(&self) -> RawValue {
        RawValue::Text(self.0.clone())
    }
}
fn validate_identity(raw: &RawMap) -> Result<()> {
    if required::<AlgorithmId>(raw, 1)?.0 == ED25519 && required::<Bytes<4096>>(raw, 2)?.0.len() != 32 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_signature(raw: &RawMap) -> Result<()> {
    if required::<AlgorithmId>(raw, 1)?.0 == ED25519 && required::<Bytes<1024>>(raw, 2)?.0.len() != 64 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
impl DeviceId {
    pub fn ordering_key(&self) -> Result<(String, Vec<u8>)> {
        Ok((self.algorithm()?.0, self.public_key()?.0))
    }
    pub fn require_supported(&self) -> Result<()> {
        if self.algorithm()?.0 == ED25519 {
            Ok(())
        } else {
            Err(ErrorCode::UnsupportedValue.into())
        }
    }
}
fn validate_boot_pair(raw: &RawMap) -> Result<()> {
    if required::<Boot>(raw, 1)?.device_id()?.ordering_key()?
        >= required::<Boot>(raw, 2)?.device_id()?.ordering_key()?
    {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
pub(crate) fn validate_capabilities(caps: &CapabilityList) -> Result<()> {
    if caps.0.windows(2).any(|w| w[0] >= w[1]) {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_offer(raw: &RawMap) -> Result<()> {
    validate_capabilities(&required(raw, 4)?)?;
    validate_capabilities(&required(raw, 5)?)?;
    let protocols = required::<List<ProtocolOffer, 8>>(raw, 3)?.0;
    if protocols.is_empty() {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let mut majors = std::collections::BTreeSet::new();
    for p in protocols {
        if !majors.insert(p.major()?) {
            return Err(ErrorCode::MalformedFrame.into());
        }
    }
    Ok(())
}
fn validate_selected(raw: &RawMap) -> Result<()> {
    validate_capabilities(&required(raw, 3)?)
}
fn validate_limits(raw: &RawMap) -> Result<()> {
    let baseline = [16384, 131072, 16777216, 1100, 122880, 16384];
    for (key, min) in (1..=6).zip(baseline) {
        if required::<u32>(raw, key)? < min {
            return Err(ErrorCode::LimitExceeded.into());
        }
    }
    Ok(())
}
impl Limits {
    pub fn baseline() -> Self {
        Self::from_raw(
            [
                (1, 16384),
                (2, 131072),
                (3, 16777216),
                (4, 1100),
                (5, 122880),
                (6, 16384),
            ]
            .into_iter()
            .map(|(k, v)| (k, RawValue::Uint(v)))
            .collect(),
        )
        .expect("fixed limits")
    }
    pub fn intersect(&self, other: &Self) -> Result<Self> {
        let raw = (1..=6)
            .map(|k| {
                Ok((
                    k,
                    RawValue::Uint(required::<u64>(self.raw(), k)?.min(required(other.raw(), k)?)),
                ))
            })
            .collect::<Result<RawMap>>()?;
        Self::from_raw(raw)
    }
}
fn validate_record(raw: &RawMap) -> Result<()> {
    RecordBody::from_raw(required(raw, 1)?)?;
    Ok(())
}
fn validate_state(raw: &RawMap) -> Result<()> {
    if RawValue::Map(raw.clone()).encoded_len()? > crate::v1::limits::CONFIG_STATE {
        return Err(ErrorCode::LimitExceeded.into());
    }
    if required::<Id16>(raw, 1)? != required::<Roster>(raw, 3)?.group()? {
        return Err(ErrorCode::AuthenticationFailed.into());
    }
    let registers = required::<List<Register, 256>>(raw, 2)?.0;
    let mut previous = None;
    for r in registers {
        let key = r.kind()?;
        if previous.as_ref().is_some_and(|p| p >= &key) {
            return Err(ErrorCode::MalformedFrame.into());
        }
        previous = Some(key);
    }
    Ok(())
}
fn validate_register(raw: &RawMap) -> Result<()> {
    let kind = required::<u32>(raw, 1)?;
    if kind == 0 || kind == u32::MAX {
        return Err(ErrorCode::MalformedFrame.into());
    }
    if crate::v1::registry::RESERVED_REGISTERS.contains(&kind) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    if defaulted::<bool>(raw, 5, || false)? {
        if matches!(kind, 2 | 8) {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        if !required::<RawMap>(raw, 4)?.is_empty() {
            return Err(ErrorCode::MalformedFrame.into());
        }
    }
    Ok(())
}
fn present(raw: &RawMap, key: u32) -> bool {
    raw.get(&key).is_some_and(|v| !matches!(v, RawValue::Null))
}
fn validate_request(raw: &RawMap) -> Result<()> {
    if [4, 5, 6].iter().filter(|k| present(raw, **k)).count() != 1 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_reply(raw: &RawMap) -> Result<()> {
    let state = required::<ReplyState>(raw, 4)?;
    let has_state = matches!(state.variant()?, crate::v1::schema::Open::Known(ReplyStateKind::State));
    if usize::from(has_state) + usize::from(present(raw, 5)) + usize::from(present(raw, 6)) > 1 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
pub(super) fn validate_topology(raw: &RawMap) -> Result<()> {
    if !(0.0..=50.0 / 686.0).contains(&defaulted::<f64>(raw, 12, || 8.0 / 686.0)?) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    for (key, default) in [(7, 5.0 / 686.0), (8, 10.0 / 686.0), (9, 8.0 / 686.0)] {
        if defaulted::<f64>(raw, key, || default)? < 0.0 {
            return Err(ErrorCode::UnsupportedValue.into());
        }
    }
    Ok(())
}
fn validate_advert(raw: &RawMap) -> Result<()> {
    if required::<u8>(raw, 5)? > 16 || required::<u16>(raw, 6)? == 0 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

fn validate_digest(raw: &RawMap) -> Result<()> {
    let algorithm = required::<AlgorithmId>(raw, 1)?;
    if matches!(algorithm.0.as_str(), CONTENT_DIGEST | STATE_DIGEST) && required::<Bytes<64>>(raw, 2)?.0.len() != 32 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
