use super::*;
use crate::v1::schema::{OpenEnvelope, defaulted, enum_body, no_validation, open_enum, required, wire_map};
use crate::v1::{ErrorCode, RawMap, Result};

wire_map!(PairContext, validate_pair_context, {
    1 => pair_id: PairId = required;
    2 => initiator: Boot = required;
    3 => responder: Boot = required;
    4 => source_group: Id16 = required;
    5 => target_group: Id16 = required;
    6 => source_count: u8 = required;
    7 => target_count: u8 = required;
    8 => destination_group: Id16 = required;
    9 => negotiation_hash: Hash32 = required;
    10 => pairing_suite: AlgorithmId = required;
    11 => target_root_id: Hash32 = required;
    12 => source_root_id: Hash32 = required;
});

wire_map!(PairPake, validate_attempt, {
    1 => pair_id: PairId = required;
    2 => suite: AlgorithmId = required;
    3 => step: PairStep = required;
    4 => attempt: u8 = required;
});

wire_map!(PakeStart, no_validation, {
    1 => context: PairContext = required;
    2 => message: Bytes<1024> = required;
});

wire_map!(PakeReply, no_validation, {
    1 => message: Bytes<1024> = required;
    2 => confirmation: Hash32 = required;
});

wire_map!(PakeFinish, no_validation, {
    1 => confirmation: Hash32 = required;
});

wire_map!(PairRequest, no_validation, {
    1 => pair_id: PairId = required;
    2 => context: PairContext = required;
});

wire_map!(PairQueued, no_validation, {
    1 => pair_id: PairId = required;
});

wire_map!(PairAccept, no_validation, {
    1 => pair_id: PairId = required;
});

wire_map!(PairDeclined, no_validation, {
    1 => pair_id: PairId = required;
    2 => reason: PairProblem = required;
});

wire_map!(PairFeedback, no_validation, {
    1 => pair_id: PairId = required;
    2 => attempt: u8 = required;
    3 => feedback: PairFeedbackKind = required;
    4 => attempts_left: u8 = required;
});

wire_map!(PairCancel, no_validation, {
    1 => pair_id: PairId = required;
    2 => reason: PairProblem = required;
});

wire_map!(PairEnded, validate_pair_ended, {
    1 => pair_id: Option<PairId> = optional;
    2 => problem: PairProblem = required;
    3 => winner: Option<PairId> = optional;
    4 => lookup: Option<GroupRef> = optional;
});

wire_map!(PairPrepare, no_validation, {
    1 => pair_id: PairId = required;
    2 => seed: MergeSeed = required;
    3 => source: ObjectRef = required;
});

wire_map!(PairReady, no_validation, {
    1 => pair_id: PairId = required;
    2 => intent: MergeIntent = required;
    3 => plan: ObjectRef = required;
});

wire_map!(PairCommitPermit, no_validation, {
    1 => pair_id: PairId = required;
    2 => intent_digest: Hash32 = required;
    3 => decision: u64 = required;
    4 => plan_digest: Hash32 = required;
});

wire_map!(PairCommit, no_validation, {
    1 => pair_id: PairId = required;
    2 => intent_digest: Hash32 = required;
    3 => source_authorization: Signature = required;
    4 => permit_decision: u64 = required;
    5 => plan_digest: Hash32 = required;
});

wire_map!(PairCommitted, no_validation, {
    1 => pair_id: PairId = required;
    2 => transfer: ObjectRef = required;
});

wire_map!(MergeLookup, no_validation, {
    1 => source_group: Id16 = required;
    2 => source_root: Hash32 = required;
    3 => requester: DeviceId = required;
    4 => after: Option<Hash32> = optional;
});

wire_map!(MergeTransfer, no_validation, {
    1 => bundle: ObjectRef = required;
});

wire_map!(MergeReceipt, no_validation, {
    1 => merge_id: Hash32 = required;
    2 => adopter: DeviceId = required;
});

wire_map!(ObjectRef, validate_object_ref, {
    1 => digest: Hash32 = required;
    2 => bytes: u32 = required;
    3 => kind: ObjectKind = required;
});

