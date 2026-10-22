//! Original source cleanup remains position-safe after native authority is lost.
use oink_foundation::{Point, content::ClipId};

pub(super) fn at_origin(
    id: ClipId,
    recorded: Option<(ClipId, Point)>,
    release: impl FnOnce(Point) -> Result<(), String>,
) -> Result<(), String> {
    let at = recorded
        .filter(|(owner, at)| *owner == id && at.x.is_finite() && at.y.is_finite())
        .map(|(_, at)| at)
        .ok_or_else(|| "original source press position unavailable".to_string())?;
    release(at)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn lost_native_authority_cannot_release_at_the_live_cursor() {
        let id = ClipId {
            origin: oink_foundation::DeviceId([1; 32]),
            seq: 7,
        };
        let start = Point::new(-120., 90.);
        let mut injections = Vec::new();
        // No operation/catcher is needed, and the live cursor is never sampled.
        at_origin(id, Some((id, start)), |at| {
            injections.push(at);
            Ok(())
        })
        .unwrap();
        for recorded in [
            None,
            Some((ClipId { seq: 8, ..id }, start)),
            Some((id, Point::new(f64::NAN, 0.))),
        ] {
            assert!(
                at_origin(id, recorded, |at| {
                    injections.push(at);
                    Ok(())
                })
                .is_err()
            );
        }
        assert_eq!(injections, vec![start]);
    }
}
