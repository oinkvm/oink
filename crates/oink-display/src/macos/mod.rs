//! macOS 实现：`CGGetOnlineDisplayList`、`CGDisplayBounds`、`CGDisplayScreenSize`、
//! `CGDisplayRegisterReconfigurationCallback`。
//!
//! - `bounds`：Quartz 全局坐标（point，主屏左上角为原点，y 向下）。
//! - 像素：当前显示模式的 backing store 像素（`CGDisplayModeGetPixelWidth`），Retina 通常是 point 的 2 倍；
//!   「更多空间」等缩放模式下是缩放后的帧缓冲像素，不是面板的原生像素。
//! - 物理尺寸：`CGDisplayScreenSize`（来自 EDID）。它不随旋转交换宽高，这里按 `bounds` 的方向校正；
//!   EDID 无效时系统按 72 dpi 伪造一个值，这种情况识别为「读不到」。
//! - 名字与虚拟显示器：CoreDisplay 的私有函数 `CoreDisplay_DisplayCreateInfoDictionary`（运行时 `dlsym`，
//!   不存在时降级），其中有 `DisplayProductName`、`kCGDisplayIsVirtualDevice`、`kCGDisplayIsAirPlay`。
//! - 镜像：镜像组里只保留主显示器（被镜像的那块），其余跳过。

mod info;
mod wallpaper;
mod watch;

use std::sync::Mutex;

use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin, CGDisplayIsMain,
    CGDisplayMirrorsDisplay, CGDisplayMode, CGDisplayModelNumber, CGDisplayRotation, CGDisplayScreenSize,
    CGDisplaySerialNumber, CGDisplayVendorNumber, CGError, CGGetOnlineDisplayList,
};
use oink_foundation::display::DisplayInfo;
use oink_foundation::geom::{Rect, Size};

use crate::{ChangeHandler, DisplayBackend, DisplayError};

pub(crate) fn backend() -> Result<Box<dyn DisplayBackend>, DisplayError> {
    Ok(Box::new(MacDisplays {
        watcher: Mutex::new(None),
    }))
}

struct MacDisplays {
    watcher: Mutex<Option<watch::Watcher>>,
}

impl DisplayBackend for MacDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        enumerate()
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), DisplayError> {
        let mut w = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        match w.as_ref() {
            Some(existing) => existing.add_handler(on_change),
            None => *w = Some(watch::Watcher::start(on_change)?),
        }
        Ok(())
    }

    fn wallpapers(&self) -> Option<Vec<crate::wallpaper::DisplayWallpaper>> {
        wallpaper::read_all()
    }

    fn watch_wallpaper(&self, on_change: ChangeHandler) -> Result<(), DisplayError> {
        wallpaper::watch_spaces(on_change);
        Ok(())
    }
}

/// 在线显示器的 ID（含休眠中的显示器与镜像组的从属显示器）。
///
/// 用在线列表而不是活动列表：显示器休眠（锁屏后熄屏）时活动列表为空，本机会被当成「没有屏幕」，
/// 布局随之抖动；在线列表里休眠的显示器仍在，bounds 与显示模式照常可读。
pub(crate) fn online_display_ids() -> Result<Vec<CGDirectDisplayID>, DisplayError> {
    let mut count: u32 = 0;
    // SAFETY: 传空指针时只返回数量。
    let err = unsafe { CGGetOnlineDisplayList(0, std::ptr::null_mut(), &mut count) };
    check(err, "CGGetOnlineDisplayList")?;
    let mut ids = vec![0 as CGDirectDisplayID; count as usize + 4];
    let mut n: u32 = 0;
    // SAFETY: 缓冲区长度与传入的 max 一致。
    let err = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) };
    check(err, "CGGetOnlineDisplayList")?;
    ids.truncate(n as usize);
    Ok(ids)
}

fn check(err: CGError, what: &str) -> Result<(), DisplayError> {
    if err == CGError::Success {
        Ok(())
    } else {
        Err(DisplayError::Platform(format!("{what} 返回 {}", err.0)))
    }
}

fn enumerate() -> Result<Vec<DisplayInfo>, DisplayError> {
    let ids = online_display_ids()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        // 镜像组：从属显示器（镜像了别人）跳过，镜像组视为一块屏幕。
        if CGDisplayMirrorsDisplay(id) != 0 {
            continue;
        }
        out.push(describe(id));
    }
    // 主屏排在最前，其余按 x、y 排序，输出稳定。
    out.sort_by(|a, b| {
        b.is_primary
            .cmp(&a.is_primary)
            .then(a.bounds.x.total_cmp(&b.bounds.x))
            .then(a.bounds.y.total_cmp(&b.bounds.y))
    });
    Ok(out)
}

