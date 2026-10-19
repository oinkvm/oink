//! Retained identities for routing and Low operations. A network owner must
//! authenticate the current connection and path before acting on these values.
use super::*;
use crate::v1::schema::{OpenEnvelope, enum_body, no_validation, open_enum, required, wire_map};
use crate::v1::{ErrorCode, RawMap, Result};

wire_map!(PathRequestId,no_validation,{
    1=>source_boot:Boot=required;2=>random:Id16=required;
});
wire_map!(NextHopBinding,no_validation,{
    1=>peer:DeviceId=required;2=>connection_id:ConnectionId=required;3=>route_tag:u64=required;
});
wire_map!(RelayReservation,no_validation,{
    1=>relay:Boot=required;2=>random:Id16=required;
    3=>source_connection:ConnectionId=required;4=>target_connection:ConnectionId=required;
});
wire_map!(MigrationId,no_validation,{
    1=>path_id:PathId=required;2=>to_revision:u64=required;
});
open_enum!(PathReferenceKind {Request=0,Installed=1});
wire_map!(RequestedPath,no_validation,{1=>request:PathRequestId=required;});
wire_map!(InstalledPath,no_validation,{1=>path:PathId=required;});
enum_body!(PathReferenceKind {Request=>RequestedPath,Installed=>InstalledPath});
pub type PathReference = OpenEnvelope<PathReferenceKind>;

wire_map!(LowBinding,no_validation,{
    1=>path_id:PathId=required;2=>route_revision:u64=required;
    3=>source:Boot=required;4=>target:Boot=required;5=>hop:NextHopBinding=required;
});
wire_map!(LowOptions,validate_low_options,{
    1=>send_max:u64=required;2=>receive_max:u64=required;3=>operation_max:u64=required;
});
wire_map!(LowRequest,no_validation,{
    1=>request:Id16=required;2=>connection:Id16=required;3=>options:Option<LowOptions>=optional;
    4=>extra:bool=required;5=>keep_alive:bool=required;6=>purpose:LowTcpPurpose=required;
    7=>paced:bool=required;8=>binding:LowBinding=required;9=>route_tag:u64=required;10=>kind:u16=required;
});

fn validate_low_options(raw: &RawMap) -> Result<()> {
    let total = required::<u64>(raw, 1)?.checked_add(required::<u64>(raw, 2)?);
    let limit = required::<u64>(raw, 3)?;
    if total.is_none_or(|total| total > limit) {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}