wire_map!(PairObjectBegin, no_validation, {
    1 => context: ObjectContext = required;
    2 => object: ObjectRef = required;
});

wire_map!(PairObjectChunk, no_validation, {
    1 => digest: Hash32 = required;
    2 => offset: u32 = required;
    3 => bytes: Bytes<16384> = required;
});

wire_map!(PairObjectEnd, no_validation, {
    1 => digest: Hash32 = required;
});

wire_map!(PairDecisionQuery, no_validation, {
    1 => pair_id: PairId = required;
    2 => intent_digest: Hash32 = required;
    3 => permit_decision: u64 = required;
});

wire_map!(PairDecisionReply, no_validation, {
    1 => pair_id: PairId = required;
    2 => intent_digest: Hash32 = required;
    3 => outcome: Decision = required;
});

wire_map!(DecisionCommitted, no_validation, {
    1 => bundle: ObjectRef = required;
});

wire_map!(DecisionAborted, no_validation, {
    1 => problem: PairProblem = required;
});

wire_map!(ObjectPairContext, no_validation, {
    1 => pair_id: PairId = required;
});

wire_map!(ObjectGroupContext, no_validation, {
    1 => group: GroupRef = required;
});

wire_map!(GroupRef, no_validation, {
    1 => group: Id16 = required;
    2 => root: Hash32 = required;
});

wire_map!(MemberImport, no_validation, {
    1 => device: DeviceId = required;
    2 => source_add: Hash32 = required;
    3 => target_epoch: u64 = required;
});

wire_map!(MergeSeed, no_validation, {
    1 => merge_id: Hash32 = required;
    2 => source: GroupRef = required;
    3 => target: GroupRef = required;
    4 => source_transition_sequence: u64 = required;
    5 => source_state_digest: Hash32 = required;
});

wire_map!(MergeIntent, no_validation, {
    1 => merge_id: Hash32 = required;
    2 => pair_id: PairId = required;
    3 => source: GroupRef = required;
    4 => target: GroupRef = required;
    5 => source_issuer: DeviceId = required;
    6 => source_issuer_ref: Hash32 = required;
    7 => target_issuer: DeviceId = required;
    8 => target_issuer_ref: Hash32 = required;
    9 => source_roster_digest: Hash32 = required;
    10 => source_state_digest: Hash32 = required;
    11 => target_base_digest: Hash32 = required;
    12 => members: List<MemberImport,16> = required;
    13 => source_transition_sequence: u64 = required;
});

const MANUAL_LAYOUT: LayoutSource = LayoutSource::Manual;
// Key 2 remains reserved. Unknown geometry survives decoding but must not be transformed.
wire_map!(GeometryPatch, no_validation, {
    1 => appended_devices: List<DevicePlacement,16> = required;
    3 => translation: Point = required;
    4 => source: LayoutSource = MANUAL_LAYOUT;
});

wire_map!(MergePlan, no_validation, {
    1 => intent: MergeIntent = required;
    3 => geometry_patch: GeometryPatch = required;
    4 => unsigned_adds: List<RecordBody,16> = required;
    5 => target_base: GroupState = required;
    6 => expected_result_registers: List<Register,256> = required;
});

wire_map!(MergeCertificate, no_validation, {
    1 => intent: MergeIntent = required;
    2 => plan_digest: Hash32 = required;
    3 => result_digest: Hash32 = required;
    4 => source_signature: Signature = required;
    5 => target_signature: Signature = required;
    6 => permit_coordinator: DeviceId = required;
    7 => permit_decision: u64 = required;
});

wire_map!(MergeTransferBundle, no_validation, {
    1 => certificate: MergeCertificate = required;
    2 => source_roster: Roster = required;
    3 => target_state: GroupState = required;
    4 => plan: MergePlan = required;
});

