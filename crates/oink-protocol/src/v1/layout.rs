//! Execution boundary for native screen geometry and rigid computer placement.
use super::{ErrorCode, RawMap, Result, dto, schema::WireDto};
use std::collections::BTreeSet;
fn known_fields(raw: &RawMap, fields: &[u32]) -> Result<()> {
    if raw.keys().any(|key| !fields.contains(key)) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
/// Retained extensions may be forwarded, but unknown geometry cannot execute.
pub fn require_supported_geometry(layout: &dto::LayoutValue) -> Result<()> {
    known_fields(layout.raw(), &[1, 2, 4])?;
    if !matches!(layout.source()?, dto::LayoutSource::Default | dto::LayoutSource::Manual) {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    let mut devices = BTreeSet::new();
    let mut total = 0;
    for group in layout.devices()?.0 {
        // Visibility is retained in register envelope key 7. DevicePlacement
        // key 5 is an in-process projection and must never cross this frozen
        // geometry boundary.
        known_fields(group.raw(), &[1, 2, 3, 4])?;
        group.device()?.require_supported()?;
        let device = group.device()?.ordering_key()?;
        if !devices.insert(device.clone()) {
            return Err(ErrorCode::StateConflict.into());
        }
        let origin = group.origin()?;
        known_fields(origin.raw(), &[1, 2])?;
        let (x, y, k) = (origin.x()?, origin.y()?, group.scale()?);
        if !x.is_finite() || !y.is_finite() || !k.is_finite() || k <= 0.0 {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        let mut screens = BTreeSet::new();
        for screen in group.screens()?.0 {
            total += 1;
            known_fields(screen.raw(), &[1, 2, 3, 5, 9, 10])?;
            let id = screen.id()?;
            id.device()?.require_supported()?;
            if id.device()?.ordering_key()? != device || !screens.insert(id.display()?.0) {
                return Err(ErrorCode::StateConflict.into());
            }
            if !matches!(
                screen.size_source()?,
                dto::SizeSource::Reported | dto::SizeSource::Estimated | dto::SizeSource::Manual
            ) {
                return Err(ErrorCode::UnsupportedValue.into());
            }
            let r = screen.local()?;
            known_fields(r.raw(), &[1, 2, 3, 4])?;
            if r.w()? <= 0.0
                || r.h()? <= 0.0
                || ![
                    r.x()?,
                    r.y()?,
                    r.w()?,
                    r.h()?,
                    r.x()? + r.w()?,
                    r.y()? + r.h()?,
                    x + r.x()? * k,
                    y + r.y()? * k,
                    x + (r.x()? + r.w()?) * k,
                    y + (r.y()? + r.h()?) * k,
                ]
                .into_iter()
                .all(f64::is_finite)
            {
                return Err(ErrorCode::UnsupportedValue.into());
            }
        }
    }
    if total > 256 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    known_fields(layout.topology()?.raw(), &[6, 7, 8, 9, 10, 12])?;
    Ok(())
}

/// Register envelope key 7: additive layout visibility metadata. It shares the
/// geometry register's LWW version/digest; old v1 editors retain the envelope.
/// Never put new fields into executable geometry: 0.3.3/101 reject those fields.
pub const VISIBILITY_KEY: u32 = 7;

fn metadata_devices(metadata: &RawMap, key: u32) -> Result<BTreeSet<(String, Vec<u8>)>> {
    Ok(metadata
        .get(&key)
        .map(|_| {
            super::schema::required::<super::schema::List<dto::DeviceId, 256>>(metadata, key)?
                .0
                .iter()
                .map(dto::DeviceId::ordering_key)
                .collect::<Result<BTreeSet<_>>>()
        })
        .transpose()?
        .unwrap_or_default())
}

/// A screen's ordering key: its device's ordering key plus its display id.
type ScreenKey = ((String, Vec<u8>), String);

fn metadata_screens(metadata: &RawMap, key: u32) -> Result<BTreeSet<ScreenKey>> {
    Ok(metadata
        .get(&key)
        .map(|_| {
            super::schema::required::<super::schema::List<dto::ScreenId, 256>>(metadata, key)?
                .0
                .iter()
                .map(|screen| Ok((screen.device()?.ordering_key()?, screen.display()?.0.clone())))
                .collect::<Result<BTreeSet<_>>>()
        })
        .transpose()?
        .unwrap_or_default())
}

/// Expand register metadata only for the new runtime's local layout projection.
pub fn register_value(register: &dto::Register) -> Result<dto::LayoutValue> {
    use super::schema::WireValue;
    let mut raw = register.value()?;
    let metadata = register.layout_visibility()?.unwrap_or_default();
    // Key 1 is the original 102 envelope shape. Keys 3 and 4 make explicit
    // visible records representable, so a false value cannot fall back to an
    // old owner preference. Old key-1-only envelopes remain readable.
    let hidden_devices = metadata_devices(&metadata, 1)?;
    let known_devices = if metadata.contains_key(&3) {
        metadata_devices(&metadata, 3)?
    } else {
        hidden_devices.clone()
    };
    let hidden_screens = metadata_screens(&metadata, 2)?;
    let known_screens = if metadata.contains_key(&4) {
        metadata_screens(&metadata, 4)?
    } else {
        hidden_screens.clone()
    };
    let mut groups = dto::LayoutValue::from_raw(raw.clone())?.devices()?.0;
    for group in &mut groups {
        let mut fields = group.raw().clone();
        let device = group.device()?.ordering_key()?;
        let device_known = known_devices.contains(&device) || fields.contains_key(&5);
        if device_known {
            fields.insert(5, hidden_devices.contains(&device).write());
        }
        let screens = group
            .screens()?
            .0
            .into_iter()
            .map(|screen| {
                let mut fields = screen.raw().clone();
                let id = screen.id()?;
                let key = (id.device()?.ordering_key()?, id.display()?.0.clone());
                if known_screens.contains(&key) || fields.contains_key(&11) {
                    fields.insert(11, hidden_screens.contains(&key).write());
                }
                dto::ScreenPlacement::from_raw(fields)
            })
            .collect::<Result<Vec<_>>>()?;
        fields.insert(
            4,
            super::RawValue::Array(screens.iter().map(WireValue::write).collect()),
        );
        *group = dto::DevicePlacement::from_raw(fields)?;
    }
    raw.insert(1, super::RawValue::Array(groups.iter().map(WireValue::write).collect()));
    dto::LayoutValue::from_raw(raw)
}

/// Edit the projected layout, then store baseline executable geometry and its
/// visibility metadata atomically in one retaining register. Unknown envelope,
/// metadata, identity and geometry fields survive. The original digest is always
/// verified before editing; normalization advances the version like any edit.
pub fn edit_register(
    current: &dto::Register,
    expected: &dto::ConfigVersion,
    writer: dto::DeviceId,
    patch: impl FnOnce(&mut RawMap) -> Result<()>,
) -> Result<dto::Register> {
    use super::schema::WireValue;
    super::retained::verify_register(current)?;
    if &current.version()? != expected {
        return Err(ErrorCode::StateConflict.into());
    }
    expected.counter()?.checked_add(1).ok_or(ErrorCode::LimitExceeded)?;
    let mut projected = register_value(current)?.into_raw();
    patch(&mut projected)?;
    let layout = dto::LayoutValue::from_raw(projected.clone())?;
    let mut hidden_devices = Vec::new();
    let mut known_devices = Vec::new();
    let mut hidden_screens = Vec::new();
    let mut known_screens = Vec::new();
    let mut groups = Vec::new();
    for group in layout.devices()?.0 {
        if group.raw().contains_key(&5) {
            let device = group.device()?;
            known_devices.push(device.clone());
            if group.hidden()? {
                hidden_devices.push(device);
            }
        }
        let mut fields = group.into_raw();
        fields.remove(&5);
        let screens = dto::DevicePlacement::from_raw(fields.clone())?.screens()?.0;
        let mut clean_screens = Vec::new();
        for screen in screens {
            if screen.raw().contains_key(&11) {
                let id = screen.id()?;
                known_screens.push(id.clone());
                if screen.hidden()? {
                    hidden_screens.push(id);
                }
            }
            let mut fields = screen.into_raw();
            fields.remove(&11);
            clean_screens.push(super::RawValue::Map(fields));
        }
        fields.insert(4, super::RawValue::Array(clean_screens));
        groups.push(super::RawValue::Map(fields));
    }
    projected.insert(1, super::RawValue::Array(groups));
    let edited = super::retained::edit_register(current, expected, writer, |value| {
        *value = projected;
        Ok(())
    })?;
    let mut raw = edited.into_raw();
    if !known_devices.is_empty() || !known_screens.is_empty() || current.layout_visibility()?.is_some() {
        hidden_devices.sort_by_key(|id| id.ordering_key().expect("validated device identity"));
        known_devices.sort_by_key(|id| id.ordering_key().expect("validated device identity"));
        hidden_screens.sort_by_key(|id| {
            (
                id.device()
                    .and_then(|d| d.ordering_key())
                    .expect("validated device identity"),
                id.display().expect("validated display").0.clone(),
            )
        });
        known_screens.sort_by_key(|id| {
            (
                id.device()
                    .and_then(|d| d.ordering_key())
                    .expect("validated device identity"),
                id.display().expect("validated display").0.clone(),
            )
        });
        let mut metadata = current.layout_visibility()?.unwrap_or_default();
        metadata.insert(
            1,
            super::RawValue::Array(hidden_devices.iter().map(WireValue::write).collect()),
        );
        metadata.insert(
            2,
            super::RawValue::Array(hidden_screens.iter().map(WireValue::write).collect()),
        );
        metadata.insert(
            3,
            super::RawValue::Array(known_devices.iter().map(WireValue::write).collect()),
        );
        metadata.insert(
            4,
            super::RawValue::Array(known_screens.iter().map(WireValue::write).collect()),
        );
        raw.insert(VISIBILITY_KEY, super::RawValue::Map(metadata));
    }
    raw.insert(6, super::retained::register_digest(&raw)?.write());
    dto::Register::from_raw(raw)
}
