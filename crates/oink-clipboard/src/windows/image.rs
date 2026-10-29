//! 图片格式转换（纯函数）：DIB（`CF_DIB` / `CF_DIBV5`）↔ PNG。
//!
//! - Read CORE/INFO/V4/V5 DIBs, palette 1/4/8-bit and 16/24/32-bit RGB or bitfields,
//!   RLE4/RLE8, and embedded PNG/JPEG. Rows may be bottom-up or top-down.
//!   BI_RGB's fourth byte is padding; explicit bitfield alpha stays unmodified,
//!   including fully transparent images. Alpha-bearing pixels are straight RGBA.
//! - 写入：`CF_DIBV5` 为 32 位 `BI_BITFIELDS`、带 alpha 掩码、sRGB、自底向上、非预乘；
//!   `CF_DIB` 为 24 位 `BI_RGB`，透明像素先与白色混合（不认 alpha 的程序也能正常显示）。

use std::io::Cursor;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_PNG: u32 = 5;
const BI_ALPHABITFIELDS: u32 = 6;
/// `LCS_sRGB`
const LCS_SRGB: u32 = 0x7352_4742;
/// `LCS_GM_IMAGES`
const LCS_GM_IMAGES: u32 = 4;
/// 拒绝解码的最大像素数（防止畸形数据导致巨大分配）。
const MAX_PIXELS: u64 = crate::model::REPRESENTATION_BYTES / 4;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error("image exceeds representation budget")]
    LimitExceeded,
    #[error("{0}")]
    InvalidData(String),
}
impl DecodeError {
    pub(crate) fn reason(&self) -> crate::model::Unavailable {
        match self {
            Self::LimitExceeded => crate::model::Unavailable::LimitExceeded,
            Self::InvalidData(_) => crate::model::Unavailable::InvalidData,
        }
    }
}
impl From<String> for DecodeError {
    fn from(value: String) -> Self {
        Self::InvalidData(value)
    }
}
impl From<&str> for DecodeError {
    fn from(value: &str) -> Self {
        Self::InvalidData(value.into())
    }
}

/// RGBA8、行优先、自顶向下。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rgba {
    pub pixels_per_meter: Option<(u32, u32)>,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn i32_at(b: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// 一个颜色通道的掩码：取值后换算到 0..=255。
#[derive(Clone, Copy, Debug)]
struct Channel {
    mask: u32,
    shift: u32,
    max: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        if mask == 0 {
            return Self {
                mask: 0,
                shift: 0,
                max: 0,
            };
        }
        let shift = mask.trailing_zeros();
        Self {
            mask,
            shift,
            max: mask >> shift,
        }
    }
    fn get(&self, px: u32) -> u8 {
        if self.max == 0 {
            return 0;
        }
        let v = (px & self.mask) >> self.shift;
        if self.max == 255 {
            v as u8
        } else {
            ((u64::from(v) * 255 + u64::from(self.max) / 2) / u64::from(self.max)) as u8
        }
    }
}

/// 解析打包的 DIB（`BITMAPINFO` + 颜色表 + 像素）。返回 RGBA；`BI_PNG` 时返回 `Err(Embedded(png))`。
pub(crate) enum Decoded {
    Pixels(Rgba),
    EmbeddedPng(Vec<u8>),
}

