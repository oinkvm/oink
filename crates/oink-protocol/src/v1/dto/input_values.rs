//! Typed input values. Native conversions belong to capture, routing and injection owners.
use super::*;
use crate::v1::hot::{BorrowValue, HotDto, Map, View, hot_map};
use crate::v1::schema::{OpenEnvelope, enum_body, no_validation, open_enum, required, wire_map};
use crate::v1::{ErrorCode, RawMap, Result};

open_enum!(MouseButtonKind {Left=0,Right=1,Middle=2,Back=3,Forward=4,Other=5});
hot_map!(OtherMouseButton,no_validation,{1=>number:u8=required;});
enum_body!(MouseButtonKind {Other=>OtherMouseButton});
pub type MouseButton = OpenEnvelope<MouseButtonKind>;

open_enum!(GestureKind {DockSwipe=0,Magnify=1,Rotate=2,SmartMagnify=3});
open_enum!(GesturePhase {Began=0,Changed=1,Ended=2,Cancelled=3});
open_enum!(SwipeAxis {Horizontal=0,Vertical=1,Pinch=2});
hot_map!(DockSwipe,no_validation,{
    1=>axis:SwipeAxis=required;2=>phase:GesturePhase=required;3=>progress:f64=required;4=>velocity:f64=required;
});
hot_map!(Magnify,no_validation,{1=>phase:GesturePhase=required;2=>amount:f64=required;});
hot_map!(Rotate,no_validation,{1=>phase:GesturePhase=required;2=>degrees:f64=required;});
enum_body!(GestureKind {DockSwipe=>DockSwipe,Magnify=>Magnify,Rotate=>Rotate});
pub type Gesture = OpenEnvelope<GestureKind>;

macro_rules! hot_envelope {
    ($name:ident,$kind:ident,{$($variant:ident=>$body:ident),*}) => {
        impl HotDto for $name {
            fn validate(view: &View<'_, Self>) -> Result<()> {
                let body = view.body()?;
                match view.kind()? {
                    $($kind::$variant => { View::<$body>::from_map(body)?; },)*
                    _ => {}
                }
                Ok(())
            }
        }
        impl<'a> View<'a, $name> {
            pub fn kind(&self) -> Result<$kind> { self.required::<$kind>(1) }
            pub fn body(&self) -> Result<Map<'a>> {
                match self.field(2)? {
                    Some(bytes) => RawMap::read_borrowed(bytes),
                    None => Map::parse(&[0xa0], 1),
                }
            }
        }
    };
}
hot_envelope!(MouseButton,MouseButtonKind,{Other=>OtherMouseButton});
hot_envelope!(Gesture,GestureKind,{DockSwipe=>DockSwipe,Magnify=>Magnify,Rotate=>Rotate});

hot_map!(ContactSurface,no_validation,{1=>width:u32=required;2=>height:u32=required;});
hot_map!(ContactPoint,no_validation,{1=>id:u32=required;2=>x:i32=required;3=>y:i32=required;});
hot_map!(ContactChange,no_validation,{1=>contact:ContactPoint=required;2=>phase:ContactPhase=required;});

