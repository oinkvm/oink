//! 当前用户桌面的静态壁纸：IDesktopWallpaper + WIC。只读文件，不截屏、不记录来源路径。
//! 空路径表示系统纯色；不可解码的来源不产生缩略图。幻灯片取当前图片；第三方动态桌面仍以系统返回的
//! 静态图片为准，不判断它是否与实际桌面一致。

use oink_foundation::geom::{Point, Rect};
use windows::Win32::Foundation::{GENERIC_READ, HGLOBAL, RECT};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppPBGRA,
    IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, IPropertyBag2, PROPBAG2};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    DWPOS_CENTER, DWPOS_FILL, DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH, DWPOS_TILE, DesktopWallpaper, IDesktopWallpaper,
};
use windows::core::{PCWSTR, PWSTR};

use super::enumerate;
use crate::wallpaper::windows_geometry::{Position, display_for_monitor, image_rect, image_uv};
use crate::wallpaper::{DisplayWallpaper, LONG_EDGE, TARGET_BYTES, Thumbnail, thumbnail_size};

const QUALITIES: [f32; 4] = [0.72, 0.55, 0.4, 0.28];

pub(super) fn read_all() -> Option<Vec<DisplayWallpaper>> {
    // 服务是无窗口进程；调用方的 Tokio 阻塞线程未初始化 COM，或可能已有 MTA。不能在调用方直接
    // 初始化 STA（已有 apartment 时会 RPC_E_CHANGED_MODE），也不能借用 UI 线程（服务没有 UI）。
    // 每次刷新使用一个专用 STA 线程，所有 Shell/WIC 对象都在它上面创建、调用、释放，再反初始化 COM；
    // 只把纯数据交回调用方，不创建窗口，不影响输入钩子或 UI。Windows 11 真机验证通过。
    std::thread::Builder::new()
        .name("oink-wallpaper".into())
        .spawn(|| {
            let _com = Apartment::new()?;
            read_on_sta()
        })
        .ok()?
        .join()
        .ok()?
}

struct Apartment;
impl Apartment {
    fn new() -> Option<Self> {
        // SAFETY: 专用新线程没有已有 COM apartment；成功后由 Drop 配对 CoUninitialize。
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().ok()? };
        Some(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: 与本线程成功的 CoInitializeEx 配对，所有 COM 对象已在 read_on_sta 返回时释放。
        unsafe { CoUninitialize() };
    }
}

/// GetWallpaper / GetMonitorDevicePathAt 的字符串由 COM 分配，连空字符串也必须释放。
fn take_string(value: PWSTR) -> Option<String> {
    // SAFETY: value 来自上述成功的 COM 调用，是以 NUL 结尾的 CoTaskMem 字符串。
    unsafe {
        let text = if value.is_null() {
            Some(String::new())
        } else {
            value.to_string().ok()
        };
        CoTaskMemFree(Some(value.0.cast()));
        text
    }
}

fn bounds(r: RECT) -> Rect {
    Rect::new(
        f64::from(r.left),
        f64::from(r.top),
        f64::from(r.right) - f64::from(r.left),
        f64::from(r.bottom) - f64::from(r.top),
    )
}

