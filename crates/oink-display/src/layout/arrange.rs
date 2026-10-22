//! 初始自动排列：物理尺寸（含未知时的估算）、设备内按系统排列、设备之间并排；显示器变化后的同步。

use super::{Layout, ScreenPlacement, SizeSource, bbox};
use oink_foundation::display::DisplayInfo;
use oink_foundation::{DeviceId, Point, Rect, Size};

/// 系统报告的物理尺寸是否可信：宽高为正、宽高比与像素宽高比相差不超过 15%、
/// 像素密度在 40–600 ppi 之间（投影仪、电视常报告 0 或假的尺寸）。
pub fn plausible_reference_extent(info: &DisplayInfo) -> Option<Size> {
    let s = info.reference_extent?;
    if !(s.w.is_finite() && s.h.is_finite() && s.w > 0.0 && s.h > 0.0) {
        return None;
    }
    if info.pixel_width == 0 || info.pixel_height == 0 {
        return Some(s);
    }
    let px_aspect = f64::from(info.pixel_width) / f64::from(info.pixel_height);
    let reference_aspect = s.w / s.h;
    if (reference_aspect / px_aspect - 1.0).abs() > 0.15 {
        return None;
    }
    let ppi = f64::from(info.pixel_width) / (s.w * oink_foundation::display::REFERENCE_DIAGONAL_METERS / 0.0254);
    (40.0..=600.0).contains(&ppi).then_some(s)
}

/// 屏幕的物理尺寸（plane）：系统报告的值可信就用它，否则按分辨率估算。
///
/// 估算用常见屏幕的像素密度：内建屏按高分屏（约 220 ppi，低分辨率内建屏 140 ppi）；外接屏
/// 5K 约 218 ppi、4K 约 163 ppi（27 寸）、带鱼屏与 1440p 约 110 ppi、其余约 92 ppi（24 寸 1080p）。
pub fn estimate_reference_extent(info: &DisplayInfo) -> (Size, SizeSource) {
    if let Some(s) = plausible_reference_extent(info) {
        return (s, SizeSource::Reported);
    }
    let (pw, ph) = if info.pixel_width > 0 && info.pixel_height > 0 {
        (f64::from(info.pixel_width), f64::from(info.pixel_height))
    } else {
        // 没有像素信息：按本机坐标当作 96 dpi 的像素。
        (info.bounds.w.max(1.0), info.bounds.h.max(1.0))
    };
    let long = pw.max(ph);
    let ppi = if info.is_builtin {
        if long >= 2500.0 { 220.0 } else { 140.0 }
    } else if long >= 5000.0 {
        218.0
    } else if long >= 3800.0 {
        163.0
    } else if long >= 2500.0 {
        110.0
    } else {
        92.0
    };
    (
        Size::new(
            pw / ppi * 0.0254 / oink_foundation::display::REFERENCE_DIAGONAL_METERS,
            ph / ppi * 0.0254 / oink_foundation::display::REFERENCE_DIAGONAL_METERS,
        ),
        SizeSource::Estimated,
    )
}

/// Expand the native desktop using one uniform device scale. Native (0, 0)
/// anchors the group, including displays with negative origins and virtual screens.
pub(crate) fn arrange_device(device: DeviceId, displays: &[DisplayInfo]) -> Vec<ScreenPlacement> {
    let reference = displays.iter().find(|d| d.is_primary).or_else(|| displays.first());
    let scale = reference
        .map(|d| estimate_reference_extent(d).0.w / d.bounds.w)
        .unwrap_or(1.0);
    displays
        .iter()
        .map(|d| {
            let mut screen = ScreenPlacement::from_display(device, d, Point::default());
            screen.rect = Rect::new(
                d.bounds.x * scale,
                d.bounds.y * scale,
                d.bounds.w * scale,
                d.bounds.h * scale,
            );
            screen
        })
        .collect()
}

impl Layout {
    /// 初始自动排列：各设备内部按系统排列 + 物理尺寸拼接，设备之间按给定顺序从左到右并排、
    /// 垂直居中、紧贴（形成传送门）。结果是规整过的逻辑布局。
    pub fn auto(devices: &[(DeviceId, Vec<DisplayInfo>)]) -> Layout {
        let mut layout = Layout::default();
        for (device, displays) in devices {
            layout.sync_device(*device, displays);
        }
        layout
    }

    /// Refresh OS-owned geometry, preserving the native-origin placement and
    /// uniform scale. Live rearranges never normalize or move another computer.
    pub fn sync_device(&mut self, device: DeviceId, displays: &[DisplayInfo]) {
        let placement = self.device_transform(device);
        let mut fresh = arrange_device(device, displays);
        // Reconnects repeat the same native facts. A manual offline placement can
        // legitimately overlap a hidden sibling; only newly occupied geometry
        // may trigger repair, never an unchanged refresh or online transition.
        let old = self
            .screens
            .iter()
            .filter(|s| s.id.device == device)
            .collect::<Vec<_>>();
        let geometry_changed = old.len() != fresh.len()
            || fresh
                .iter()
                .any(|s| !old.iter().any(|o| o.id == s.id && o.local == s.local));
        let (origin, scale) = placement.unwrap_or_else(|| {
            let group = bbox(fresh.iter().map(|s| s.rect));
            let existing = bbox(self.screens.iter().filter(|s| s.id.device != device).map(|s| s.rect));
            let offset = match (existing, group) {
                (Some(e), Some(g)) => Point::new(e.right() - g.x, e.center().y - g.center().y),
                (_, Some(g)) => Point::new(-g.x, -g.y),
                _ => Point::default(),
            };
            let scale = fresh.first().map(|s| s.rect.w / s.local.w).unwrap_or(1.0);
            (offset, scale)
        });
        let manual = self
            .screens
            .iter()
            .any(|s| s.id.device == device && s.size_source == SizeSource::Manual);
        for screen in &mut fresh {
            screen.rect = Rect::new(
                origin.x + screen.local.x * scale,
                origin.y + screen.local.y * scale,
                screen.local.w * scale,
                screen.local.h * scale,
            );
            if manual {
                screen.size_source = SizeSource::Manual;
            }
        }
        self.screens.retain_mut(|s| {
            if s.id.device != device {
                return true;
            }
            if let Some(index) = fresh.iter().position(|f| f.id == s.id) {
                *s = fresh.remove(index);
                true
            } else {
                false
            }
        });
        self.screens.extend(fresh);
        if geometry_changed
            && let Some(id) = self
                .screens
                .iter()
                .find(|s| s.id.device == device)
                .map(|s| s.id.clone())
        {
            self.repair_overlaps_around(&id);
        }
    }

    /// 移除一台设备的所有屏幕（设备离开设备组时，不是暂时离线）。不保留在 `detached` 中，
    /// 只删除屏幕，保留空位。Removing screens cannot create a collision, and
    /// compaction only follows a manual edit, so no other computer moves.
    pub fn remove_device(&mut self, device: DeviceId) {
        self.screens.retain(|s| s.id.device != device);
    }
}
