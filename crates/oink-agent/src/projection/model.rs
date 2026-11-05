//! Ephemeral layout editing only. Final geometry uses the normal C0 LayoutItem.
use oink_foundation::{DeviceId, Rect};

#[derive(Clone, Debug, PartialEq)]
pub struct ScreenRect {
    pub device: DeviceId,
    pub display: String,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Arrangement {
        sequence: u64,
        lease_ms: u32,
        expires_ms: u64,
        snapshot: Option<Box<crate::layout_profiles::types::ArrangementSnapshot>>,
    },
    /// A full, latest-only preview. None clears this author's preview immediately.
    Preview {
        sequence: u64,
        screens: Option<Vec<ScreenRect>>,
        affected: Vec<(DeviceId, String)>,
    },
}

impl Action {
    pub fn valid(&self) -> bool {
        match self {
            Self::Arrangement { lease_ms, snapshot, .. } => {
                (1..=2000).contains(lease_ms)
                    && snapshot
                        .as_ref()
                        .is_none_or(|s| crate::layout_profiles::render::valid_preview(s))
            }
            Self::Preview { screens, affected, .. } => {
                affected.len() <= 64
                    && affected.iter().map(|(_, key)| key.len()).sum::<usize>()
                        + screens
                            .as_ref()
                            .map_or(0, |v| v.iter().map(|s| s.display.len()).sum::<usize>())
                        <= 8000
                    && affected.iter().all(|(_, key)| key.len() <= 512)
                    && screens.as_ref().is_none_or(|screens| {
                        screens.len() <= 64
                            && screens.iter().all(|s| {
                                let r = s.rect;
                                s.display.len() <= 512
                                    && [r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite())
                                    && r.x.abs() <= 1_000_000.0 / 686.0
                                    && r.y.abs() <= 1_000_000.0 / 686.0
                                    && r.w > 0.0
                                    && r.h > 0.0
                                    && r.w <= 4000.0 / 686.0
                                    && r.h <= 4000.0 / 686.0
                            })
                    })
            }
        }
    }
}