hot_map!(RecentAudioNotifications,validate_audio_notifications,{
    1=>default_endpoint_age_ms:Option<u32>=optional;2=>arrival_removal_age_ms:Option<u32>=optional;
});
hot_map!(VolumeKeyOrigin,no_validation,{
    1=>physical:Option<bool>=optional;2=>source_kind:SourceKind=required;
    3=>keep_local_playback:Option<bool>=optional;4=>windows_notifications:Option<RecentAudioNotifications>=optional;
});
fn check_audio_notifications(default_endpoint: Option<u32>, arrival_removal: Option<u32>) -> Result<()> {
    if [default_endpoint, arrival_removal]
        .into_iter()
        .flatten()
        .any(|age| age > 5000)
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
fn validate_audio_notifications(raw: &RawMap) -> Result<()> {
    check_audio_notifications(
        crate::v1::schema::defaulted(raw, 1, || None)?,
        crate::v1::schema::defaulted(raw, 2, || None)?,
    )
}
pub(crate) fn validate_audio_notifications_hot(view: &View<'_, RecentAudioNotifications>) -> Result<()> {
    check_audio_notifications(view.default_endpoint_age_ms()?, view.arrival_removal_age_ms()?)
}

wire_map!(TrailPoint,validate_trail_point,{1=>age_secs:f64=required;2=>pos:Point=required;});
wire_map!(TrailContinuation,validate_trail,{
    1=>shown_secs:f64=required;2=>size_scale:f64=required;3=>native_units_per_reference:f64=required;
    4=>emit_carry:f64=required;5=>heading:Point=required;6=>points:List<TrailPoint,64>=required;
});
fn bounded_trail_point(point: &Point) -> Result<()> {
    if point.x()?.abs() >= 1e7 || point.y()?.abs() >= 1e7 {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
fn validate_trail_point(raw: &RawMap) -> Result<()> {
    if !(0.0..=62.0).contains(&required::<f64>(raw, 1)?) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    bounded_trail_point(&required::<Point>(raw, 2)?)
}
fn validate_trail(raw: &RawMap) -> Result<()> {
    let size = required::<f64>(raw, 2)?;
    let units = required::<f64>(raw, 3)?;
    if required::<f64>(raw, 1)? < 0.0
        || size <= 0.0
        || size > 100.0
        || units <= 0.0
        || units > 10000.0 * 686.0
        || !(0.0..1.0).contains(&required::<f64>(raw, 4)?)
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    bounded_trail_point(&required::<Point>(raw, 5)?)?;
    let mut previous = f64::INFINITY;
    for point in required::<List<TrailPoint, 64>>(raw, 6)?.0 {
        let age = point.age_secs()?;
        if age > previous {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        previous = age;
    }
    Ok(())
}

fn check_contacts(
    device: u32,
    phase: ContactPhase,
    width: u32,
    height: u32,
    changes: impl Iterator<Item = Result<(u32, i32, i32, ContactPhase)>>,
) -> Result<()> {
    // Cancel carries no usable surface. Future phases remain opaque until the
    // input owner decides whether that action is supported.
    if matches!(phase, ContactPhase::Cancel | ContactPhase::Unknown(_)) {
        return Ok(());
    }
    if device == 0 || !(1000..=100000).contains(&width) || !(1000..=100000).contains(&height) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    let mut ids = [0; 10];
    let mut count = 0;
    let mut active_changes = 0;
    for change in changes {
        let (id, x, y, change_phase) = change?;
        if count == ids.len()
            || id == 0
            || x < 0
            || y < 0
            || x as u32 > width
            || y as u32 > height
            || ids[..count].contains(&id)
            || (phase == ContactPhase::Begin && !matches!(change_phase, ContactPhase::Begin | ContactPhase::Unknown(_)))
        {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        if matches!(change_phase, ContactPhase::Begin | ContactPhase::Move) {
            active_changes += 1;
        }
        ids[count] = id;
        count += 1;
    }
    if active_changes > 5 || (phase == ContactPhase::Begin && !(1..=5).contains(&count)) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
pub(super) fn validate_input_contacts(raw: &RawMap) -> Result<()> {
    let surface = required::<ContactSurface>(raw, 4)?;
    let changes = required::<List<ContactChange, 10>>(raw, 6)?.0;
    check_contacts(
        required(raw, 1)?,
        required(raw, 3)?,
        surface.width()?,
        surface.height()?,
        changes.into_iter().map(|change| {
            let point = change.contact()?;
            Ok((point.id()?, point.x()?, point.y()?, change.phase()?))
        }),
    )
}
pub(crate) fn validate_input_contacts_hot(view: &View<'_, InputTouchpadContacts>) -> Result<()> {
    let surface = view.surface()?;
    check_contacts(
        view.device()?,
        view.phase()?,
        surface.width()?,
        surface.height()?,
        view.changes()?.map(|change| {
            let change = change?;
            let point = change.contact()?;
            Ok((point.id()?, point.x()?, point.y()?, change.phase()?))
        }),
    )
}
