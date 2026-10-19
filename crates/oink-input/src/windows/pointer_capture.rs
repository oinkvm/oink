//! Read-only 50 ms sampling. No window/GDI queries run in input callbacks.
//! Native compilation and bounded desktop injection validated on Windows 11 (2026-10-09).
//! Physical HID and installed cross-device routing remain separate acceptance checks.
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use oink_foundation::Point;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::pointer_capture::CaptureSample;
use crate::pointer_capture::windows::{Bounds, MotionBatch, WindowsSample, cursor_hidden, unchanged_render};

static MOTION: OnceLock<(Instant, Mutex<MotionBatch>)> = OnceLock::new();

fn motion_state() -> &'static (Instant, Mutex<MotionBatch>) {
    MOTION.get_or_init(|| (Instant::now(), Mutex::new(MotionBatch::default())))
}

pub(super) fn record_motion(nonzero: bool) {
    if nonzero {
        let (origin, batch) = motion_state();
        let mut batch = batch.lock().unwrap_or_else(|e| e.into_inner());
        batch.record(origin.elapsed().as_micros() as u64);
    }
}

fn bounds(r: RECT) -> Bounds {
    Bounds {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

pub(super) fn sample() -> Option<CaptureSample> {
    let _dpi = super::dpi::ThreadDpiGuard::per_monitor_v2();
    let mut info = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: read-only queries into initialized stack buffers; PMv2 screen coordinates.
    unsafe {
        GetCursorInfo(&mut info).ok()?;
        let foreground = GetForegroundWindow();
        if foreground.is_invalid() {
            return None;
        }
        let showing = info.flags.0 & CURSOR_SHOWING.0 != 0;
        let suppressed = info.flags.0 & CURSOR_SUPPRESSED.0 != 0;
        let transparent = if showing && !suppressed {
            transparent_cursor(info.hCursor)
        } else {
            None
        };
        let application_hidden = cursor_hidden(showing, suppressed, transparent);
        // Preserve actual invisibility for the pointer trail; suppression is
        // excluded from capture via application_hidden, not by showing a trail.
        let hidden = !showing || suppressed || transparent == Some(true);
        let mut clip = RECT::default();
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let desktop = Bounds {
            left: vx,
            top: vy,
            right: vx.saturating_add(GetSystemMetrics(SM_CXVIRTUALSCREEN)),
            bottom: vy.saturating_add(GetSystemMetrics(SM_CYVIRTUALSCREEN)),
        };
        let confined = GetClipCursor(&mut clip).is_ok() && bounds(clip).confined(desktop);
        let mut window = RECT::default();
        let mut monitor = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let fullscreen = IsWindowVisible(foreground).as_bool()
            && !IsIconic(foreground).as_bool()
            && GetWindowRect(foreground, &mut window).is_ok()
            && GetMonitorInfoW(MonitorFromWindow(foreground, MONITOR_DEFAULTTONULL), &mut monitor).as_bool()
            && bounds(window).covers(bounds(monitor.rcMonitor));
        // Avoid combining two different cursor/foreground states across GDI work.
        let mut after = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        GetCursorInfo(&mut after).ok()?;
        if foreground != GetForegroundWindow() || info.flags != after.flags || info.hCursor != after.hCursor {
            return None;
        }
        let (origin, motion) = motion_state();
        let mut motion = motion.lock().unwrap_or_else(|e| e.into_inner());
        let sampled_at = origin.elapsed().as_micros() as u64;
        Some(CaptureSample {
            windows: Some(WindowsSample {
                sampled_at,
                fullscreen,
                motion: std::mem::take(&mut *motion),
            }),
            hidden,
            application_hidden: Some(application_hidden),
            confined,
            position: Point::new(f64::from(after.ptScreenPos.x), f64::from(after.ptScreenPos.y)),
            motion_serial: 0,
            motion_observable: false,
            foreground: foreground.0 as usize as u64,
        })
    }
}

/// Draw the full native image on two opposite backgrounds: alpha-only inspection
/// misclassifies legacy AND/XOR cursors; a single white canvas misses a white cursor.
/// Re-read every sample (no handle cache: a handle's image can be replaced in place).
/// Animated cursors use frame zero; other animation frames need native validation.
fn transparent_cursor(cursor: HCURSOR) -> Option<bool> {
    if cursor.is_invalid() {
        return None;
    }
    // SAFETY: GetIconInfo allocates caller-owned bitmaps. Each successful allocation
    // is freed below, on success and failure; the system cursor itself is borrowed.
    unsafe {
        let mut icon = ICONINFO::default();
        GetIconInfo(HICON(cursor.0), &mut icon).ok()?;
        let result = (|| {
            let mut bitmap = BITMAP::default();
            let source = if icon.hbmColor.is_invalid() {
                icon.hbmMask
            } else {
                icon.hbmColor
            };
            if GetObjectW(
                HGDIOBJ(source.0),
                size_of::<BITMAP>() as i32,
                Some((&mut bitmap as *mut BITMAP).cast()),
            ) == 0
            {
                return None;
            }
            let width = bitmap.bmWidth;
            let height = if icon.hbmColor.is_invalid() {
                bitmap.bmHeight / 2
            } else {
                bitmap.bmHeight
            };
            // Bound allocations and 20 Hz scans. Unsupported images are not proof of hiding.
            if !(1..=512).contains(&width) || !(1..=512).contains(&height) {
                return None;
            }
            render_transparent(cursor, width, height)
        })();
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmColor.0));
        }
        if !icon.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
        }
        result
    }
}

fn render_transparent(cursor: HCURSOR, width: i32, height: i32) -> Option<bool> {
    // SAFETY: private memory DC and top-down 32-bit DIB; dimensions are checked by
    // the caller. Flush GDI before CPU reads; restore selection before freeing.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let result = (|| {
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let dib = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, HGDIOBJ(dib.0));
            let result = (|| {
                if bits.is_null() || old.is_invalid() {
                    return None;
                }
                let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (width * height) as usize);
                for background in [0, 0x00ff_ffff] {
                    pixels.fill(background);
                    DrawIconEx(dc, 0, 0, HICON(cursor.0), width, height, 0, None, DI_NORMAL).ok()?;
                    if !GdiFlush().as_bool() {
                        return None;
                    }
                    if !unchanged_render(pixels, background) {
                        return Some(false);
                    }
                }
                Some(true)
            })();
            if !old.is_invalid() {
                SelectObject(dc, old);
            }
            let _ = DeleteObject(HGDIOBJ(dib.0));
            result
        })();
        let _ = DeleteDC(dc);
        result
    }
}
