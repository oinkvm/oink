//! Optional source drag artwork. Pixels never become clipboard content.
use serde::{Deserialize, Serialize};

/// Fixed destination observations, never a claim that submitted nil providers
/// have disappeared from the display. No content or window identity is stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeHoldEnd {
    Resume,
    DraggingExited,
    DraggingEnded,
    DropAccepted,
    DropRejected,
    CancelRepositioned,
    HoldRejected,
    #[default]
    HoldUnconfirmed,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DestinationSnapshot {
    pub session_location: Option<crate::Point>,
    pub destination_ours: bool,
    pub catcher: crate::Rect,
    pub mouse_location: crate::Point,
    pub mouse_hit_ours: bool,
    pub visible: bool,
    pub level: i64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NativeHoldSummary {
    pub operation: u64,
    pub native_sequence: i64,
    pub elapsed_us: u64,
    pub end: NativeHoldEnd,
    pub entered: u64,
    pub updated: u64,
    pub image_updated: u64,
    pub exited: u64,
    pub ended: u64,
    pub first_exit_us: Option<u64>,
    pub last_source_mask: u64,
    pub last_returned_mask: u64,
    pub last_is_own: bool,
    pub returned_none: u64,
    pub first_none_us: Option<u64>,
    pub initial_nil_items: u64,
    pub update_nil_items: u64,
    pub providers_on_update: u64,
    pub show_calls: u64,
    pub move_calls: u64,
    pub start: DestinationSnapshot,
    pub first_image: Option<DestinationSnapshot>,
    pub finish: DestinationSnapshot,
    /// Appended fixed numeric evidence for this hold's cancel, never image pixels.
    pub cancel: Option<CancelObservation>,
    /// Appended per-hold image-refresh evidence. A callback is not a visual ACK.
    pub image_nudges_posted: u64,
    pub image_nudges: Vec<ImageNudgeObservation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageNudgeReason {
    InitialHide,
    ImageUpdate,
}

/// Public CGEvent submission, independently of a subsequent tap acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageNudgePost {
    pub at: crate::Point,
    pub button: crate::input::MouseButton,
    pub flags: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageNudgeObservation {
    pub generation: u64,
    pub reason: ImageNudgeReason,
    pub posted_us: u64,
    pub post: ImageNudgePost,
    /// First image-update callback after this post, relative to hold start.
    /// None at hold end means no callback was observed, not a hide failure.
    pub image_update_us: Option<u64>,
}
/// Cancel geometry is in source native coordinates, except `target_plane`
/// which is the cancelling holder's actual pointer in the shared plane plane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CancelGeometry {
    pub down: Option<crate::Point>,
    pub target_plane: Option<crate::Point>,
    pub projected: Option<crate::Point>,
    pub edge: Option<crate::Point>,
    /// Used only when no valid geometric intersection was available.
    pub fallback: Option<crate::Point>,
}
impl CancelGeometry {
    pub fn requested(&self) -> Option<crate::Point> {
        self.edge.or(self.fallback)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CancelObservation {
    pub geometry: CancelGeometry,
    /// Exact native coordinate submitted by the input backend (after rounding).
    /// None means no submission was observed, not a successful zero move.
    pub posted: Option<crate::Point>,
    pub delivery_ack: Option<bool>,
    pub callback_count: u64,
    pub last_callback: Option<crate::Point>,
    pub ack_position: Option<crate::Point>,
    /// From prepare start to the first matching AppKit callback, not tap receipt.
    pub ack_latency_us: Option<u64>,
    pub wait_us: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFailure {
    Cancelled,
    Timeout,
    Failed,
}

/// Local-only native catcher ownership. This is never serialized or diagnosed
/// with a window handle. The input owner uses its existing capture/warp ledger.
#[derive(Clone, Copy, PartialEq)]
pub struct DragAnchor {
    pub generation: u64,
    pub at: crate::Point,
    /// Windows catcher HWND. Zero on platforms without native window handles.
    pub surface: usize,
}
impl std::fmt::Debug for DragAnchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeDragAnchor")
    }
}

/// Small PNG plus logical size and a top-left-relative cursor hotspot.
/// PNG dimensions are independently bounded before any platform decoder sees it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DragImage {
    pub png: std::sync::Arc<Vec<u8>>,
    pub width: f64,
    pub height: f64,
    pub hotspot_x: f64,
    pub hotspot_y: f64,
}
impl std::fmt::Debug for DragImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DragImage")
            .field("bytes", &self.png.len())
            .field("size", &(self.width, self.height))
            .field("hotspot", &(self.hotspot_x, self.hotspot_y))
            .finish()
    }
}
impl DragImage {
    /// Shared PNG wire/capture/receiver/target budget on both platforms.
    pub const MAX_BYTES: usize = 512 * 1024;
    pub const MAX_PIXELS: u32 = 512;
    pub fn valid(&self) -> bool {
        let p = &self.png;
        if p.len() < 33
            || p.len() > Self::MAX_BYTES
            || &p[..8] != b"\x89PNG\r\n\x1a\n"
            || p[8..12] != 13_u32.to_be_bytes()
            || &p[12..16] != b"IHDR"
        {
            return false;
        }
        let w = u32::from_be_bytes(p[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(p[20..24].try_into().unwrap());
        (1..=Self::MAX_PIXELS).contains(&w)
            && (1..=Self::MAX_PIXELS).contains(&h)
            && [self.width, self.height]
                .iter()
                .all(|v| v.is_finite() && *v > 0. && *v <= 512.)
            && self.hotspot_x.is_finite()
            && (0. ..=self.width).contains(&self.hotspot_x)
            && self.hotspot_y.is_finite()
            && (0. ..=self.height).contains(&self.hotspot_y)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_png_budget_accepts_512_kib_and_rejects_one_byte_more() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&512_u32.to_be_bytes());
        png.extend_from_slice(&512_u32.to_be_bytes());
        let mut image = DragImage {
            png: png.into(),
            width: 512.,
            height: 512.,
            hotspot_x: 512.,
            hotspot_y: 256.,
        };
        // valid() checks the bounded PNG envelope; platform decoders check pixels.
        std::sync::Arc::make_mut(&mut image.png).resize(64 * 1024 + 1, 0);
        assert!(image.valid());
        std::sync::Arc::make_mut(&mut image.png).resize(512 * 1024, 0);
        assert_eq!(image.png.len(), DragImage::MAX_BYTES);
        assert!(image.valid());
        std::sync::Arc::make_mut(&mut image.png).push(0);
        assert!(!image.valid());
    }
    #[test]
    fn rejects_oversized_pixels_and_nonfinite_geometry() {
        let mut p = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        p.extend_from_slice(&1_u32.to_be_bytes());
        p.extend_from_slice(&2_u32.to_be_bytes());
        p.resize(33, 0);
        let mut image = DragImage {
            png: p.into(),
            width: 1.,
            height: 2.,
            hotspot_x: 0.,
            hotspot_y: 1.,
        };
        assert!(image.valid());
        let copy = image.clone();
        assert!(
            std::sync::Arc::ptr_eq(&image.png, &copy.png),
            "input/capture snapshots share immutable pixels"
        );
        image.width = f64::NAN;
        assert!(!image.valid());
        image.width = 1.;
        std::sync::Arc::make_mut(&mut image.png)[16..20].copy_from_slice(&513_u32.to_be_bytes());
        assert!(!image.valid());
        std::sync::Arc::make_mut(&mut image.png).truncate(20);
        assert!(!image.valid());
    }
}