pub(crate) fn decode_dib(dib: &[u8]) -> Result<Decoded, DecodeError> {
    let bad = || "DIB 数据不完整".to_string();
    let header_size = u32_at(dib, 0).ok_or_else(bad)? as usize;
    if header_size == 12 {
        let decoder = image::codecs::bmp::BmpDecoder::new_without_file_header(Cursor::new(dib))
            .map_err(|e| DecodeError::InvalidData(e.to_string()))?;
        return decode_image(decoder, None).map(Decoded::Pixels);
    }
    if !matches!(header_size, 40 | 52 | 56 | 108 | 124) || header_size > dib.len() {
        return Err(format!("不支持的 DIB 头（{header_size} 字节）").into());
    }
    let width = i32_at(dib, 4).ok_or_else(bad)?;
    let height = i32_at(dib, 8).ok_or_else(bad)?;
    let bpp = u16_at(dib, 14).ok_or_else(bad)?;
    if u16_at(dib, 12) != Some(1) {
        return Err("invalid DIB planes".into());
    }
    let compression = u32_at(dib, 16).ok_or_else(bad)?;
    let size_image = u32_at(dib, 20).ok_or_else(bad)? as usize;
    let clr_used = u32_at(dib, 32).ok_or_else(bad)? as usize;
    if width <= 0 || height == 0 {
        return Err(format!("DIB 尺寸无效：{width}×{height}").into());
    }
    let (w, h) = (width as u32, height.unsigned_abs());
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(DecodeError::LimitExceeded);
    }
    let top_down = height < 0;

    let resolution = || {
        let x = i32_at(dib, 24).unwrap_or(0).max(0) as u32;
        let y = i32_at(dib, 28).unwrap_or(0).max(0) as u32;
        (x != 0 || y != 0).then_some((x, y))
    };
    if matches!(compression, 1 | 2) {
        if top_down || (compression == 1 && bpp != 8) || (compression == 2 && bpp != 4) {
            return Err("invalid compressed DIB depth/orientation".into());
        }
        let decoder = image::codecs::bmp::BmpDecoder::new_without_file_header(Cursor::new(dib))
            .map_err(|e| DecodeError::InvalidData(e.to_string()))?;
        return decode_image(decoder, resolution()).map(Decoded::Pixels);
    }
    if matches!(compression, BI_PNG | 4) {
        let end = if size_image > 0 {
            header_size
                .checked_add(size_image)
                .ok_or("invalid compressed DIB length")?
        } else {
            dib.len()
        };
        let bytes = dib.get(header_size..end).ok_or_else(bad)?;
        if compression == BI_PNG {
            let decoded = decode_png(bytes)?;
            if (decoded.width, decoded.height) != (w, h) {
                return Err("embedded PNG dimensions mismatch".into());
            }
            return Ok(Decoded::EmbeddedPng(bytes.to_vec()));
        }
        let decoder = image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes))
            .map_err(|e| DecodeError::InvalidData(e.to_string()))?;
        let image = decode_image(decoder, resolution())?;
        if (image.width, image.height) != (w, h) {
            return Err("embedded JPEG dimensions mismatch".into());
        }
        return Ok(Decoded::Pixels(image));
    }
    if !matches!(compression, BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS) {
        return Err(format!("不支持的 DIB 压缩方式 {compression}").into());
    }

    if !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) || (compression != BI_RGB && !matches!(bpp, 16 | 32)) {
        return Err("unsupported DIB depth/compression pair".into());
    }
    // 掩码：V4/V5 头里自带；40 字节头时跟在头后面
    let mut offset = header_size;
    let (mut rm, mut gm, mut bm, mut am) = (0u32, 0u32, 0u32, 0u32);
    let bitfields = compression == BI_BITFIELDS || compression == BI_ALPHABITFIELDS;
    if bitfields {
        if header_size >= 52 {
            rm = u32_at(dib, 40).ok_or_else(bad)?;
            gm = u32_at(dib, 44).ok_or_else(bad)?;
            bm = u32_at(dib, 48).ok_or_else(bad)?;
            if header_size >= 56 {
                am = u32_at(dib, 52).ok_or_else(bad)?;
            }
        } else {
            rm = u32_at(dib, offset).ok_or_else(bad)?;
            gm = u32_at(dib, offset + 4).ok_or_else(bad)?;
            bm = u32_at(dib, offset + 8).ok_or_else(bad)?;
            offset += 12;
            if compression == BI_ALPHABITFIELDS {
                am = u32_at(dib, offset).ok_or_else(bad)?;
                offset += 4;
            }
        }
    } else if bpp == 16 {
        (rm, gm, bm) = (0x7C00, 0x03E0, 0x001F);
    } else if bpp == 32 {
        (rm, gm, bm, am) = (0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0);
        // BI_RGB's fourth byte is padding, including nonzero values. Only
        // explicit bitfield masks grant alpha semantics to a DIB.
    }

    if bitfields {
        let valid_mask = |mask: u32| {
            if mask == 0 {
                return false;
            }
            let bits = mask >> mask.trailing_zeros();
            bits & bits.wrapping_add(1) == 0 && (bpp == 32 || mask < (1u32 << bpp))
        };
        if ![rm, gm, bm].into_iter().all(valid_mask)
            || (am != 0 && !valid_mask(am))
            || rm & gm != 0
            || rm & bm != 0
            || gm & bm != 0
            || (rm | gm | bm) & am != 0
        {
            return Err("invalid DIB channel masks".into());
        }
    }
    let mut palette: Vec<[u8; 3]> = Vec::new();
    if bpp <= 8 {
        let n = if clr_used > 0 { clr_used.min(256) } else { 1usize << bpp };
        for i in 0..n {
            let e = dib.get(offset + i * 4..offset + i * 4 + 4).ok_or_else(bad)?;
            palette.push([e[2], e[1], e[0]]);
        }
        offset += n * 4;
    } else if clr_used > 0 {
        // 高色深也可以带（无用的）颜色表
        offset += clr_used * 4;
    }

    let stride = (w as usize * bpp as usize).div_ceil(32) * 4;
    let pixels_len = stride * h as usize;
    let pixels = dib.get(offset..offset + pixels_len).ok_or_else(bad)?;

    let (r, g, b, a) = (Channel::new(rm), Channel::new(gm), Channel::new(bm), Channel::new(am));
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let src_row = if top_down { y } else { h as usize - 1 - y };
        let row = &pixels[src_row * stride..src_row * stride + stride];
        for x in 0..w as usize {
            let o = (y * w as usize + x) * 4;
            let (pr, pg, pb, pa) = match bpp {
                1 | 4 | 8 => {
                    let bit = x * bpp as usize;
                    let byte = row[bit / 8];
                    let idx = match bpp {
                        8 => byte as usize,
                        4 => ((byte >> (4 - (bit % 8))) & 0x0f) as usize,
                        _ => ((byte >> (7 - (bit % 8))) & 0x01) as usize,
                    };
                    let c = palette.get(idx).copied().ok_or("DIB palette index out of range")?;
                    (c[0], c[1], c[2], 255)
                }
                16 => {
                    let px = u32::from(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]));
                    (r.get(px), g.get(px), b.get(px), if am != 0 { a.get(px) } else { 255 })
                }
                24 => (row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255),
                32 => {
                    let px = u32::from_le_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]]);
                    (r.get(px), g.get(px), b.get(px), if am != 0 { a.get(px) } else { 255 })
                }
                other => return Err(format!("不支持的色深 {other}").into()),
            };
            out[o..o + 4].copy_from_slice(&[pr, pg, pb, pa]);
        }
    }
    Ok(Decoded::Pixels(Rgba {
        pixels_per_meter: resolution(),
        width: w,
        height: h,
        data: out,
    }))
}

