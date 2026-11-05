//! Optional wallpaper payloads have no implicit native serialization.
use super::*;
use crate::v1::schema::{no_validation, required, wire_map};
use crate::v1::{ErrorCode, RawMap, Result};
wire_map!(ScreenWallpaper,validate_wallpaper,{
    1=>display:Text<512>=required;2=>width:u16=required;3=>height:u16=required;4=>bytes:Bytes<262144>=required;
});
fn validate_wallpaper(raw: &RawMap) -> Result<()> {
    if required::<u16>(raw, 2)? == 0 || required::<u16>(raw, 3)? == 0 {
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
