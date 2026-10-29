//! Demand-driven microphone sessions use the existing audio stream and codecs.
//! Claims use the same ordering primitive as speaker exits, in an independent
//! namespace. A microphone claim always names the source, never the receiver.
use super::business::validate_packet;
use super::*;
use crate::v1::hot::hot_map;
use crate::v1::schema::{no_validation, open_enum, wire_map};

open_enum!(MicrophonePermission {
    Granted=0,NotDetermined=1,Denied=2,Restricted=3,SystemControlled=4,Unsupported=5
});
wire_map!(MicrophoneInput, no_validation, {
    1 => id: Text<512> = required;
    2 => name: Text<256> = required;
});
wire_map!(MicrophoneObservation, no_validation, {
    1 => permission: MicrophonePermission = required;
    2 => virtual_ready: bool = required;
    3 => inputs: List<MicrophoneInput,64> = required;
    4 => input: Option<Text<512>> = optional;
    5 => consumer_active: bool = required;
    6 => capture_active: bool = required;
    7 => receiving_from: Option<DeviceId> = optional;
    8 => fault: Option<ResourceFault> = optional;
});
// The random request belongs to one actual virtual-input consumer lifetime.
// Source/receiver boots, output generation and source claim must all still match
// at every transition. Renew never creates a session and media never renews it.
wire_map!(MicrophoneSession, no_validation, {
    1 => source: Boot = required;
    2 => receiver_output: ReceiverOutputBinding = required;
    3 => share_claim: ExitClaimStamp = required;
    4 => request: Id16 = required;
});
wire_map!(MicrophoneDemand, no_validation, {
    1 => session: MicrophoneSession = required;
    2 => formats: List<AudioDescriptor,8> = required;
});
wire_map!(MicrophonePrepared, no_validation, {
    1 => session: MicrophoneSession = required;
    2 => stream_id: u64 = required;
    3 => descriptor: AudioDescriptor = required;
});
wire_map!(MicrophoneReady, no_validation, {
    1 => session: MicrophoneSession = required;
    2 => stream_id: u64 = required;
});
wire_map!(MicrophoneRenew, no_validation, {
    1 => session: MicrophoneSession = required;
    2 => stream_id: u64 = required;
});
wire_map!(MicrophoneStop, no_validation, {
    1 => session: MicrophoneSession = required;
    2 => reason: ControlFailure = required;
});
// Identical media bodies, separately capability-gated kinds on the same M1.
// The discriminator permits bounded demultiplexing before either native sink
// consumes a frame, including simultaneous speaker and microphone from a peer.
hot_map!(MicrophonePacket, validate_packet, {
    1 => stream_id: u64 = required;
    2 => sequence: u64 = required;
    3 => burst: u64 = required;
    4 => source_time_ns: u64 = required;
    5 => silent: bool = required;
    6 => payload: Bytes<960> = required;
});
hot_map!(MicrophoneSleep, no_validation, {
    1 => stream_id: u64 = required;
    2 => burst: u64 = required;
    3 => sequence: u64 = required;
});