/// Decode legacy/core, RLE and embedded-JPEG sources only after bounding the
/// expanded image. These codecs must share the same representation budget.
fn decode_image(decoder: impl image::ImageDecoder, pixels_per_meter: Option<(u32, u32)>) -> Result<Rgba, DecodeError> {
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS || decoder.total_bytes() > crate::model::REPRESENTATION_BYTES {
        return Err(DecodeError::LimitExceeded);
    }
    let data = image::DynamicImage::from_decoder(decoder)
        .map_err(|e| DecodeError::InvalidData(e.to_string()))?
        .into_rgba8()
        .into_raw();
    Ok(Rgba {
        pixels_per_meter,
        width,
        height,
        data,
    })
}

/// CF_TIFF is an encoded TIFF stream, not a DIB header. Keep its decode under
/// the same expanded-pixel and allocation budgets as the other image formats.
pub(crate) fn tiff_to_png(bytes: &[u8]) -> Result<Vec<u8>, DecodeError> {
    use image::ImageDecoder;
    if bytes.len() as u64 > crate::model::REPRESENTATION_BYTES {
        return Err(DecodeError::LimitExceeded);
    }
    let failure = |error: image::ImageError| match error {
        image::ImageError::Limits(_) => DecodeError::LimitExceeded,
        _ => DecodeError::InvalidData(error.to_string()),
    };
    let mut decoder = image::codecs::tiff::TiffDecoder::new(Cursor::new(bytes)).map_err(failure)?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(crate::model::REPRESENTATION_BYTES);
    decoder.set_limits(limits).map_err(failure)?;
    encode_png(&decode_image(decoder, None)?).map_err(Into::into)
}

/// DIB → PNG。
pub(crate) fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, DecodeError> {
    match decode_dib(dib)? {
        Decoded::EmbeddedPng(png) => Ok(png),
        Decoded::Pixels(img) => encode_png(&img).map_err(Into::into),
    }
}

