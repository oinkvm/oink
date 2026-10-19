//! Backend-authored presentation uses codes; human labels and opaque identities
//! remain text. UI/native menus translate codes with the same locale catalogue.
use super::*;
use crate::v1::schema::{no_validation, open_enum, wire_map};
wire_map!(UserFacingProblem,no_validation,{
    1=>code:UserFacingCode=required;2=>native_code:Option<u32>=optional;3=>retry_after_ms:Option<u32>=optional;
});
open_enum!(UserFacingCode {
    Unspecified=0,PermissionRequired=1,PermissionDenied=2,CaptureUnavailable=3,InjectionUnavailable=4,
    SecureInput=5,DeviceUnavailable=6,CleanupFailed=7,Unsupported=8,BackendFailed=9,
    LinkLost=10,Timeout=11,StorageFull=12,InvalidValue=13,StateConflict=14,LocalServiceUnavailable=15
});
open_enum!(AudioPhase {Idle=0,Preparing=1,Forwarding=2,Receiving=3,SoundSleep=4,WaitingForExit=5,Paused=6,Failed=7});

// Old LowUnavailable.reason key3 contained TCP stage/detail display strings.
wire_map!(LowUnavailable,no_validation,{
    1=>from:DeviceId=required;2=>to:DeviceId=required;4=>code:LowFailureCode=required;
    5=>stage:Option<TcpStage>=optional;6=>native_code:Option<u32>=optional;
});
open_enum!(LowFailureCode {NoDirectNeighbor=0,ConnectFailed=1,TlsFailed=2,PeerClosed=3,Timeout=4,Unsupported=5});
open_enum!(TcpStage {Connect=0,Tls=1,Admission=2,Open=3,Read=4,Write=5});
