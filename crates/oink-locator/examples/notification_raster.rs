//! Headless native-font evidence: writes PPM images, never creates a surface/window.
use oink_foundation::{Rect, display::DisplayInfo};
use oink_locator::{
    aim::text,
    notification::{Queue, Style, Toast},
};
use std::{
    io::Write,
    time::{Duration, Instant},
};
/// Reproduce the legacy recolored mask without changing the current card layout.
#[derive(Debug)]
struct Grayscale(Box<dyn text::TextRaster>);
impl text::TextRaster for Grayscale {
    fn line(&self, value: &str, cap: f64, bold: bool) -> text::Mask {
        self.0.line(value, cap, bold)
    }
}
fn main() -> std::io::Result<()> {
    let out = std::path::PathBuf::from(std::env::args_os().nth(1).expect("output directory"));
    std::fs::create_dir_all(&out)?;
    let font: Box<dyn text::TextRaster> = if std::env::args().nth(2).as_deref() == Some("--grayscale") {
        Box::new(Grayscale(text::system_toast()))
    } else {
        text::system_toast()
    };
    for percent in [100, 125, 150, 200] {
        for dark in [false, true] {
            let now = Instant::now();
            let mut q = Queue::default();
            q.push(
                Toast {
                    title: "已禁用本机键盘".into(),
                    description: "按住 Ctrl+Alt+Shift+U 1 秒解除".into(),
                    undo: Some("撤销".into()),
                    persistent: true,
                    scale: f64::from(percent) / 100.,
                    display: DisplayInfo {
                        key: "test".into(),
                        name: String::new(),
                        bounds: Rect::new(0., 0., 3840., 2160.),
                        pixel_width: 3840,
                        pixel_height: 2160,
                        reference_extent: None,
                        rotation_deg: 0,
                        is_primary: true,
                        is_builtin: false,
                        is_virtual: false,
                    },
                },
                now,
            );
            for (phase, ms) in [("enter", 80), ("settled", 1000)] {
                let f = q
                    .frames(
                        now + Duration::from_millis(ms),
                        Style {
                            dark,
                            ..Style::default()
                        },
                        false,
                        &*font,
                    )
                    .remove(0);
                let name = format!("{percent}-{}-{phase}.ppm", if dark { "dark" } else { "light" });
                let mut file = std::fs::File::create(out.join(name))?;
                write!(file, "P6\n{} {}\n255\n", f.width, f.height)?;
                for p in f.pixels {
                    let bg = if dark { 32 } else { 240 };
                    let a = p >> 24;
                    let channel = |shift: u32| (((p >> shift) & 255u32) + bg * (255 - a) / 255) as u8;
                    file.write_all(&[channel(16), channel(8), channel(0)])?;
                }
            }
        }
    }
    Ok(())
}