/// Decode the bitmap medium's width/height + bottom-up 32-bit BGRX contract.
pub(crate) fn bitmap_to_png(bytes: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let width = i32_at(bytes, 0).ok_or("incomplete bitmap dimensions")?;
    let height = i32_at(bytes, 4).ok_or("incomplete bitmap dimensions")?;
    if width <= 0 || height <= 0 {
        return Err("invalid bitmap dimensions".into());
    }
    let pixels = width as u64 * height as u64;
    if pixels > MAX_PIXELS {
        return Err(DecodeError::LimitExceeded);
    }
    if pixels * 4 != bytes.len().saturating_sub(8) as u64 {
        return Err("invalid bitmap pixel length".into());
    }
    let stride = width as usize * 4;
    let mut data = Vec::with_capacity(bytes.len() - 8);
    // GetDIBits serializes CF_BITMAP as BI_RGB, not an alpha-bearing DIBV5.
    // Its fourth byte is unused even when nonzero; PNG must be opaque. Reverse
    // rows because the native serializer requests a positive bitmap height.
    for row in bytes[8..].chunks_exact(stride).rev() {
        for px in row.as_chunks::<4>().0 {
            data.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    encode_png(&Rgba {
        pixels_per_meter: None,
        width: width as u32,
        height: height as u32,
        data,
    })
    .map_err(Into::into)
}

/// RGBA → PNG；完全不透明时写成 RGB（更小）。
pub(crate) fn encode_png(img: &Rgba) -> Result<Vec<u8>, String> {
    let opaque = img.data.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_pixel_dims(img.pixels_per_meter.map(|(xppu, yppu)| png::PixelDimensions {
            xppu,
            yppu,
            unit: png::Unit::Meter,
        }));
        enc.set_depth(png::BitDepth::Eight);
        enc.set_color(if opaque {
            png::ColorType::Rgb
        } else {
            png::ColorType::Rgba
        });
        let mut writer = enc.write_header().map_err(|e| format!("PNG 编码：{e}"))?;
        if opaque {
            let rgb: Vec<u8> = img
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            writer.write_image_data(&rgb)
        } else {
            writer.write_image_data(&img.data)
        }
        .map_err(|e| format!("PNG 编码：{e}"))?;
    }
    Ok(out)
}

/// PNG → RGBA8（灰度、调色板、16 位等都转成 8 位 RGBA）。
pub(crate) fn decode_png(bytes: &[u8]) -> Result<Rgba, DecodeError> {
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let header = dec
        .read_header_info()
        .map_err(|e| DecodeError::InvalidData(e.to_string()))?;
    if u64::from(header.width) * u64::from(header.height) > MAX_PIXELS {
        return Err(DecodeError::LimitExceeded);
    }
    let mut reader = dec.read_info().map_err(|e| format!("PNG 解码：{e}"))?;
    let size = reader.output_buffer_size().ok_or("PNG 过大")?;
    if u64::from(reader.info().width) * u64::from(reader.info().height) > MAX_PIXELS
        || size as u64 > crate::model::REPRESENTATION_BYTES
    {
        return Err(DecodeError::LimitExceeded);
    }
    let pixels_per_meter = reader
        .info()
        .pixel_dims
        .filter(|d| d.unit == png::Unit::Meter)
        .map(|d| (d.xppu, d.yppu));
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("PNG 解码：{e}"))?;
    if u64::from(info.width) * u64::from(info.height) > MAX_PIXELS {
        return Err(DecodeError::LimitExceeded);
    }
    buf.truncate(info.buffer_size());
    let n = info.width as usize * info.height as usize;
    let data: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::Indexed => return Err("PNG 调色板未展开".into()),
    };
    if data.len() != n * 4 {
        return Err("PNG 像素数据长度不符".into());
    }
    Ok(Rgba {
        pixels_per_meter,
        width: info.width,
        height: info.height,
        data,
    })
}