fn describe(id: CGDirectDisplayID) -> DisplayInfo {
    let b = CGDisplayBounds(id);
    let bounds = Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height);
    let rotation_deg = normalize_rotation(CGDisplayRotation(id));

    let (mut pw, mut ph) = match CGDisplayCopyDisplayMode(id) {
        Some(mode) => (
            CGDisplayMode::pixel_width(Some(&mode)) as u32,
            CGDisplayMode::pixel_height(Some(&mode)) as u32,
        ),
        None => ((bounds.w) as u32, (bounds.h) as u32),
    };
    if needs_swap(f64::from(pw), f64::from(ph), &bounds) {
        std::mem::swap(&mut pw, &mut ph);
    }

    let s = CGDisplayScreenSize(id);
    let reference_extent = reference_extent(s.width * 0.001, s.height * 0.001, &bounds, pw, ph);

    let extra = info::display_info(id);
    let is_builtin = CGDisplayIsBuiltin(id);
    let vendor = CGDisplayVendorNumber(id);
    let model = CGDisplayModelNumber(id);
    let serial = CGDisplaySerialNumber(id);

    let key = info::display_uuid(id)
        .or_else(|| extra.uuid.clone())
        .unwrap_or_else(|| format!("cg:{vendor:04x}-{model:04x}-{serial:08x}"));
    let name = extra
        .product_name
        .clone()
        .map(|n| {
            if is_builtin && n == "Color LCD" {
                "Built-in Display".to_owned()
            } else {
                n
            }
        })
        .unwrap_or_else(|| {
            if is_builtin {
                "Built-in Display".to_owned()
            } else {
                format!("Display {id}")
            }
        });
    let is_virtual = extra.is_virtual.unwrap_or(false)
        || (!is_builtin && vendor == 0 && model == 0 && reference_extent.is_none())
        || looks_virtual(&name);

    DisplayInfo {
        key,
        name,
        bounds,
        pixel_width: pw,
        pixel_height: ph,
        reference_extent,
        rotation_deg,
        is_primary: CGDisplayIsMain(id),
        is_builtin,
        is_virtual,
    }
}

/// `CGDisplayRotation` 返回顺时针角度（double）；取最近的 90° 倍数。
fn normalize_rotation(deg: f64) -> u16 {
    let r = (deg / 90.0).round() as i64;
    (r.rem_euclid(4) * 90) as u16
}

/// 宽高方向与 `bounds` 不一致时需要交换（旋转 90/270° 的屏幕）。
fn needs_swap(w: f64, h: f64, bounds: &Rect) -> bool {
    w > 0.0 && h > 0.0 && bounds.w != bounds.h && w != h && ((w > h) != (bounds.w > bounds.h))
}

/// Convert measured metres into reference geometry, oriented like native bounds.
/// Reject missing measurements and the system's fabricated 72-dpi EDID fallback.
fn reference_extent(w: f64, h: f64, bounds: &Rect, pixel_w: u32, pixel_h: u32) -> Option<Size> {
    if !(w.is_finite() && h.is_finite()) || w <= 0.001 || h <= 0.001 {
        return None;
    }
    let (w, h) = if needs_swap(w, h, bounds) { (h, w) } else { (w, h) };
    const FAKE_PIXELS_PER_METER: f64 = 2835.0;
    let is_fake = |len: f64, units: f64| units > 0.0 && (units / FAKE_PIXELS_PER_METER - len).abs() < 0.001;
    if (is_fake(w, bounds.w) && is_fake(h, bounds.h))
        || (is_fake(w, f64::from(pixel_w)) && is_fake(h, f64::from(pixel_h)))
    {
        return None;
    }
    Some(Size::new(
        w / oink_foundation::display::REFERENCE_DIAGONAL_METERS,
        h / oink_foundation::display::REFERENCE_DIAGONAL_METERS,
    ))
}

/// 名字里带有明显虚拟屏特征（远程桌面、无头显示器软件）时判为虚拟显示器。
fn looks_virtual(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "virtual",
        "dummy",
        "headless",
        "vnc",
        "remote display",
        "parsec",
        "deskpad",
    ]
    .iter()
    .any(|k| n.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_is_normalized() {
        assert_eq!(normalize_rotation(0.0), 0);
        assert_eq!(normalize_rotation(90.0), 90);
        assert_eq!(normalize_rotation(270.0), 270);
        assert_eq!(normalize_rotation(-90.0), 270);
        assert_eq!(normalize_rotation(360.0), 0);
    }

    #[test]
    fn reference_extent_follows_rotation() {
        let portrait = Rect::new(0.0, 0.0, 1080.0, 1920.0);
        let s = reference_extent(0.600, 0.340, &portrait, 1080, 1920).unwrap();
        assert_eq!((s.w, s.h), (0.340 / 0.686, 0.600 / 0.686));
        let landscape = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let s = reference_extent(0.600, 0.340, &landscape, 1920, 1080).unwrap();
        assert_eq!((s.w, s.h), (0.600 / 0.686, 0.340 / 0.686));
    }

    #[test]
    fn fake_edid_size_is_rejected() {
        let b = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        assert!(reference_extent(1920.0 / 2835.0, 1080.0 / 2835.0, &b, 1920, 1080).is_none());
        assert!(reference_extent(0.0, 0.0, &b, 1920, 1080).is_none());
    }

    #[test]
    fn virtual_names() {
        assert!(looks_virtual("BetterDisplay Dummy 16:9"));
        assert!(!looks_virtual("DELL U2720Q"));
    }

    #[test]
    fn enumerates_real_displays() {
        // 在 macOS 上总能拿到至少一块显示器（CI 的无头 mac 也有虚拟帧缓冲）。
        let list = enumerate().expect("enumerate");
        assert!(!list.is_empty());
        assert_eq!(list.iter().filter(|d| d.is_primary).count(), 1);
        for d in &list {
            assert!(d.bounds.w > 0.0 && d.bounds.h > 0.0);
            assert!(!d.key.is_empty());
        }
    }
}
