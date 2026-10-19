//! Exact domain separation for authenticated transcripts and diagnostic aliases.
//! A trailing NUL belongs to cryptographic byte labels; BLAKE3 contexts are text.
pub const NEGOTIATION: &[u8] = b"oink/v1/negotiation\0";
pub const ROSTER_RECORD: &[u8] = b"oink/v1/roster/record\0";
pub const ROSTER_SIGNATURE: &[u8] = b"oink/v1/roster/signature\0";
pub const PAIR_TRANSCRIPT: &[u8] = b"oink/v1/pair/transcript\0";
pub const PAIR_CONFIRMATION: &[u8] = b"oink/v1/pair/confirmation\0";
pub const MERGE_AUTHORIZE: &[u8] = b"oink/v1/merge/authorize\0";
pub const MERGE_COMMITTED: &[u8] = b"oink/v1/merge/committed\0";
pub const DIAGNOSTICS_GROUP: &str = "oink/v1/diagnostics/group";
pub const DIAGNOSTICS_INDEPENDENT: &str = "oink/v1/diagnostics/independent";

/// Exported with the registry so compatibility checks also protect exact labels.
pub const ALL: &[(&str, &[u8])] = &[
    ("negotiation", NEGOTIATION),
    ("roster_record", ROSTER_RECORD),
    ("roster_signature", ROSTER_SIGNATURE),
    ("pair_transcript", PAIR_TRANSCRIPT),
    ("pair_confirmation", PAIR_CONFIRMATION),
    ("merge_authorize", MERGE_AUTHORIZE),
    ("merge_committed", MERGE_COMMITTED),
    ("diagnostics_group", DIAGNOSTICS_GROUP.as_bytes()),
    ("diagnostics_independent", DIAGNOSTICS_INDEPENDENT.as_bytes()),
    ("quic_alpn", super::QUIC_ALPN),
    ("low_alpn", super::LOW_ALPN),
    ("discovery_service", super::DISCOVERY_SERVICE.as_bytes()),
];
