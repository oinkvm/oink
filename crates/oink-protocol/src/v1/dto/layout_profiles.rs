//! Optional layout-profile capability. Baseline GroupState/LayoutValue stay frozen.
use super::*;
use crate::v1::schema::{no_validation, wire_map};
use crate::v1::{ErrorCode, RawMap, RawValue, Result};

wire_map!(LayoutProfilesState, no_validation, {
    1 => group: Id16 = required;
    2 => catalog: LayoutProfileCatalog = required;
    3 => merge: Option<MergeTransferBundle> = optional;
    4 => layout_version: Option<ConfigVersion> = optional;
});
wire_map!(LayoutProfilesReceipt, no_validation, {
    1 => group: Id16 = required;
    3 => accepted: bool = required;
});
wire_map!(LayoutProfileCatalog, no_validation, {
    1 => schema: u32 = required;
    2 => group: Id16 = required;
    3 => version: ConfigVersion = required;
    4 => active: Id16 = required;
    5 => profiles: List<LayoutProfile,8> = required;
    6 => imported_merges: List<Hash32,256> = required;
});
wire_map!(LayoutProfile, no_validation, {
    1 => id: Id16 = required;
    2 => name: Option<Text<128>> = optional;
    3 => content_revision: ConfigVersion = required;
    4 => last_used: LayoutProfileRecency = required;
    5 => content: LayoutProfileSnapshot = required;
    6 => state: LayoutProfileDeviceState = required;
    7 => default_group: Option<Id16> = optional;
    8 => physical_topology: Option<List<LayoutPhysicalDevice,16>> = optional;
    9 => physical_topology_revision: Option<ConfigVersion> = optional;
});
wire_map!(LayoutProfileRecency, no_validation, {
    1 => logical_ms: u64 = required;
    2 => writer: DeviceId = required;
    3 => sequence: u64 = required;
});
wire_map!(LayoutProfileDeviceState, no_validation, {
    1 => online: List<DeviceId,16> = required;
    2 => devices: List<LayoutProfileScreens,16> = required;
});
wire_map!(LayoutProfileScreens, no_validation, {
    1 => device: DeviceId = required;
    2 => displays: Option<List<Text<512>,256>> = optional;
});
wire_map!(LayoutProfileSnapshot, no_validation, {
    1 => placements: List<LayoutProfilePlacement,16> = required;
    2 => disabled: List<LayoutProfileDisabled,256> = required;
    3 => native: List<LayoutProfileNative,16> = required;
    4 => topology: TopologySettings = required;
    5 => screen_shared: Option<List<LayoutProfileSharing,16>> = optional;
});
wire_map!(LayoutProfileSharing, no_validation, {
    1 => device: DeviceId = required;
    2 => shared: bool = required;
});
wire_map!(LayoutProfilePlacement, no_validation, {
    1 => device: DeviceId = required;
    2 => origin: Point = required;
    3 => scale: f64 = required;
});
wire_map!(LayoutProfileDisabled, no_validation, {
    1 => screen: ScreenId = required;
    2 => disabled: bool = required;
});
wire_map!(LayoutProfileNative, no_validation, {
    1 => device: DeviceId = required;
    2 => primary: ScreenId = required;
    3 => screens: List<LayoutProfileScreen,256> = required;
    4 => token: Option<Text<128>> = optional;
});
wire_map!(LayoutProfileScreen, no_validation, {
    1 => screen: ScreenId = required;
    2 => origin: Point = required;
    3 => size: Size = required;
});
wire_map!(LayoutArrangementRequest, no_validation, {
    1 => request_id: Id16 = required;
    2 => target: Boot = required;
    3 => status_only: bool = false;
    4 => native_token: Option<Text<128>> = optional;
    5 => preference_revision: Option<u64> = optional;
    6 => screen_revision: Option<u64> = optional;
    7 => origins: List<LayoutNativeOrigin,256> = (List(Vec::new()));
    8 => disabled: List<LayoutProfileDisabled,256> = (List(Vec::new()));
});
wire_map!(LayoutNativeOrigin, no_validation, {
    1 => display: Text<512> = required;
    2 => origin: Point = required;
});
wire_map!(LayoutArrangementResult, no_validation, {
    1 => request_id: Id16 = required;
    2 => target: Boot = required;
    3 => outcome: u32 = required;
    4 => native: Option<LayoutProfileNative> = optional;
    5 => error: Option<Text<64>> = optional;
    6 => preferences: List<DevicePreference,32> = (List(Vec::new()));
    7 => layout_version: Option<ConfigVersion> = optional;
    8 => fact_sequence: Option<u64> = optional;
});

