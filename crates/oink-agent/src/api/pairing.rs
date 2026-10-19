//! Local projections of pairing state. Wire DTOs remain the authoritative stored
//! and transported values; these types only cross authenticated local IPC.
use super::{DeviceKey, GroupKey};
use crate::device_cards::DecimalU64;
use serde::{Deserialize, Serialize};

/// Opaque base64url-encoded deterministic PairId DTO, including initiator Boot.
pub type PairId = String;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRef {
    pub group: GroupKey,
    pub root: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocketAddress {
    pub family: u32,
    pub address: Vec<u8>,
    pub port: u16,
    pub interface_scope: u32,
}
impl From<std::net::SocketAddr> for SocketAddress {
    fn from(value: std::net::SocketAddr) -> Self {
        match value {
            std::net::SocketAddr::V4(value) => Self {
                family: 0,
                address: value.ip().octets().to_vec(),
                port: value.port(),
                interface_scope: 0,
            },
            std::net::SocketAddr::V6(value) => Self {
                family: 1,
                address: value.ip().octets().to_vec(),
                port: value.port(),
                interface_scope: value.scope_id(),
            },
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerView {
    pub device: DeviceKey,
    pub name: String,
    pub os: String,
    pub remote: SocketAddress,
    pub listen: Option<SocketAddress>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairEndpoint {
    pub address: SocketAddress,
    pub expected_id: Option<DeviceKey>,
    pub verified: Option<PeerView>,
}
/// Local network diagnostics; these do not add device-to-device protocol codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressNetworkProblem {
    NoRoute,
    NoNetworkInterface,
    PeerUnreachable,
    SocketError,
    NetworkStopped,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairProblem {
    pub code: u32,
    pub stage: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_problem: Option<AddressNetworkProblem>,
    pub retry_after_ms: Option<u32>,
    pub attempts_left: Option<u8>,
    pub winner: Option<PairId>,
    pub needed: Option<u32>,
    pub limit: Option<u32>,
    pub protocol_code: Option<u16>,
    pub limit_kind: Option<u32>,
    pub cause: Option<u32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityEvidence {
    Advertised,
    TlsVerified,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredGroupRef {
    pub group: GroupKey,
    pub root: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearbyRow {
    pub device: DeviceKey,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearbySnapshot {
    pub revision: DecimalU64,
    pub rows: Vec<NearbyRow>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Incoming,
    Outgoing,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feedback {
    Waiting,
    Correct,
    Wrong,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairView {
    pub id: PairId,
    pub revision: DecimalU64,
    pub direction: Direction,
    pub peer: PairEndpoint,
    pub phase: u32,
    pub attempts_left: u8,
    pub feedback: Feedback,
    pub terminal: Option<PairProblem>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    pub id: PairId,
    pub revision: DecimalU64,
    pub peer: PeerView,
    pub source: GroupRef,
    pub target: GroupRef,
    pub source_count: u8,
    pub target_count: u8,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingSnapshot {
    pub revision: DecimalU64,
    pub requests: Vec<PendingRequest>,
    pub sessions: Vec<PairView>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundPeer {
    pub query_id: String,
    pub generation: DecimalU64,
    pub peer: PeerView,
    pub prepared: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoHandle {
    pub group: GroupRef,
    pub merge_id: String,
    pub peer: DeviceKey,
    pub observed_add: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PairTarget {
    NearbyDevice { device: DeviceKey },
    PreparedQuery { query_id: String, generation: DecimalU64 },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CancelResult {
    Cancelled,
    AlreadyCommitted { undo: UndoHandle },
    CommitUnconfirmed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypedResult {
    Ok,
    Problem { problem: PairProblem },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AddressQueryResult {
    Found { peer: FoundPeer },
    Problem { problem: PairProblem },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    ProofWaiting,
    Adopting,
    Adopted,
    Conflict,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMigrationStatus {
    pub source: GroupRef,
    pub target: Option<GroupRef>,
    pub phase: MigrationPhase,
    pub problem: Option<PairProblem>,
}

pub fn pair_id(value: &oink_protocol::v1::dto::PairId) -> oink_protocol::v1::Result<PairId> {
    use base64::Engine;
    use oink_protocol::v1::schema::WireDto;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.encode(8192)?))
}
pub fn wire_pair_id(value: &str) -> oink_protocol::v1::Result<oink_protocol::v1::dto::PairId> {
    use base64::Engine;
    use oink_protocol::v1::{ErrorCode, cbor, dto, schema::WireDto};
    if value.len() > 11000 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| oink_protocol::v1::Error::from(ErrorCode::MalformedFrame))?;
    dto::PairId::from_raw(cbor::decode_map(&bytes, 8192)?)
}
impl TryFrom<&oink_protocol::v1::dto::PairProblem> for PairProblem {
    type Error = oink_protocol::v1::Error;
    fn try_from(value: &oink_protocol::v1::dto::PairProblem) -> Result<Self, Self::Error> {
        Ok(Self {
            network_problem: None,
            code: value.code()?.number(),
            stage: value.stage()?.number(),
            retry_after_ms: value.retry_after_ms()?,
            attempts_left: value.attempts_left()?,
            winner: value.winner()?.as_ref().map(pair_id).transpose()?,
            needed: value.needed()?,
            limit: value.limit()?,
            protocol_code: value.protocol_code()?,
            limit_kind: value.limit_kind()?.map(|code| code.number()),
            cause: value.cause()?.map(|code| code.number()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::{
        RawValue, dto,
        schema::{FixedBytes, WireDto, WireValue},
    };
    #[test]
    fn opaque_pair_identity_preserves_boot_and_retained_fields() {
        let owner = crate::device_cards::OwnerVersion {
            group: oink_foundation::GroupId([1; 16]).into(),
            device: oink_foundation::DeviceId([2; 32]).into(),
            boot_nonce: [3; 16],
            revision: 1,
        };
        let id = dto::PairId::from_raw(
            [
                (1, crate::device_controls::requests::wire_boot(owner).write()),
                (2, FixedBytes([4; 16]).write()),
                (90, RawValue::Text("retained".into())),
            ]
            .into_iter()
            .collect(),
        )
        .unwrap();
        let opaque = pair_id(&id).unwrap();
        assert_eq!(wire_pair_id(&opaque).unwrap(), id);
        assert!(wire_pair_id("not a pair id").is_err());
    }
    #[test]
    fn unknown_pair_problem_codes_survive_local_projection() {
        let problem = dto::PairProblem::from_raw(
            [(1, 100_000u32.write()), (2, 70_000u32.write()), (10, 80_000u32.write())]
                .into_iter()
                .collect(),
        )
        .unwrap();
        let projected = PairProblem::try_from(&problem).unwrap();
        assert_eq!(
            (projected.code, projected.stage, projected.cause),
            (100_000, 70_000, Some(80_000))
        );
        assert!(projected.winner.is_none());
        let json = serde_json::to_value(projected).unwrap();
        assert_eq!(json["code"], 100_000);
        assert!(json.get("message").is_none());
    }
}

/// Business refusals are ordinary correlated reply values. Transport failure
/// alone uses the outer IPC error channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PairReply<T> {
    Problem(PairRejection),
    Value(T),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairRejection {
    Problem { problem: PairProblem },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Accepted {
    Accepted,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Started {
    Started { pair_id: PairId },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeView {
    pub six_digits: String,
}
impl std::fmt::Debug for CodeView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CodeView { six_digits: [redacted] }")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PairingEvent {
    NearbyChanged {
        revision: DecimalU64,
        upserts: Vec<NearbyRow>,
        removed: Vec<DeviceKey>,
    },
    NearbySnapshot(NearbySnapshot),
    NearbyRefreshFinished {
        operation_id: String,
        result: TypedResult,
    },
    AddressQueryFinished {
        operation_id: String,
        generation: DecimalU64,
        result: AddressQueryResult,
    },
    PairingChanged {
        revision: DecimalU64,
        view: PairView,
    },
    PairingRequestsChanged {
        revision: DecimalU64,
        requests: Vec<PendingRequest>,
    },
    GroupMigrationChanged(GroupMigrationStatus),
    PairingFailed {
        event_id: DecimalU64,
        pair_id: PairId,
        peer: PairEndpoint,
        problem: PairProblem,
    },
    PairingCompleted {
        event_id: DecimalU64,
        pair_id: PairId,
        peer: PeerView,
        merge_id: String,
        target_group: GroupRef,
        imported_devices: Vec<DeviceKey>,
        undo: UndoHandle,
        reconciled: bool,
    },
    MembershipEdited {
        operation_id: String,
        result: TypedResult,
    },
}

impl From<PairingEvent> for super::ApiEvent {
    fn from(event: PairingEvent) -> Self {
        match event {
            PairingEvent::NearbyChanged {
                revision,
                upserts,
                removed,
            } => Self::NearbyChanged {
                revision,
                upserts,
                removed,
            },
            PairingEvent::NearbySnapshot(value) => Self::NearbySnapshot(value),
            PairingEvent::NearbyRefreshFinished { operation_id, result } => {
                Self::NearbyRefreshFinished { operation_id, result }
            }
            PairingEvent::AddressQueryFinished {
                operation_id,
                generation,
                result,
            } => Self::AddressQueryFinished {
                operation_id,
                generation,
                result,
            },
            PairingEvent::PairingChanged { revision, view } => Self::PairingChanged { revision, view },
            PairingEvent::PairingRequestsChanged { revision, requests } => {
                Self::PairingRequestsChanged { revision, requests }
            }
            PairingEvent::GroupMigrationChanged(value) => Self::GroupMigrationChanged(value),
            PairingEvent::PairingFailed {
                event_id,
                pair_id,
                peer,
                problem,
            } => Self::PairingFailed {
                event_id,
                pair_id,
                peer,
                problem,
            },
            PairingEvent::PairingCompleted {
                event_id,
                pair_id,
                peer,
                merge_id,
                target_group,
                imported_devices,
                undo,
                reconciled,
            } => Self::PairingCompleted {
                event_id,
                pair_id,
                peer,
                merge_id,
                target_group,
                imported_devices,
                undo,
                reconciled,
            },
            PairingEvent::MembershipEdited { operation_id, result } => Self::MembershipEdited { operation_id, result },
        }
    }
}
