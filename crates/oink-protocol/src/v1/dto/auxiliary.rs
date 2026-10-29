//! Optional game and wallpaper payloads have no implicit native serialization.
use super::*;
use crate::v1::schema::{no_validation, required, wire_map};
use crate::v1::{ErrorCode, RawMap, Result};
wire_map!(GameInstance,no_validation,{1=>authority_boot:Boot=required;2=>game_counter:u64=required;});
wire_map!(GameEvent,no_validation,{1=>game_instance:GameInstance=required;2=>sender_boot:Boot=required;3=>event_seq:u64=required;});
wire_map!(AimTarget,no_validation,{
    1=>id:u32=required;2=>device:DeviceId=required;3=>display:Text<512>=required;4=>pos:Point=required;
});
wire_map!(ScreenWallpaper,validate_wallpaper,{
    1=>display:Text<512>=required;2=>width:u16=required;3=>height:u16=required;4=>bytes:Bytes<262144>=required;
});
fn validate_wallpaper(raw: &RawMap) -> Result<()> {
    if required::<u16>(raw, 2)? == 0 || required::<u16>(raw, 3)? == 0 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
pub(super) fn validate_aim_state(raw: &RawMap) -> Result<()> {
    if !(0.5 / 686.0..=200.0 / 686.0).contains(&required::<f32>(raw, 7)?) || required::<u32>(raw, 4)? > 3_600_000 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    for target in required::<List<AimTarget, 8>>(raw, 9)?.0 {
        validate_position(&target.pos()?)?;
    }
    Ok(())
}
pub(super) fn validate_aim_click(raw: &RawMap) -> Result<()> {
    validate_position(&required::<Point>(raw, 3)?)
}
fn validate_position(point: &Point) -> Result<()> {
    if point.x()?.abs() >= 1e7 / 686.0 || point.y()?.abs() >= 1e7 / 686.0 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

// Optional notification capability. Existing v1 messages and their keys are unchanged.
wire_map!(NotifyShow,validate_notify,{
    1=>id:u64=required;2=>display:Text<512>=required;3=>level:u8=required;
    4=>title:Text<512>=required;5=>description:Text<2048>=required;
    6=>persistent:bool=required;7=>undo:bool=required;8=>kind:u8=required;
    9=>microphone_muted:Option<bool>=optional;
});
wire_map!(NotifyAction,no_validation,{1=>id:u64=required;2=>undo:bool=required;});
wire_map!(NotifyDismiss,no_validation,{1=>id:u64=required;});
fn validate_notify(raw: &RawMap) -> Result<()> {
    if required::<u8>(raw, 3)? > 2 || required::<u8>(raw, 8)? > 4 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