wire_map!(PairProblem, no_validation, {
    1 => code: PairCode = required;
    2 => stage: PairStage = required;
    3 => retry_after_ms: Option<u32> = optional;
    4 => attempts_left: Option<u8> = optional;
    5 => winner: Option<PairId> = optional;
    6 => needed: Option<u32> = optional;
    7 => limit: Option<u32> = optional;
    8 => protocol_code: Option<u16> = optional;
    9 => limit_kind: Option<PairLimitKind> = optional;
    10 => cause: Option<PairCode> = optional;
});

wire_map!(PairReceipt, no_validation, {
    1 => pair_id: PairId = required;
    2 => merge_id: Hash32 = required;
    3 => state_digest: Hash32 = required;
    4 => reply: bool = false;
});
open_enum!(PairFeedbackKind { Waiting=0,WrongCode=1,Verified=2 });
open_enum!(ObjectKind { SourceState=0,Plan=1,Transfer=2 });
open_enum!(PairStage { Address=0,Connect=1,Negotiate=2,Request=3,Code=4,Prepare=5,Commit=6,Adopt=7,Undo=8 });
open_enum!(PairLimitKind { Bytes=0,Members=1,RosterRecords=2,Registers=3,Screens=4,Staging=5,PendingRequests=7 });
open_enum!(PairCode {
    InvalidAddress=0,InvalidPort=1,ScopeRequired=2,SelfDevice=3,Unreachable=4,
    ConnectionRefused=5,ConnectTimeout=6,NotOink=7,IdentityChanged=8,
    UnsupportedProtocol=9,MissingCapability=10,QueueFull=11,RateLimited=12,
    Declined=13,CancelledByPeer=14,CancelledLocally=15,PeerClosed=16,LinkLost=17,
    InvalidCodeSyntax=18,WrongCode=19,TooManyAttempts=20,PairClosed=21,
    AlreadyConnected=22,SupersededByPairing=23,GroupChanged=24,MemberLimitExceeded=25,
    MergeTooLarge=26,RosterInvalid=27,MergeConflict=28,
    LocalStorageFailed=30,CommitUnconfirmed=31,MergeProofUnavailable=32,
    MembershipRevoked=33,UndoStale=34,PermissionDenied=35,PreparedPeerLost=36,
    StaleRevision=37,ProtocolViolation=38,StateChanged=39,AlreadyCommitted=40,
    AuthenticationFailed=41,NoCommonCrypto=42,UnsupportedLayout=43,
    LocalServiceUnavailable=44,NetworkTimeout=45,TooManyPendingRequests=46,
});
open_enum!(PairStepKind { Start=0,Reply=1,Finish=2 });
enum_body!(PairStepKind { Start=>PakeStart,Reply=>PakeReply,Finish=>PakeFinish });
pub type PairStep = OpenEnvelope<PairStepKind>;
open_enum!(DecisionKind { Pending=0,Committed=1,Aborted=2 });
enum_body!(DecisionKind { Committed=>DecisionCommitted,Aborted=>DecisionAborted });
pub type Decision = OpenEnvelope<DecisionKind>;
open_enum!(ObjectContextKind { PairId=0,GroupRef=1 });
enum_body!(ObjectContextKind { PairId=>ObjectPairContext,GroupRef=>ObjectGroupContext });
pub type ObjectContext = OpenEnvelope<ObjectContextKind>;
fn validate_pair_context(raw: &RawMap) -> Result<()> {
    if required::<Id16>(raw, 5)? != required::<Id16>(raw, 8)? {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_attempt(raw: &RawMap) -> Result<()> {
    if !(1..=3).contains(&required::<u8>(raw, 4)?) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
fn validate_pair_ended(raw: &RawMap) -> Result<()> {
    let pair: Option<PairId> = defaulted(raw, 1, || None)?;
    let group: Option<GroupRef> = defaulted(raw, 4, || None)?;
    if pair.is_some() == group.is_some() {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_object_ref(raw: &RawMap) -> Result<()> {
    let bytes = required::<u32>(raw, 2)?;
    if bytes == 0 || bytes as usize > crate::v1::limits::PAIR_OBJECT {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}