fn read_on_sta() -> Option<Vec<DisplayWallpaper>> {
    let _dpi = super::dpi::ThreadDpiGuard::per_monitor_v2();
    let displays = enumerate::displays().ok()?;
    // SAFETY: 调用线程已初始化 STA；对象不离开本线程。
    let (wallpaper, factory): (IDesktopWallpaper, IWICImagingFactory) = unsafe {
        (
            CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL).ok()?,
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?,
        )
    };
    // SAFETY: 本线程持有有效接口；这里只读当前用户的桌面配置。
    let (position, color, count) = unsafe {
        (
            wallpaper.GetPosition().ok()?,
            wallpaper.GetBackgroundColor().ok()?.0,
            wallpaper.GetMonitorDevicePathCount().ok()?,
        )
    };
    let position = match position {
        DWPOS_CENTER => Position::Center,
        DWPOS_TILE => Position::Tile,
        DWPOS_STRETCH => Position::Stretch,
        DWPOS_FIT => Position::Fit,
        DWPOS_FILL => Position::Fill,
        DWPOS_SPAN => Position::Span,
        _ => return None,
    };
    // COLORREF 是 0x00BBGGRR；位图与 JPEG 编码器使用 BGR。
    let fill = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
    let mut monitors = Vec::new();
    for index in 0..count {
        // SAFETY: index 小于系统返回的 count；复制字符串后立即释放 COM 分配。
        let Some(path) = (unsafe { wallpaper.GetMonitorDevicePathAt(index) }.ok()).and_then(take_string) else {
            continue;
        };
        let wide = enumerate::wide(&path);
        // SAFETY: wide 在调用期间有效且 NUL 结尾。
        let Ok(r) = (unsafe { wallpaper.GetMonitorRECT(PCWSTR(wide.as_ptr())) }) else {
            continue;
        };
        let r = bounds(r);
        // 系统也会列出已拔掉的显示器；GetMonitorRECT 对它们返回 S_FALSE / 空矩形。
        if r.w > 0.0 && r.h > 0.0 {
            monitors.push((path, r));
        }
    }
    let desktop = monitors.iter().map(|(_, r)| *r).reduce(|a, b| {
        let (x, y) = (a.x.min(b.x), a.y.min(b.y));
        Rect::new(x, y, a.right().max(b.right()) - x, a.bottom().max(b.bottom()) - y)
    });
    let Some(desktop) = desktop else {
        return Some(Vec::new());
    };
    let mut out = Vec::new();
    for (path, r) in monitors {
        let Some(display) = display_for_monitor(&path, r, &displays) else {
            continue;
        };
        if out.iter().any(|w: &DisplayWallpaper| w.key == display.key) {
            continue;
        }
        let wide = enumerate::wide(&path);
        // 每屏指定 monitor ID，不能传 NULL：多图或幻灯片时 NULL 返回空字符串。
        // SAFETY: wide NUL 结尾，字符串由 take_string 释放。
        let Some(source) = (unsafe { wallpaper.GetWallpaper(PCWSTR(wide.as_ptr())) }.ok()).and_then(take_string) else {
            continue;
        };
        if let Some(thumbnail) = render(&factory, &source, fill, position, r, desktop) {
            out.push(DisplayWallpaper {
                key: display.key.clone(),
                thumbnail,
            });
        }
    }
    Some(out)
}

struct Pixels {
    width: u32,
    height: u32,
    /// WIC 预乘 alpha 的 BGRA，长边最多 960 px。
    data: Vec<u8>,
    native_width: u32,
    native_height: u32,
}

fn decode(factory: &IWICImagingFactory, path: &str) -> Option<Pixels> {
    if !std::path::Path::new(path).is_file() {
        return None;
    }
    let wide = enumerate::wide(path);
    // SAFETY: 接口在 STA 上；字符串 NUL 结尾；各输出缓冲区按尺寸分配。不接受视频或动态桌面句柄。
    unsafe {
        let decoder = factory
            .CreateDecoderFromFilename(PCWSTR(wide.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnLoad)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut iw, mut ih) = (0, 0);
        frame.GetSize(&mut iw, &mut ih).ok()?;
        if iw == 0 || ih == 0 {
            return None;
        }
        // 与 macOS 一样先解码到两倍缩略图长边；不会为每块屏幕分配整张 4K/8K 像素缓冲区。
        let scale = (f64::from(LONG_EDGE * 2) / f64::from(iw.max(ih))).min(1.0);
        let (w, h) = (
            (f64::from(iw) * scale).round().max(1.0) as u32,
            (f64::from(ih) * scale).round().max(1.0) as u32,
        );
        let converter = factory.CreateFormatConverter().ok()?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(&converter, w, h, WICBitmapInterpolationModeFant)
            .ok()?;
        let mut data = vec![0; (w * h * 4) as usize];
        scaler.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
        Some(Pixels {
            width: w,
            height: h,
            data,
            native_width: iw,
            native_height: ih,
        })
    }
}

/// 双线性采样预乘 BGRA。尺寸为一的图片也有效；边界钳制避免取到相邻行。
fn sample(image: &Pixels, u: f64, v: f64) -> [f64; 4] {
    let x = (u * f64::from(image.width) - 0.5).clamp(0.0, f64::from(image.width - 1));
    let y = (v * f64::from(image.height) - 0.5).clamp(0.0, f64::from(image.height - 1));
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(image.width - 1), (y0 + 1).min(image.height - 1));
    let (fx, fy) = (x - f64::from(x0), y - f64::from(y0));
    let at = |x, y, c| f64::from(image.data[((y * image.width + x) * 4) as usize + c]);
    std::array::from_fn(|c| {
        (at(x0, y0, c) * (1.0 - fx) + at(x1, y0, c) * fx) * (1.0 - fy)
            + (at(x0, y1, c) * (1.0 - fx) + at(x1, y1, c) * fx) * fy
    })
}

