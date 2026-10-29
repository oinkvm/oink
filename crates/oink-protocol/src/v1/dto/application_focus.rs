//! Application focus never carries native application/window identities.
use super::*;
use crate::v1::hot::hot_map;
use crate::v1::schema::{OpenEnvelope, enum_body, no_validation, open_enum, wire_map};

wire_map!(ApplicationFocusRecord, no_validation, {
    1 => claim: ApplicationFocusClaim = required;
    2 => action: ApplicationFocusAction = required;
});
wire_map!(ApplicationFocusClaim, no_validation, {
    1 => epoch: u64 = required;
    2 => pointer: Boot = required;
    3 => scope: FocusScope = required;
});
open_enum!(ApplicationFocusActionKind {
    Observe=0, Prepare=1, Commit=2, Applied=3, Snapshot=4, Retire=5, Sample=6, Cancel=7
});
enum_body!(ApplicationFocusActionKind {
    Observe=>ApplicationFocusObservation,
    Prepare=>ApplicationFocusPrepare,
    Commit=>ApplicationFocusSnapshot,
    Applied=>ApplicationFocusApplied,
    Snapshot=>ApplicationFocusSnapshot,
    Retire=>ApplicationFocusRetire,
    Cancel=>ApplicationFocusPrepare
});
pub type ApplicationFocusAction = OpenEnvelope<ApplicationFocusActionKind>;
wire_map!(ApplicationFocusObservation, no_validation, {
    1 => generation: u64 = required;
    2 => sequence: u64 = required;
    3 => foreground: ApplicationForeground = required;
    4 => hidden_lost: bool = required;
    5 => park_operation: Option<u64> = optional;
});
open_enum!(ApplicationForeground {
    Hidden=0, Windowed=1, Fullscreen=2, OrdinaryUnknown=3, Unclassified=4, Unavailable=5
});
wire_map!(ApplicationFocusPrepare, no_validation, {
    1 => revision: u64 = required;
    2 => operation: u64 = required;
    3 => target: Boot = required;
    4 => generation: u64 = required;
    5 => sequence: u64 = required;
});
wire_map!(ApplicationFocusApplied, no_validation, {
    1 => request: ApplicationFocusPrepare = required;
    2 => result: ApplicationParkResult = required;
});
open_enum!(ApplicationParkResult {
    Confirmed=0, FullscreenExempt=1, UnknownExempt=2, Denied=3, Stale=4, Unavailable=5
});
wire_map!(ApplicationFocusSnapshot, no_validation, {
    1 => revision: u64 = required;
    2 => keyboard: Option<Boot> = optional;
    3 => phase: ApplicationFocusPhase = required;
    4 => reason: ApplicationFocusReason = required;
    5 => participants: List<Boot,16> = required;
});
open_enum!(ApplicationFocusPhase { Off=0, Preparing=1, Active=2, Suspended=3 });
open_enum!(ApplicationFocusReason {
    Disabled=0, Ready=1, NoPointer=2, PointerNotParticipating=3, AwaitingApplication=4,
    DesktopUnavailable=5, Drag=6, Game=7, CounterExhausted=8
});
wire_map!(ApplicationFocusRetire, no_validation, {
    1 => revision: u64 = required;
    2 => destination: Boot = required;
});

// The accepted snapshot supplies the full scope. This token must match its
// pointer Boot, epoch, revision and destination before a terminal key is used.
// press_id fences delayed UP/repeat from an earlier press of the same key.
hot_map!(ApplicationKeyRoute, no_validation, {
    1 => epoch: u64 = required;
    2 => pointer: Boot = required;
    3 => revision: u64 = required;
    4 => destination: Boot = required;
    5 => press_id: u64 = required;
});

// Physical identity plus press identity; no application or native window IDs.
hot_map!(ApplicationHeldKey, no_validation, {
    1 => code: u32 = required;
    2 => press_id: u64 = required;
});

// Group register kind 8. Missing register/value is default-off; device capability
// still gates execution independently of this group-wide choice.
wire_map!(ApplicationFocusSettings, no_validation, {
    1 => enabled: bool = false;
});
