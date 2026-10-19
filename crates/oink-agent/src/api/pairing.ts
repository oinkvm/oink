/** Authenticated local pairing facade. Numeric open codes are never localized by the backend. */
import type { DeviceKey, DecimalU64 } from "../device_cards/contract";

/** Opaque base64url encoding of the complete deterministic PairId DTO. */
export type PairId = string;
export type OperationId = string; // UUID
export interface GroupRef { group: string; root: string }
export interface SocketAddress {
  family: number; // 0 IPv4, 1 IPv6; current product permits IPv4
  address: number[];
  port: number;
  interface_scope: number;
}
export interface PeerView {
  device: DeviceKey;
  name: string;
  os: string;
  remote: SocketAddress;
  listen: SocketAddress | null;
}
export interface PairEndpoint {
  address: SocketAddress;
  expected_id: DeviceKey | null;
  verified: PeerView | null;
}
export interface PairProblem {
  network_problem?: "no_route" | "no_network_interface" | "peer_unreachable" | "socket_error" | "network_stopped";
  code: number;
  stage: number;
  retry_after_ms: number | null;
  attempts_left: number | null;
  winner: PairId | null;
  needed: number | null;
  limit: number | null;
  protocol_code: number | null;
  limit_kind: number | null;
  cause: number | null;
}
export interface NearbyRow {
  device: DeviceKey;
  name: string;
  os: string;
  group: { group: string; root: string | null };
  count: number | null;
  addresses: SocketAddress[];
  selected: SocketAddress;
  source: number;
  identity: "advertised" | "tls_verified";
  reachable: boolean; compatible: boolean; problem: PairProblem | null;
}
export interface NearbySnapshot { revision: DecimalU64; rows: NearbyRow[] }
export interface PairView {
  id: PairId;
  revision: DecimalU64;
  direction: "incoming" | "outgoing";
  peer: PairEndpoint;
  /** 0 connecting,1 show_code,2 enter_code,3 checking_code,4 preparing,
   * 5 committing,6 admitting,7 completed,8 ended. Preserve unknown values. */
  phase: number;
  attempts_left: number;
  feedback: "waiting" | "correct" | "wrong";
  terminal: PairProblem | null;
}
export interface PendingRequest {
  id: PairId;
  revision: DecimalU64;
  peer: PeerView;
  source: GroupRef;
  target: GroupRef;
  source_count: number;
  target_count: number;
}
export interface PairingSnapshot {
  revision: DecimalU64;
  requests: PendingRequest[];
  sessions: PairView[];
}
export interface FoundPeer {
  query_id: string;
  generation: DecimalU64;
  peer: PeerView;
  prepared: boolean;
}
export interface UndoHandle {
  group: GroupRef;
  merge_id: string;
  peer: DeviceKey;
  observed_add: string;
}
export interface GroupMigrationStatus {
  source: GroupRef;
  target: GroupRef | null;
  phase: "proof_waiting" | "adopting" | "adopted" | "conflict";
  problem: PairProblem | null;
}
export type PairTarget =
  | { type: "nearby_device"; device: DeviceKey }
  | { type: "prepared_query"; query_id: string; generation: DecimalU64 };
export type PairReply<T> = T | { type: "problem"; problem: PairProblem };
export type Accepted = { type: "accepted" };
export type Started = { type: "started"; pair_id: PairId };
export type CancelResult =
  | { type: "cancelled" }
  | { type: "already_committed"; undo: UndoHandle }
  | { type: "commit_unconfirmed" };
export type TypedResult = { type: "ok" } | { type: "problem"; problem: PairProblem };
export type PairingRequest =
  | { method: "nearby" }
  | { method: "refresh_nearby"; params: { operation_id: OperationId } }
  | { method: "query_pairing_address"; params: { operation_id: OperationId;
      generation: DecimalU64; ip: string; port?: number } }
  | { method: "release_pairing_query"; params: { query_id: string; generation: DecimalU64 } }
  | { method: "start_pairing"; params: { operation_id: OperationId; target: PairTarget } }
  | { method: "group_migration" }
  | { method: "pairings" }
  | { method: "accept_pairing" | "decline_pairing" | "cancel_pairing";
      params: { pair_id: PairId; expected_revision: DecimalU64 } }
  | { method: "pairing_code"; params: { pair_id: PairId } }
  | { method: "submit_pairing_code"; params: { pair_id: PairId;
      expected_revision: DecimalU64; attempt: number; code: string } }
  | { method: "undo_pairing"; params: { operation_id: OperationId; handle: UndoHandle } }
  | { method: "subscribe_pairing_events"; params: { after_event_id: DecimalU64 } }
  | { method: "ack_pairing_event"; params: { event_id: DecimalU64 } };
export type PairingEvent =
  | { type: "nearby_changed"; data: { revision: DecimalU64; upserts: NearbyRow[]; removed: DeviceKey[] } }
  | { type: "nearby_snapshot"; data: NearbySnapshot }
  | { type: "nearby_refresh_finished"; data: { operation_id: OperationId; result: TypedResult } }
  | { type: "address_query_finished"; data: { operation_id: OperationId; generation: DecimalU64;
      result: { type: "found"; peer: FoundPeer } | { type: "problem"; problem: PairProblem } } }
  | { type: "pairing_changed"; data: { revision: DecimalU64; view: PairView } }
  | { type: "pairing_requests_changed"; data: { revision: DecimalU64; requests: PendingRequest[] } }
  | { type: "group_migration_changed"; data: GroupMigrationStatus }
  | { type: "pairing_failed"; data: { event_id: DecimalU64; pair_id: PairId;
      peer: PairEndpoint; problem: PairProblem } }
  | { type: "pairing_completed"; data: { event_id: DecimalU64; pair_id: PairId; peer: PeerView;
      merge_id: string; target_group: GroupRef; imported_devices: DeviceKey[]; undo: UndoHandle; reconciled: boolean } }
  | { type: "membership_edited"; data: { operation_id: OperationId; result: TypedResult } };