fn compose(
    image: Option<&Pixels>,
    fill: [u8; 3],
    position: Position,
    screen: Rect,
    desktop: Rect,
    tw: u32,
    th: u32,
) -> Vec<u8> {
    let placement = image.map(|i| {
        image_rect(
            position,
            f64::from(i.native_width),
            f64::from(i.native_height),
            screen,
            desktop,
        )
    });
    let mut bgr = vec![0; (tw * th * 3) as usize];
    for y in 0..th {
        for x in 0..tw {
            let pixel = &mut bgr[((y * tw + x) * 3) as usize..][..3];
            pixel.copy_from_slice(&fill);
            if let (Some(image), Some(placement)) = (image, placement) {
                let p = Point::new(
                    screen.x + (f64::from(x) + 0.5) * screen.w / f64::from(tw),
                    screen.y + (f64::from(y) + 0.5) * screen.h / f64::from(th),
                );
                if let Some((u, v)) = image_uv(position, placement, p) {
                    let bgra = sample(image, u, v);
                    for c in 0..3 {
                        pixel[c] = (bgra[c] + f64::from(fill[c]) * (1.0 - bgra[3] / 255.0))
                            .round()
                            .clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
    bgr
}

fn render(
    factory: &IWICImagingFactory,
    path: &str,
    fill: [u8; 3],
    position: Position,
    screen: Rect,
    desktop: Rect,
) -> Option<Thumbnail> {
    let (tw, th) = thumbnail_size(screen.w, screen.h)?;
    // 空路径是系统纯色；非空但读不了的来源按没有可用壁纸处理，不能把解码失败伪装成纯色。
    let image = if path.is_empty() {
        None
    } else {
        Some(decode(factory, path)?)
    };
    let bgr = compose(image.as_ref(), fill, position, screen, desktop, tw, th);
    let mut jpeg = Vec::new();
    for quality in QUALITIES {
        jpeg = encode(factory, &bgr, tw, th, quality)?;
        if jpeg.len() <= TARGET_BYTES {
            break;
        }
    }
    Some(Thumbnail {
        width: tw as u16,
        height: th as u16,
        jpeg,
    })
}

fn encode(factory: &IWICImagingFactory, bgr: &[u8], w: u32, h: u32, quality: f32) -> Option<Vec<u8>> {
    // SAFETY: WIC 接口留在 STA；流自己管理内存，返回前已完成编码；像素缓冲区大小为 w*h*3。
    unsafe {
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true).ok()?;
        let encoder = factory
            .CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())
            .ok()?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache).ok()?;
        let (mut frame, mut options): (Option<IWICBitmapFrameEncode>, Option<IPropertyBag2>) = (None, None);
        encoder.CreateNewFrame(&mut frame, &mut options).ok()?;
        let (frame, options) = (frame?, options?);
        let mut name = enumerate::wide("ImageQuality");
        let property = PROPBAG2 {
            pstrName: PWSTR(name.as_mut_ptr()),
            ..Default::default()
        };
        options.Write(1, &property, &VARIANT::from(quality)).ok()?;
        frame.Initialize(&options).ok()?;
        frame.SetSize(w, h).ok()?;
        let mut format = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format).ok()?;
        if format != GUID_WICPixelFormat24bppBGR {
            return None;
        }
        frame.WritePixels(h, w * 3, bgr).ok()?;
        frame.Commit().ok()?;
        encoder.Commit().ok()?;
        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        let len = u32::try_from(stat.cbSize).ok()?;
        let mut jpeg = vec![0; len as usize];
        stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
        let mut read = 0;
        stream.Read(jpeg.as_mut_ptr().cast(), len, Some(&mut read)).ok().ok()?;
        (read == len).then_some(jpeg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory() -> IWICImagingFactory {
        // SAFETY: 测试在已初始化 STA 的线程上调用。
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap() }
    }

    #[test]
    fn composition_preserves_bars_native_center_tiles_and_alpha() {
        let image = Pixels {
            width: 2,
            height: 1,
            native_width: 2,
            native_height: 1,
            data: vec![0, 0, 255, 255, 255, 0, 0, 255],
        };
        let screen = Rect::new(0.0, 0.0, 4.0, 4.0);
        let fill = [20, 40, 60];
        let at = |pixels: &[u8], x: usize, y: usize| pixels[(y * 4 + x) * 3..][..3].to_vec();
        let fit = compose(Some(&image), fill, Position::Fit, screen, screen, 4, 4);
        assert_eq!(at(&fit, 0, 0), fill);
        assert_eq!(at(&fit, 0, 1), [0, 0, 255]);
        assert_eq!(at(&fit, 3, 2), [255, 0, 0]);
        assert_eq!(at(&fit, 0, 3), fill);
        let center = compose(Some(&image), fill, Position::Center, screen, screen, 4, 4);
        assert_eq!(at(&center, 0, 2), fill);
        assert_eq!(at(&center, 1, 1), [0, 0, 255]);
        assert_eq!(at(&center, 2, 1), [255, 0, 0]);
        let tile = compose(Some(&image), fill, Position::Tile, screen, screen, 4, 4);
        assert_eq!(at(&tile, 0, 3), [0, 0, 255]);
        assert_eq!(at(&tile, 3, 3), [255, 0, 0]);
        let alpha = Pixels {
            width: 1,
            height: 1,
            native_width: 1,
            native_height: 1,
            data: vec![0, 0, 128, 128],
        };
        let pixels = compose(Some(&alpha), fill, Position::Stretch, screen, screen, 4, 4);
        assert_eq!(at(&pixels, 0, 0), [10, 20, 158]);
    }

    /// 人工构造的 BMP 条纹：WIC 文件解码、Fant 缩放、裁剪、JPEG 编码/解码全部走原生接口。
    #[test]
    fn wic_round_trip_crops_to_screen_aspect_and_renders_solid_color() {
        let _com = Apartment::new().unwrap();
        let factory = factory();
        let name = std::env::var("OINK_WORKTREE").unwrap_or_else(|_| "oink-display-wallpaper-tests".into());
        let dir = std::env::temp_dir()
            .join(name)
            .join(format!("wallpaper-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bands.bmp");
        let (w, h) = (800u32, 200u32);
        let size = w * h * 3;
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(size + 54).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&w.to_le_bytes());
        bmp.extend_from_slice(&h.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        for _ in 0..h {
            for x in 0..w {
                bmp.extend_from_slice(if x < w / 3 {
                    &[0, 0, 255]
                } else if x < 2 * w / 3 {
                    &[0, 255, 0]
                } else {
                    &[255, 0, 0]
                });
            }
        }
        std::fs::write(&path, bmp).unwrap();
        let screen = Rect::new(0.0, 0.0, 400.0, 200.0);
        let t = render(&factory, path.to_str().unwrap(), [0; 3], Position::Fill, screen, screen).unwrap();
        assert_eq!((t.width, t.height), (480, 240));
        assert!(t.jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(t.jpeg.len() <= TARGET_BYTES);
        let jpeg_path = dir.join("crop.jpg");
        std::fs::write(&jpeg_path, &t.jpeg).unwrap();
        let decoded = decode(&factory, jpeg_path.to_str().unwrap()).unwrap();
        assert_eq!((decoded.width, decoded.height), (480, 240));
        let red = sample(&decoded, 0.04, 0.5);
        let green = sample(&decoded, 0.5, 0.5);
        let blue = sample(&decoded, 0.96, 0.5);
        assert!(red[2] > 240.0 && red[0] < 10.0 && red[1] < 10.0);
        assert!(green[1] > 240.0 && green[0] < 10.0 && green[2] < 10.0);
        assert!(blue[0] > 240.0 && blue[1] < 10.0 && blue[2] < 10.0);
        let portrait = Rect::new(0.0, 0.0, 1080.0, 1920.0);
        let solid = render(&factory, "", [30, 60, 90], Position::Center, portrait, portrait).unwrap();
        assert_eq!((solid.width, solid.height), (270, 480));
        assert!(solid.jpeg.len() <= TARGET_BYTES);
        let solid_path = dir.join("solid.jpg");
        std::fs::write(&solid_path, &solid.jpeg).unwrap();
        let solid_pixels = decode(&factory, solid_path.to_str().unwrap()).unwrap();
        let actual = sample(&solid_pixels, 0.5, 0.5);
        for (a, b) in actual[..3].iter().zip([30.0, 60.0, 90.0]) {
            assert!((a - b).abs() < 3.0)
        }
        assert!(render(&factory, dir.to_str().unwrap(), [0; 3], Position::Fill, screen, screen).is_none());
        assert!(
            render(
                &factory,
                dir.join("missing.jpg").to_str().unwrap(),
                [0; 3],
                Position::Fill,
                screen,
                screen
            )
            .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