fn put_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// RGBA → `CF_DIBV5`（`BITMAPV5HEADER` + 32 位像素，自底向上，非预乘 alpha）。
pub(crate) fn rgba_to_dibv5(img: &Rgba) -> Vec<u8> {
    let (w, h) = (img.width as usize, img.height as usize);
    let mut v = Vec::with_capacity(124 + w * h * 4);
    put_u32(&mut v, 124); // bV5Size
    put_u32(&mut v, img.width); // bV5Width
    put_u32(&mut v, img.height); // bV5Height（正：自底向上）
    put_u16(&mut v, 1); // bV5Planes
    put_u16(&mut v, 32); // bV5BitCount
    put_u32(&mut v, BI_BITFIELDS); // bV5Compression
    put_u32(&mut v, (w * h * 4) as u32); // bV5SizeImage
    put_u32(&mut v, img.pixels_per_meter.map_or(0, |p| p.0));
    put_u32(&mut v, img.pixels_per_meter.map_or(0, |p| p.1));
    put_u32(&mut v, 0); // bV5ClrUsed
    put_u32(&mut v, 0); // bV5ClrImportant
    put_u32(&mut v, 0x00FF_0000); // bV5RedMask
    put_u32(&mut v, 0x0000_FF00); // bV5GreenMask
    put_u32(&mut v, 0x0000_00FF); // bV5BlueMask
    put_u32(&mut v, 0xFF00_0000); // bV5AlphaMask
    put_u32(&mut v, LCS_SRGB); // bV5CSType
    v.extend_from_slice(&[0u8; 36]); // bV5Endpoints
    put_u32(&mut v, 0); // bV5GammaRed
    put_u32(&mut v, 0); // bV5GammaGreen
    put_u32(&mut v, 0); // bV5GammaBlue
    put_u32(&mut v, LCS_GM_IMAGES); // bV5Intent
    put_u32(&mut v, 0); // bV5ProfileData
    put_u32(&mut v, 0); // bV5ProfileSize
    put_u32(&mut v, 0); // bV5Reserved
    debug_assert_eq!(v.len(), 124);
    for y in (0..h).rev() {
        for px in img.data[y * w * 4..(y + 1) * w * 4].as_chunks::<4>().0 {
            v.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    v
}

/// RGBA → `CF_DIB`（`BITMAPINFOHEADER` + 24 位像素，自底向上，透明部分与白色混合）。
pub(crate) fn rgba_to_dib24(img: &Rgba) -> Vec<u8> {
    let (w, h) = (img.width as usize, img.height as usize);
    let stride = (w * 3).div_ceil(4) * 4;
    let mut v = Vec::with_capacity(40 + stride * h);
    put_u32(&mut v, 40);
    put_u32(&mut v, img.width);
    put_u32(&mut v, img.height);
    put_u16(&mut v, 1);
    put_u16(&mut v, 24);
    put_u32(&mut v, BI_RGB);
    put_u32(&mut v, (stride * h) as u32);
    put_u32(&mut v, img.pixels_per_meter.map_or(0, |p| p.0));
    put_u32(&mut v, img.pixels_per_meter.map_or(0, |p| p.1));
    put_u32(&mut v, 0);
    put_u32(&mut v, 0);
    let blend = |c: u8, a: u8| -> u8 { ((u32::from(c) * u32::from(a) + 255 * (255 - u32::from(a)) + 127) / 255) as u8 };
    for y in (0..h).rev() {
        let start = v.len();
        for px in img.data[y * w * 4..(y + 1) * w * 4].as_chunks::<4>().0 {
            v.extend_from_slice(&[blend(px[2], px[3]), blend(px[1], px[3]), blend(px[0], px[3])]);
        }
        v.resize(start + stride, 0);
    }
    v
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn tiff_preserves_asymmetric_pixels_and_rejects_invalid_or_oversized_images() {
        let image = sample();
        let mut encoded = Cursor::new(Vec::new());
        image::codecs::tiff::TiffEncoder::new(&mut encoded)
            .encode(&image.data, image.width, image.height, image::ExtendedColorType::Rgba8)
            .unwrap();
        let mut bytes = encoded.into_inner();
        assert_eq!(decode_png(&tiff_to_png(&bytes).unwrap()).unwrap(), image);
        for bad in [b"invalid TIFF".as_slice(), &bytes[..8]] {
            assert_eq!(
                tiff_to_png(bad).unwrap_err().reason(),
                crate::model::Unavailable::InvalidData
            );
        }
        // A well-formed directory can declare enormous dimensions while the
        // encoded input is tiny. Reject it before allocating the decoded image.
        assert_eq!(&bytes[..2], b"II");
        let ifd = u32_at(&bytes, 4).unwrap() as usize;
        let entries = u16_at(&bytes, ifd).unwrap() as usize;
        let width = (0..entries)
            .map(|n| ifd + 2 + 12 * n)
            .find(|offset| u16_at(&bytes, *offset) == Some(256))
            .unwrap();
        assert_eq!(u16_at(&bytes, width + 2), Some(4));
        bytes[width + 8..width + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            tiff_to_png(&bytes).unwrap_err().reason(),
            crate::model::Unavailable::LimitExceeded
        );
    }

    fn sample() -> Rgba {
        // 3×2：第一行 红 半透明绿 透明蓝；第二行 白 黑 灰
        Rgba {
            pixels_per_meter: None,
            width: 3,
            height: 2,
            data: vec![
                255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, //
                255, 255, 255, 255, 0, 0, 0, 255, 128, 128, 128, 255,
            ],
        }
    }

    fn pixels(d: Decoded) -> Rgba {
        match d {
            Decoded::Pixels(p) => p,
            Decoded::EmbeddedPng(_) => panic!("unexpected embedded png"),
        }
    }

    #[test]
    fn bitmap_bgrx_is_bottom_up_and_always_opaque() {
        let bytes = [
            1i32.to_le_bytes().as_slice(),
            2i32.to_le_bytes().as_slice(),
            &[0, 0, 255, 127, 255, 0, 0, 0],
        ]
        .concat();
        let png = bitmap_to_png(&bytes).unwrap();
        let decoded = decode_png(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (1, 2));
        assert_eq!(decoded.data, [0, 0, 255, 255, 255, 0, 0, 255]);
        assert!(bitmap_to_png(&bytes[..bytes.len() - 1]).is_err());
        assert!(bitmap_to_png(&[bytes.clone(), vec![0]].concat()).is_err());
        assert!(bitmap_to_png(&[]).is_err());
        for (width, height) in [(0i32, 1i32), (1, -1), (i32::MAX, i32::MAX)] {
            assert!(bitmap_to_png(&[width.to_le_bytes(), height.to_le_bytes()].concat()).is_err());
        }
    }

    #[test]
    fn explicit_zero_alpha_stays_transparent_but_bi_rgb_padding_is_ignored() {
        let transparent = Rgba {
            pixels_per_meter: None,
            width: 1,
            height: 1,
            data: vec![250, 20, 10, 0],
        };
        assert_eq!(pixels(decode_dib(&rgba_to_dibv5(&transparent)).unwrap()), transparent);
        let mut rgb = rgba_to_dibv5(&transparent);
        rgb[0..4].copy_from_slice(&40u32.to_le_bytes());
        rgb[16..20].copy_from_slice(&BI_RGB.to_le_bytes());
        rgb.drain(40..124);
        rgb[43] = 127;
        assert_eq!(pixels(decode_dib(&rgb).unwrap()).data, [250, 20, 10, 255]);
    }
    #[test]
    fn packed_v5_profile_after_pixels_does_not_shift_the_pixels() {
        let image = sample();
        let mut dib = rgba_to_dibv5(&image);
        dib[56..60].copy_from_slice(&0x4d424544u32.to_le_bytes()); // PROFILE_EMBEDDED
        let profile_offset = dib.len() as u32;
        dib[112..116].copy_from_slice(&profile_offset.to_le_bytes());
        dib[116..120].copy_from_slice(&12u32.to_le_bytes());
        dib.extend_from_slice(&[42; 12]);
        assert_eq!(pixels(decode_dib(&dib).unwrap()), image);
    }
    #[test]
    fn png_dib_conversion_preserves_physical_resolution() {
        let mut dib = rgba_to_dibv5(&sample());
        dib[24..28].copy_from_slice(&11811u32.to_le_bytes());
        dib[28..32].copy_from_slice(&5906u32.to_le_bytes());
        let png = dib_to_png(&dib).unwrap();
        let reader = png::Decoder::new(Cursor::new(&png)).read_info().unwrap();
        let dims = reader.info().pixel_dims.unwrap();
        assert_eq!((dims.xppu, dims.yppu), (11811, 5906));
        assert_eq!(dims.unit, png::Unit::Meter);
        for roundtrip in [
            rgba_to_dibv5(&decode_png(&png).unwrap()),
            rgba_to_dib24(&decode_png(&png).unwrap()),
        ] {
            assert_eq!(u32_at(&roundtrip, 24), Some(11811));
            assert_eq!(u32_at(&roundtrip, 28), Some(5906));
        }
    }
    #[test]
    fn dibv5_roundtrip_keeps_alpha() {
        let img = sample();
        let dib = rgba_to_dibv5(&img);
        assert_eq!(dib.len(), 124 + 3 * 2 * 4);
        assert_eq!(pixels(decode_dib(&dib).unwrap()), img);
    }

    #[test]
    fn dib24_blends_over_white() {
        let img = sample();
        let dib = rgba_to_dib24(&img);
        // stride = 12 (9 → 12)
        assert_eq!(dib.len(), 40 + 12 * 2);
        let out = pixels(decode_dib(&dib).unwrap());
        assert_eq!(&out.data[0..4], &[255, 0, 0, 255]);
        // 半透明绿 → (127,255,127)
        assert_eq!(&out.data[4..8], &[127, 255, 127, 255]);
        // 全透明 → 白
        assert_eq!(&out.data[8..12], &[255, 255, 255, 255]);
        assert_eq!(&out.data[12..24], &img.data[12..24]);
    }

    #[test]
    fn png_roundtrip() {
        let img = sample();
        let png = encode_png(&img).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(decode_png(&png).unwrap(), img);
        // 不透明图片写成 RGB，解码回来一致
        let mut opaque = img.clone();
        for px in opaque.data.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        assert_eq!(decode_png(&encode_png(&opaque).unwrap()).unwrap(), opaque);
    }

    /// 手工构造 40 字节头的 32 位 BI_RGB（alpha 字节全 0）与自顶向下的情况。
    #[test]
    fn bi_rgb32_with_zero_alpha_is_opaque_and_top_down() {
        let mut v = Vec::new();
        put_u32(&mut v, 40);
        put_u32(&mut v, 2);
        put_u32(&mut v, (-1i32) as u32); // 自顶向下
        put_u16(&mut v, 1);
        put_u16(&mut v, 32);
        put_u32(&mut v, BI_RGB);
        v.extend_from_slice(&[0u8; 20]);
        v.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // 红、蓝（BGRX）
        let out = pixels(decode_dib(&v).unwrap());
        assert_eq!(out.data, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn bitfields_after_info_header_565() {
        let mut v = Vec::new();
        put_u32(&mut v, 40);
        put_u32(&mut v, 1);
        put_u32(&mut v, 1);
        put_u16(&mut v, 1);
        put_u16(&mut v, 16);
        put_u32(&mut v, BI_BITFIELDS);
        v.extend_from_slice(&[0u8; 20]);
        put_u32(&mut v, 0xF800);
        put_u32(&mut v, 0x07E0);
        put_u32(&mut v, 0x001F);
        v.extend_from_slice(&0x07E0u16.to_le_bytes()); // 纯绿
        v.extend_from_slice(&[0, 0]); // 行对齐
        let out = pixels(decode_dib(&v).unwrap());
        assert_eq!(out.data, vec![0, 255, 0, 255]);
    }

    #[test]
    fn palette_8bit_and_1bit() {
        let mut v = Vec::new();
        put_u32(&mut v, 40);
        put_u32(&mut v, 2);
        put_u32(&mut v, 1);
        put_u16(&mut v, 1);
        put_u16(&mut v, 1);
        put_u32(&mut v, BI_RGB);
        v.extend_from_slice(&[0u8; 12]);
        put_u32(&mut v, 2); // biClrUsed
        put_u32(&mut v, 0);
        v.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]); // 黑、白
        v.extend_from_slice(&[0b0100_0000, 0, 0, 0]); // 像素 0=黑 1=白
        let out = pixels(decode_dib(&v).unwrap());
        assert_eq!(out.data, vec![0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn embedded_png_is_passed_through() {
        let png = encode_png(&sample()).unwrap();
        let mut v = Vec::new();
        put_u32(&mut v, 40);
        put_u32(&mut v, 3);
        put_u32(&mut v, 2);
        put_u16(&mut v, 1);
        put_u16(&mut v, 0);
        put_u32(&mut v, BI_PNG);
        put_u32(&mut v, png.len() as u32);
        v.extend_from_slice(&[0u8; 16]);
        v.extend_from_slice(&png);
        assert_eq!(dib_to_png(&v).unwrap(), png);
    }

    #[test]
    fn generated_dib_depth_and_orientation_conversion_pairs() {
        for bpp in [1u16, 4, 8, 16, 24, 32] {
            for top_down in [false, true] {
                let mut dib = Vec::new();
                put_u32(&mut dib, 40);
                put_u32(&mut dib, 2);
                put_u32(&mut dib, if top_down { (-2i32) as u32 } else { 2 });
                put_u16(&mut dib, 1);
                put_u16(&mut dib, bpp);
                put_u32(&mut dib, BI_RGB);
                dib.resize(40, 0);
                if bpp <= 8 {
                    dib[32..36].copy_from_slice(&2u32.to_le_bytes());
                    dib.extend_from_slice(&[0, 0, 0, 0, 0, 0, 255, 0]);
                }
                let (black_red, red_black) = match bpp {
                    1 => (vec![0x40, 0, 0, 0], vec![0x80, 0, 0, 0]),
                    4 => (vec![0x01, 0, 0, 0], vec![0x10, 0, 0, 0]),
                    8 => (vec![0, 1, 0, 0], vec![1, 0, 0, 0]),
                    16 => (vec![0, 0, 0, 0x7c], vec![0, 0x7c, 0, 0]),
                    24 => (vec![0, 0, 0, 0, 0, 255, 0, 0], vec![0, 0, 255, 0, 0, 0, 0, 0]),
                    _ => (vec![0, 0, 0, 123, 0, 0, 255, 123], vec![0, 0, 255, 123, 0, 0, 0, 123]),
                };
                if top_down {
                    dib.extend_from_slice(&black_red);
                    dib.extend_from_slice(&red_black);
                } else {
                    dib.extend_from_slice(&red_black);
                    dib.extend_from_slice(&black_red);
                }
                let expected = [0, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 0, 255];
                assert_eq!(
                    pixels(decode_dib(&dib).unwrap()).data,
                    expected,
                    "bpp={bpp} top_down={top_down}"
                );
                assert_eq!(decode_png(&dib_to_png(&dib).unwrap()).unwrap().data, expected);
            }
        }
    }
    #[test]
    fn malformed_palette_masks_and_planes_are_unavailable() {
        let mut palette = vec![0; 40];
        palette[..4].copy_from_slice(&40u32.to_le_bytes());
        palette[4..8].copy_from_slice(&1u32.to_le_bytes());
        palette[8..12].copy_from_slice(&1u32.to_le_bytes());
        palette[12..14].copy_from_slice(&1u16.to_le_bytes());
        palette[14..16].copy_from_slice(&8u16.to_le_bytes());
        palette[32..36].copy_from_slice(&1u32.to_le_bytes());
        palette.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0]);
        assert!(decode_dib(&palette).is_err(), "palette index out of range");
        let valid = rgba_to_dibv5(&sample());
        for (offset, bytes) in [
            (12, vec![2, 0]),
            (44, 0x00ff_0000u32.to_le_bytes().to_vec()),
            (40, 0x0055_0000u32.to_le_bytes().to_vec()),
        ] {
            let mut bad = valid.clone();
            bad[offset..offset + bytes.len()].copy_from_slice(&bytes);
            assert!(decode_dib(&bad).is_err(), "invalid header field {offset}");
        }
    }
    #[test]
    fn bitmap_core_header_and_rle_palette_pairs_convert_to_png() {
        let mut core = Vec::new();
        put_u32(&mut core, 12);
        put_u16(&mut core, 2);
        put_u16(&mut core, 1);
        put_u16(&mut core, 1);
        put_u16(&mut core, 24);
        core.extend_from_slice(&[0, 0, 255, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_png(&dib_to_png(&core).unwrap()).unwrap().data,
            [255, 0, 0, 255, 0, 0, 0, 255]
        );
        for (bpp, compression, run) in [
            (8u16, 1u32, vec![1, 1, 1, 0, 0, 0, 0, 1]),
            (4, 2, vec![2, 0x10, 0, 0, 0, 1]),
        ] {
            let mut dib = vec![0; 40];
            dib[..4].copy_from_slice(&40u32.to_le_bytes());
            dib[4..8].copy_from_slice(&2u32.to_le_bytes());
            dib[8..12].copy_from_slice(&1u32.to_le_bytes());
            dib[12..14].copy_from_slice(&1u16.to_le_bytes());
            dib[14..16].copy_from_slice(&bpp.to_le_bytes());
            dib[16..20].copy_from_slice(&compression.to_le_bytes());
            dib[20..24].copy_from_slice(&(run.len() as u32).to_le_bytes());
            dib[32..36].copy_from_slice(&2u32.to_le_bytes());
            dib.extend_from_slice(&[0, 0, 0, 0, 0, 0, 255, 0]);
            dib.extend_from_slice(&run);
            assert_eq!(
                decode_png(&dib_to_png(&dib).unwrap()).unwrap().data,
                [255, 0, 0, 255, 0, 0, 0, 255],
                "RLE {bpp}"
            );
        }
    }
    #[test]
    fn embedded_jpeg_dib_converts_without_exposing_compressed_bytes_as_png() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&[250, 20, 10], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let mut dib = vec![0; 40];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1u32.to_le_bytes());
        dib[8..12].copy_from_slice(&1u32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[16..20].copy_from_slice(&4u32.to_le_bytes());
        dib[20..24].copy_from_slice(&(jpeg.len() as u32).to_le_bytes());
        dib.extend_from_slice(&jpeg);
        let image = decode_png(&dib_to_png(&dib).unwrap()).unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        assert!(image.data[0] > 240);
    }
    #[test]
    fn compressed_image_dimensions_are_bounded_before_pixel_decoding() {
        let mut bytes = Vec::new();
        let mut enc = png::Encoder::new(&mut bytes, 65536, 1025);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_chunk(png::chunk::IDAT, &[]).unwrap();
        writer.finish().unwrap();
        assert_eq!(
            decode_png(&bytes).unwrap_err().reason(),
            crate::model::Unavailable::LimitExceeded
        );
    }
    #[test]
    fn rejects_truncated() {
        let dib = rgba_to_dibv5(&sample());
        assert!(decode_dib(&dib[..dib.len() - 1]).is_err());
        assert!(decode_dib(&dib[..20]).is_err());
    }
}
