//! Window-free comparison of the toast path with direct Windows GDI text output.
#[cfg(windows)]
mod native {
    use oink_locator::aim::{
        raster::{Canvas, Color},
        text::{self, Mask, Pen, TextRaster},
    };
    use std::{io::Write, path::Path};
    use windows::{
        Win32::{
            Foundation::{COLORREF, SIZE},
            Graphics::Gdi::*,
            UI::WindowsAndMessaging::*,
        },
        core::{PCWSTR, w},
    };

    #[derive(Debug)]
    struct MaskOnly(Box<dyn TextRaster>);
    impl TextRaster for MaskOnly {
        fn line(&self, text: &str, cap: f64, bold: bool) -> Mask {
            self.0.line(text, cap, bold)
        }
    }
    fn save(path: &Path, c: &Canvas) -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        write!(file, "P6\n{} {}\n255\n", c.w, c.h)?;
        for p in &c.px {
            file.write_all(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8])?;
        }
        Ok(())
    }
    fn line(
        c: &mut Canvas,
        text: &str,
        cap: f64,
        bold: bool,
        origin: (f64, f64),
        fg: Color,
        settings: (FONT_QUALITY, PCWSTR),
    ) {
        let (quality, face) = settings;
        // SAFETY: memory DC/DIB only. All objects remain selected only until deletion.
        unsafe {
            let dc = CreateCompatibleDC(None);
            assert!(!dc.is_invalid());
            let font = |em: f64| {
                CreateFontW(
                    -(em.round() as i32),
                    0,
                    0,
                    0,
                    if bold { 600 } else { 450 },
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_TT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    quality,
                    0,
                    face,
                )
            };
            let metric_font = font(200.);
            let old_font = SelectObject(dc, metric_font.into());
            let mut gm = GLYPHMETRICS::default();
            let identity = MAT2 {
                eM11: FIXED { fract: 0, value: 1 },
                eM22: FIXED { fract: 0, value: 1 },
                ..Default::default()
            };
            assert_ne!(
                GetGlyphOutlineW(dc, u32::from(b'H'), GGO_METRICS, &mut gm, 0, None, &identity),
                u32::MAX
            );
            let final_font = font(cap / (f64::from(gm.gmBlackBoxY) / 200.));
            SelectObject(dc, final_font.into());
            let _ = DeleteObject(metric_font.into());
            let mut tm = TEXTMETRICW::default();
            assert!(GetTextMetricsW(dc, &mut tm).as_bool());
            let mut actual_face = [0u16; 128];
            let count = GetTextFaceW(dc, Some(&mut actual_face));
            println!(
                "face={} cap={cap:.3} em={} requested_weight={} actual_weight={} quality={}",
                String::from_utf16_lossy(&actual_face[..count.saturating_sub(1) as usize]),
                tm.tmHeight - tm.tmInternalLeading,
                if bold { 600 } else { 450 },
                tm.tmWeight,
                quality.0
            );

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: c.w as i32,
                    biHeight: -(c.h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old_bitmap = SelectObject(dc, bitmap.into());
            std::ptr::copy_nonoverlapping(c.px.as_ptr(), bits.cast(), c.px.len());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(fg.r as u32 | (fg.g as u32) << 8 | (fg.b as u32) << 16));
            let wide: Vec<u16> = text.encode_utf16().collect();
            let mut extent = SIZE::default();
            assert!(GetTextExtentPoint32W(dc, &wide, &mut extent).as_bool());
            assert!(
                TextOutW(
                    dc,
                    origin.0.round() as i32,
                    (origin.1 - (f64::from(tm.tmAscent) - cap)).round() as i32,
                    &wide
                )
                .as_bool()
            );
            let _ = GdiFlush();
            for (dst, src) in
                c.px.iter_mut()
                    .zip(std::slice::from_raw_parts(bits.cast::<u32>(), c.w * c.h))
            {
                *dst = 0xff000000 | (src & 0xffffff);
            }
            SelectObject(dc, old_bitmap);
            let _ = DeleteObject(bitmap.into());
            SelectObject(dc, old_font);
            let _ = DeleteObject(final_font.into());
            let _ = DeleteDC(dc);
        }
    }
    pub fn run() -> std::io::Result<()> {
        let out = std::path::PathBuf::from(std::env::args_os().nth(1).expect("output directory"));
        std::fs::create_dir_all(&out)?;
        // SAFETY: read-only system font-smoothing queries; never change preferences.
        unsafe {
            for (name, action) in [
                ("smoothing", SPI_GETFONTSMOOTHING),
                ("type", SPI_GETFONTSMOOTHINGTYPE),
                ("contrast", SPI_GETFONTSMOOTHINGCONTRAST),
                ("orientation", SPI_GETFONTSMOOTHINGORIENTATION),
            ] {
                let mut value = 0u32;
                SystemParametersInfoW(
                    action,
                    0,
                    Some((&mut value as *mut u32).cast()),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
                .unwrap();
                println!("system_{name}={value}");
            }
        }

        for percent in [100, 150, 200] {
            let scale = f64::from(percent) / 100.;
            for dark in [false, true] {
                let bg = if dark { 0xff312f2c } else { 0xffffffff };
                let fg = if dark {
                    Color::rgb(244., 244., 246.)
                } else {
                    Color::rgb(27., 28., 34.)
                };
                let secondary = if dark {
                    Color::rgb(174., 172., 168.)
                } else {
                    Color::rgb(95., 96., 106.)
                };
                let mut toast_pixels = Vec::new();
                for mode in [
                    "toast-before",
                    "toast-after",
                    "native-grayscale",
                    "native-cleartype",
                    "segoe-variable-cleartype",
                ] {
                    let font: Box<dyn TextRaster> = if mode == "toast-before" {
                        Box::new(MaskOnly(text::system_toast()))
                    } else {
                        text::system_toast()
                    };
                    let mut c = Canvas::new((520. * scale) as usize, (80. * scale) as usize);
                    c.px.fill(bg);
                    for (label, size, bold, y, color) in [
                        ("已禁用本机键盘", 14., true, 15., fg),
                        ("按住 Ctrl+Alt+Shift+U 1 秒解除", 13., false, 45., secondary),
                    ] {
                        let cap = size * 0.72 * scale;
                        if mode.starts_with("toast") {
                            let advance = text::width(&*font, cap, label, bold);
                            Pen::new(&mut c, &*font).centered(
                                8. * scale + advance / 2.,
                                y * scale,
                                cap,
                                label,
                                bold,
                                color,
                            );
                        } else {
                            line(
                                &mut c,
                                label,
                                cap,
                                bold,
                                (8. * scale, y * scale),
                                color,
                                (
                                    if mode == "native-grayscale" {
                                        ANTIALIASED_QUALITY
                                    } else {
                                        CLEARTYPE_QUALITY
                                    },
                                    if mode.starts_with("segoe") {
                                        w!("Segoe UI Variable")
                                    } else {
                                        w!("Microsoft YaHei UI")
                                    },
                                ),
                            );
                        }
                    }
                    if mode == "toast-after" {
                        toast_pixels = c.px.clone();
                    }
                    if mode == "native-cleartype" {
                        assert!(
                            toast_pixels == c.px,
                            "toast must match independently drawn native GDI output at {percent}% dark={dark}"
                        );
                    }
                    save(
                        &out.join(format!("{percent}-{}-{mode}.ppm", if dark { "dark" } else { "light" })),
                        &c,
                    )?;
                }
            }
        }
        Ok(())
    }
}
#[cfg(windows)]
fn main() -> std::io::Result<()> {
    native::run()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("This reference uses Windows GDI; run it on Windows without a desktop session.");
}
