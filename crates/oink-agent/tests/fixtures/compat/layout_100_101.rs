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