// Identical arrangement DTO to durable LayoutProfile.content; this envelope alone
// determines ephemeral semantics. Absence of arrangement terminates the lease.
wire_map!(ArrangementPreview, validate_arrangement_preview, {
    1 => is_preview: bool = required;
    2 => sequence: u64 = required;
    3 => lease_ms: u32 = required;
    4 => expires_ms: u64 = required;
    5 => arrangement: Option<LayoutProfileSnapshot> = optional;
});
fn validate_arrangement_preview(raw: &RawMap) -> Result<()> {
    if raw.get(&1) != Some(&RawValue::Bool(true)) || !(1..=2000).contains(&crate::v1::schema::required::<u32>(raw, 3)?)
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

// Additive 0.2 catalog envelope. The /1 catalog and its eight-entry bound stay frozen.
wire_map!(LayoutProfilesStateV2, no_validation, {
    1 => group: Id16 = required;
    2 => catalog: LayoutProfileCatalogV2 = required;
    3 => merge: Option<MergeTransferBundle> = optional;
    4 => layout_version: Option<ConfigVersion> = optional;
});
wire_map!(LayoutProfileCatalogV2, no_validation, {
    1 => schema: u32 = required;
    2 => group: Id16 = required;
    3 => version: ConfigVersion = required;
    4 => active: Id16 = required;
    5 => profiles: List<LayoutProfile,16> = required;
    6 => imported_merges: List<Hash32,256> = required;
    7 => discarded_defaults: Option<List<Id16,512>> = optional;
    8 => default_imports: Option<List<Hash32,256>> = optional;
});

// Acceptor-only joins. No profile import proof is carried on this stream.
wire_map!(LayoutProfilesStateV3, no_validation, {
    1 => group: Id16 = required;
    2 => catalog: LayoutProfileCatalogV3 = required;
    4 => layout_version: Option<ConfigVersion> = optional;
});
wire_map!(LayoutProfileCatalogV3, no_validation, {
    1 => schema: u32 = required;
    2 => group: Id16 = required;
    3 => version: ConfigVersion = required;
    4 => active: Id16 = required;
    5 => profiles: List<LayoutProfile,16> = required;
    9 => completed_joins: List<LayoutProfileJoinCompletion,256> = (List(Vec::new()));
});
wire_map!(LayoutProfileJoinCompletion, no_validation, {
    1 => merge_id: Hash32 = required;
    2 => version: ConfigVersion = required;
    3 => default_profile: Option<Id16> = optional;
    4 => dropped_defaults: List<Id16,16> = required;
});
wire_map!(LayoutProfilesJoin, no_validation, {
    1 => group: Id16 = required;
    2 => merge_id: Hash32 = required;
    3 => catalog: LayoutProfileCatalogV3 = required;
});

// Presence, names and native observation tokens are intentionally absent.
wire_map!(LayoutPhysicalDevice, no_validation, {
    1 => device: DeviceId = required;
    2 => screens: List<LayoutPhysicalScreen,256> = required;
});
wire_map!(LayoutPhysicalScreen, no_validation, {
    1 => id: Text<512> = required;
    2 => bounds: Rect = required;
    3 => pixel_width: u32 = required;
    4 => pixel_height: u32 = required;
    5 => scale: f64 = required;
    6 => rotation: u16 = required;
    7 => primary: bool = required;
});
