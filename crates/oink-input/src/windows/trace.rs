//! Numeric lifecycle observations at native state transitions.
use crate::native_lifecycle::{self, Data, Kind};
use windows::Win32::Foundation::{POINT, RECT};

/// Infrequent lifecycle observation. State may be borrowed by its caller.
pub(super) fn action(kind: Kind, point: Option<POINT>, reason: u32, success: Option<bool>) {
    if native_lifecycle::lifecycle_enabled() {
        native_lifecycle::emit(
            kind,
            Data {
                point: point.map(|p| [p.x, p.y]),
                reason,
                success,
                ..Data::default()
            },
        );
    }
}
pub(super) fn clip_action(rect: RECT, reason: u32, success: bool) {
    if native_lifecycle::lifecycle_enabled() {
        native_lifecycle::emit(
            Kind::ClipApply,
            Data {
                clip: Some([rect.left, rect.top, rect.right, rect.bottom]),
                reason,
                success: Some(success),
                ..Data::default()
            },
        );
    }
}
