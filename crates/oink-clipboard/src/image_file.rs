//! Native decoding of an image file into PNG bitmap data, and bounded
//! previews (thumbnails) of encoded images.
//!
//! A proxy publishes a phone's photo the way the OS itself publishes a copied
//! picture: the file plus a bitmap representation. Only the platform image
//! codecs (ImageIO, WIC) are used, so HEIC and other system formats decode
//! exactly as they do for local apps; EXIF orientation is applied.
use std::{io, path::Path};

/// Largest edge of the produced bitmap. Bounds decode memory for hostile or
/// panoramic inputs (8192² × 4 B stays within the representation budget).
pub const MAX_EDGE: u32 = 8192;

/// Decode `path` with the system codecs and encode it as PNG. `Unsupported`
/// means this system has no decoder for the file's format.
pub fn png_from_image_file(path: &Path) -> io::Result<Vec<u8>> {
    let png = platform::png(path)?;
    if png.len() as u64 > crate::model::REPRESENTATION_BYTES {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(png)
}

/// A preview a phone shows instead of an image it did not download
/// (protocol-v2 §4.4 `ContentThumbnail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    /// `image/jpeg` for an opaque image, `image/png` when it has transparency.
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
    /// Size of the encoded original the preview was made from.
    pub source_bytes: u64,
}

/// The encodings a thumbnail uses (protocol-v2 `THUMBNAIL_MIMES`).
pub const THUMBNAIL_MIMES: [&str; 2] = ["image/jpeg", "image/png"];

/// JPEG qualities tried in order before the edge is reduced.
const QUALITIES: [f64; 3] = [0.8, 0.65, 0.5];
/// Smallest edge tried when the byte limit cannot be met at a larger size.
const MIN_EDGE: u32 = 64;

/// Decode `encoded` with the system codecs (EXIF orientation applied) and
/// encode a preview whose long edge is at most `max_edge` (never enlarged)
/// and whose encoding is at most `max_bytes`. The aspect ratio is the
/// original's. Lower JPEG qualities, then smaller edges are tried until the
/// byte limit holds; `FileTooLarge` if even the smallest edge does not fit.
/// `Unsupported` means this system has no decoder for the data.
pub fn thumbnail(encoded: &[u8], max_edge: u32, max_bytes: usize) -> io::Result<Thumbnail> {
    if encoded.len() as u64 > crate::model::REPRESENTATION_BYTES {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    let source = platform::Source::new(encoded)?;
    let mut edge = max_edge.max(1);
    loop {
        let image = source.scaled(edge)?;
        let (width, height) = (image.width(), image.height());
        if image.opaque() {
            for quality in QUALITIES {
                let bytes = image.jpeg(quality)?;
                if bytes.len() <= max_bytes {
                    return Ok(Thumbnail {
                        mime: "image/jpeg",
                        width,
                        height,
                        bytes,
                        source_bytes: encoded.len() as u64,
                    });
                }
            }
        } else {
            let bytes = image.png()?;
            if bytes.len() <= max_bytes {
                return Ok(Thumbnail {
                    mime: "image/png",
                    width,
                    height,
                    bytes,
                    source_bytes: encoded.len() as u64,
                });
            }
        }
        if edge <= MIN_EDGE {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        // Continue from the produced size: an image smaller than `edge` was
        // not enlarged, so shrinking `edge` alone could repeat the same try.
        edge = (width.max(height) * 3 / 4).max(MIN_EDGE);
    }
}

/// Target size for a long edge of at most `edge`, never enlarging.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn fit(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= edge {
        return (width, height);
    }
    let scale = f64::from(edge) / f64::from(longest);
    (
        ((f64::from(width) * scale).round() as u32).clamp(1, edge),
        ((f64::from(height) * scale).round() as u32).clamp(1, edge),
    )
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_core_foundation::{
        CFBoolean, CFData, CFDictionary, CFMutableData, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
    };
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapContextGetBytesPerRow, CGBitmapContextGetData,
        CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGInterpolationQuality, kCGColorSpaceSRGB,
    };
    use objc2_image_io::{
        CGImageDestination, CGImageSource, kCGImageDestinationLossyCompressionQuality,
        kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
        kCGImageSourceThumbnailMaxPixelSize,
    };
    use std::{io, path::Path};

    /// Decoded once; each `scaled` call renders one bounded, upright frame.
    pub(super) struct Source(CFRetained<CGImageSource>);
    pub(super) struct Image {
        image: CFRetained<CGImage>,
        width: u32,
        height: u32,
        opaque: bool,
    }

    impl Source {
        pub(super) fn new(encoded: &[u8]) -> io::Result<Self> {
            let data = CFData::from_bytes(encoded);
            // SAFETY: owned immutable bytes and default options.
            let source = unsafe { CGImageSource::with_data(&data, None) }.ok_or(io::ErrorKind::Unsupported)?;
            // SAFETY: read-only query.
            if unsafe { source.count() } == 0 {
                return Err(io::ErrorKind::Unsupported.into());
            }
            Ok(Self(source))
        }

        pub(super) fn scaled(&self, edge: u32) -> io::Result<Image> {
            let unsupported = || io::Error::from(io::ErrorKind::Unsupported);
            // The ImageIO thumbnail applies the EXIF orientation and decodes
            // at most `edge` pixels on the long side (see `png` below).
            // SAFETY: ImageIO option constants have documented CF types.
            let keys: [&CFString; 3] = unsafe {
                [
                    kCGImageSourceCreateThumbnailFromImageAlways,
                    kCGImageSourceCreateThumbnailWithTransform,
                    kCGImageSourceThumbnailMaxPixelSize,
                ]
            };
            let max = CFNumber::new_i64(i64::from(edge));
            let values: [&CFType; 3] = [CFBoolean::new(true), CFBoolean::new(true), &max];
            let options = CFDictionary::from_slices(&keys, &values);
            // SAFETY: frame zero exists (checked in `new`); corrupt data returns None.
            let image = unsafe { self.0.thumbnail_at_index(0, Some(options.as_opaque())) }.ok_or_else(unsupported)?;
            let (iw, ih) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
            if iw == 0 || ih == 0 {
                return Err(unsupported());
            }
            let (width, height) = super::fit(iw as u32, ih as u32, edge);
            // SAFETY: reads an exported CoreGraphics constant.
            let space =
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or_else(|| io::Error::other("sRGB"))?;
            // SAFETY: a null data pointer makes CoreGraphics allocate and own
            // the bitmap (bytes per row 0 = computed); RGBA premultiplied.
            let context = unsafe {
                CGBitmapContextCreate(
                    std::ptr::null_mut(),
                    width as usize,
                    height as usize,
                    8,
                    0,
                    Some(&space),
                    CGImageAlphaInfo::PremultipliedLast.0,
                )
            }
            .ok_or_else(|| io::Error::other("bitmap context"))?;
            let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(f64::from(width), f64::from(height)));
            CGContext::clear_rect(Some(&context), rect);
            CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::High);
            CGContext::draw_image(Some(&context), rect, Some(&image));
            let data = CGBitmapContextGetData(Some(&context)).cast::<u8>();
            let row = CGBitmapContextGetBytesPerRow(Some(&context));
            if data.is_null() || row < width as usize * 4 {
                return Err(io::Error::other("bitmap context"));
            }
            let opaque = (0..height as usize).all(|y| {
                // SAFETY: the context owns `height` rows of `row` bytes; each
                // row holds `width` RGBA pixels and the context outlives the slice.
                let line = unsafe { std::slice::from_raw_parts(data.add(y * row), width as usize * 4) };
                line.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == u8::MAX)
            });
            let image = CGBitmapContextCreateImage(Some(&context)).ok_or_else(|| io::Error::other("bitmap image"))?;
            Ok(Image {
                image,
                width,
                height,
                opaque,
            })
        }
    }

    impl Image {
        pub(super) fn width(&self) -> u32 {
            self.width
        }
        pub(super) fn height(&self) -> u32 {
            self.height
        }
        pub(super) fn opaque(&self) -> bool {
            self.opaque
        }
        pub(super) fn jpeg(&self, quality: f64) -> io::Result<Vec<u8>> {
            encode(&self.image, "public.jpeg", Some(quality))
        }
        pub(super) fn png(&self) -> io::Result<Vec<u8>> {
            encode(&self.image, "public.png", None)
        }
    }

    fn encode(image: &CGImage, uti: &'static str, quality: Option<f64>) -> io::Result<Vec<u8>> {
        let out = CFMutableData::new(None, 0).ok_or_else(|| io::Error::other("CFData allocation failed"))?;
        // SAFETY: the destination owns exactly one image; the CFData outlives finalize.
        let dest = unsafe { CGImageDestination::with_data(&out, &CFString::from_static_str(uti), 1, None) }
            .ok_or_else(|| io::Error::other("image encoder unavailable"))?;
        let quality = quality.map(CFNumber::new_f64);
        // SAFETY: reads an exported ImageIO constant.
        let keys: [&CFString; 1] = unsafe { [kCGImageDestinationLossyCompressionQuality] };
        let properties = quality.as_ref().map(|quality| {
            let values: [&CFType; 1] = [quality];
            CFDictionary::<CFString, CFType>::from_slices(&keys, &values)
        });
        // SAFETY: the declared single image with documented property types.
        unsafe { dest.add_image(image, properties.as_ref().map(|p| p.as_opaque())) };
        // SAFETY: as above.
        if !unsafe { dest.finalize() } {
            return Err(io::Error::other("image encoding failed"));
        }
        Ok(out.to_vec())
    }

    pub(super) fn png(path: &Path) -> io::Result<Vec<u8>> {
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > crate::model::REPRESENTATION_BYTES {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let data = CFData::from_bytes(&std::fs::read(path)?);
        let unsupported = || io::Error::from(io::ErrorKind::Unsupported);
        // SAFETY: owned immutable bytes and default options.
        let source = unsafe { CGImageSource::with_data(&data, None) }.ok_or_else(unsupported)?;
        // A full-size "thumbnail" is the documented ImageIO way to decode with
        // the EXIF orientation applied; the max edge bounds decode memory.
        // SAFETY: ImageIO option constants have documented CF types.
        let keys: [&CFString; 3] = unsafe {
            [
                kCGImageSourceCreateThumbnailFromImageAlways,
                kCGImageSourceCreateThumbnailWithTransform,
                kCGImageSourceThumbnailMaxPixelSize,
            ]
        };
        let max = CFNumber::new_i64(i64::from(super::MAX_EDGE));
        let values: [&CFType; 3] = [CFBoolean::new(true), CFBoolean::new(true), &max];
        let options = CFDictionary::from_slices(&keys, &values);
        // SAFETY: frame zero with typed ImageIO options; corrupt data returns None.
        let image = unsafe { source.thumbnail_at_index(0, Some(options.as_opaque())) }.ok_or_else(unsupported)?;
        let out = CFMutableData::new(None, 0).ok_or_else(|| io::Error::other("CFData allocation failed"))?;
        // SAFETY: the destination owns exactly one image; the CFData outlives finalize.
        let dest = unsafe { CGImageDestination::with_data(&out, &CFString::from_static_str("public.png"), 1, None) }
            .ok_or_else(|| io::Error::other("PNG encoder unavailable"))?;
        // SAFETY: the declared single image is added before finalize.
        unsafe { dest.add_image(&image, None) };
        // SAFETY: as above.
        if !unsafe { dest.finalize() } {
            return Err(io::Error::other("PNG encoding failed"));
        }
        Ok(out.to_vec())
    }
}

#[cfg(windows)]
mod platform {
    use std::{io, os::windows::ffi::OsStrExt, path::Path};
    use windows::{
        Win32::{
            Foundation::{GENERIC_READ, RPC_E_CHANGED_MODE, WINCODEC_ERR_BADIMAGE, WINCODEC_ERR_COMPONENTNOTFOUND},
            Graphics::Imaging::*,
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
                StructuredStorage::{PROPVARIANT, PropVariantClear, PropVariantToUInt16},
            },
        },
        core::{Interface, PCWSTR, w},
    };

    pub(super) fn png(path: &Path) -> io::Result<Vec<u8>> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let (width, height, pixels) = with_com(|factory| {
            // SAFETY: the NUL-terminated path buffer outlives the call.
            let decoder = unsafe {
                factory.CreateDecoderFromFilename(
                    PCWSTR(wide.as_ptr()),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnDemand,
                )
            }?;
            decode(factory, &decoder, super::MAX_EDGE)
        })?;
        crate::windows::encode_rgba_png(width, height, pixels).map_err(io::Error::other)
    }

    /// The encoded bytes are decoded again for each size; COM objects never
    /// leave the call that created them.
    pub(super) struct Source<'a>(&'a [u8]);
    pub(super) struct Image {
        width: u32,
        height: u32,
        /// Straight (not premultiplied) RGBA rows.
        pixels: Vec<u8>,
    }

    impl<'a> Source<'a> {
        pub(super) fn new(encoded: &'a [u8]) -> io::Result<Self> {
            Ok(Self(encoded))
        }

        pub(super) fn scaled(&self, edge: u32) -> io::Result<Image> {
            let (width, height, pixels) = with_com(|factory| {
                // SAFETY: WIC reads the borrowed buffer, which outlives the
                // stream and decoder created and dropped inside this closure.
                let decoder = unsafe {
                    let stream = factory.CreateStream()?;
                    stream.InitializeFromMemory(self.0)?;
                    factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?
                };
                decode(factory, &decoder, edge)
            })?;
            Ok(Image { width, height, pixels })
        }
    }

    impl Image {
        pub(super) fn width(&self) -> u32 {
            self.width
        }
        pub(super) fn height(&self) -> u32 {
            self.height
        }
        pub(super) fn opaque(&self) -> bool {
            self.pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == u8::MAX)
        }
        pub(super) fn jpeg(&self, quality: f64) -> io::Result<Vec<u8>> {
            use image::ImageEncoder;
            let rgb: Vec<u8> = self
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[r, g, b, _]| [r, g, b])
                .collect();
            let mut out = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, (quality * 100.0).round() as u8)
                .write_image(&rgb, self.width, self.height, image::ExtendedColorType::Rgb8)
                .map_err(io::Error::other)?;
            Ok(out)
        }
        pub(super) fn png(&self) -> io::Result<Vec<u8>> {
            crate::windows::encode_rgba_png(self.width, self.height, self.pixels.clone()).map_err(io::Error::other)
        }
    }

    fn with_com<T>(work: impl FnOnce(&IWICImagingFactory) -> windows::core::Result<T>) -> io::Result<T> {
        // The caller runs on a blocking worker thread; COM is initialized for
        // this call only. A thread already in another apartment keeps it.
        // SAFETY: balanced with CoUninitialize below when it succeeded.
        let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if init.is_err() && init != RPC_E_CHANGED_MODE {
            return Err(io::Error::other(init.message()));
        }
        // SAFETY: the factory is used on this thread only.
        let result = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            .and_then(|factory: IWICImagingFactory| work(&factory));
        if init.is_ok() {
            // SAFETY: pairs the successful CoInitializeEx above; every COM
            // object of `work` has been dropped.
            unsafe { CoUninitialize() };
        }
        result.map_err(|error| {
            // WINCODEC_ERR_COMPONENTNOTFOUND: no codec is installed for this
            // format (for example HEIC without the HEIF extension).
            if error.code() == WINCODEC_ERR_COMPONENTNOTFOUND {
                io::ErrorKind::Unsupported.into()
            } else {
                io::Error::other(error.message())
            }
        })
    }

    /// Frame zero as straight RGBA, at most `max_edge` on the long side
    /// (never enlarged), with the EXIF orientation applied.
    fn decode(
        factory: &IWICImagingFactory,
        decoder: &IWICBitmapDecoder,
        max_edge: u32,
    ) -> windows::core::Result<(u32, u32, Vec<u8>)> {
        // SAFETY: COM objects are used on this thread only and every out
        // pointer is a live local.
        unsafe {
            let frame = decoder.GetFrame(0)?;
            let (mut width, mut height) = (0u32, 0u32);
            frame.GetSize(&mut width, &mut height)?;
            if width == 0 || height == 0 {
                return Err(windows::core::Error::from_hresult(WINCODEC_ERR_BADIMAGE));
            }
            let converter = factory.CreateFormatConverter()?;
            converter.Initialize(
                &frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;
            let mut source: IWICBitmapSource = converter.cast()?;
            let (target_width, target_height) = super::fit(width, height, max_edge);
            if (target_width, target_height) != (width, height) {
                let scaler = factory.CreateBitmapScaler()?;
                scaler.Initialize(
                    &source,
                    target_width,
                    target_height,
                    WICBitmapInterpolationModeHighQualityCubic,
                )?;
                source = scaler.cast()?;
            }
            if let Some(transform) = orientation(&frame).and_then(transform) {
                let rotator = factory.CreateBitmapFlipRotator()?;
                rotator.Initialize(&source, transform)?;
                source = rotator.cast()?;
            }
            source.GetSize(&mut width, &mut height)?;
            let stride = width as usize * 4;
            let mut pixels = vec![0u8; stride * height as usize];
            source.CopyPixels(std::ptr::null(), stride as u32, &mut pixels)?;
            Ok((width, height, pixels))
        }
    }

    /// EXIF orientation through WIC's photo metadata policy (JPEG, TIFF, HEIF).
    fn orientation(frame: &IWICBitmapFrameDecode) -> Option<u16> {
        // SAFETY: the query reader and PROPVARIANT are owned locals.
        unsafe {
            let reader = frame.GetMetadataQueryReader().ok()?;
            let mut value = PROPVARIANT::default();
            reader
                .GetMetadataByName(w!("System.Photo.Orientation"), &mut value)
                .ok()?;
            let orientation = PropVariantToUInt16(&value).ok();
            let _ = PropVariantClear(&mut value);
            orientation
        }
    }

    /// WIC rotates clockwise; EXIF 2/4/5/7 mirror.
    fn transform(orientation: u16) -> Option<WICBitmapTransformOptions> {
        Some(match orientation {
            2 => WICBitmapTransformFlipHorizontal,
            3 => WICBitmapTransformRotate180,
            4 => WICBitmapTransformFlipVertical,
            5 => WICBitmapTransformOptions(WICBitmapTransformRotate90.0 | WICBitmapTransformFlipHorizontal.0),
            6 => WICBitmapTransformRotate90,
            7 => WICBitmapTransformOptions(WICBitmapTransformRotate270.0 | WICBitmapTransformFlipHorizontal.0),
            8 => WICBitmapTransformRotate270,
            _ => return None,
        })
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use std::io;
    pub(super) fn png(_: &std::path::Path) -> io::Result<Vec<u8>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub(super) struct Source;
    pub(super) enum Image {}
    impl Source {
        pub(super) fn new(_: &[u8]) -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub(super) fn scaled(&self, _: u32) -> io::Result<Image> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
    impl Image {
        pub(super) fn width(&self) -> u32 {
            match *self {}
        }
        pub(super) fn height(&self) -> u32 {
            match *self {}
        }
        pub(super) fn opaque(&self) -> bool {
            match *self {}
        }
        pub(super) fn jpeg(&self, _: f64) -> io::Result<Vec<u8>> {
            match *self {}
        }
        pub(super) fn png(&self) -> io::Result<Vec<u8>> {
            match *self {}
        }
    }
}

#[cfg(all(test, any(target_os = "macos", windows)))]
mod tests {
    use super::*;

    #[test]
    fn jpeg_file_decodes_to_an_upright_png() {
        use image::ImageEncoder;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        // 4 × 2 landscape pixels.
        let rgb: Vec<u8> = (0..8).flat_map(|i| [i as u8 * 30, 100, 200]).collect();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .write_image(&rgb, 4, 2, image::ExtendedColorType::Rgb8)
            .unwrap();
        std::fs::write(&path, &jpeg).unwrap();
        let png = png_from_image_file(&path).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((width, height), (4, 2));
    }

    #[test]
    fn non_image_file_is_reported_unsupported_or_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.heic");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(png_from_image_file(&path).is_err());
    }

    fn jpeg(width: u32, height: u32, mut pixel: impl FnMut(u32, u32) -> [u8; 3]) -> Vec<u8> {
        use image::ImageEncoder;
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                rgb.extend(pixel(x, y));
            }
        }
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
            .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    fn png_rgba(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[40, 90, 200, alpha].repeat((width * height) as usize))
            .unwrap();
        writer.finish().unwrap();
        out
    }

    /// The JPEG with an APP1 Exif segment whose IFD0 has only Orientation.
    fn with_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut tiff = b"II*\0".to_vec();
        tiff.extend(8u32.to_le_bytes());
        tiff.extend(1u16.to_le_bytes());
        tiff.extend(0x0112u16.to_le_bytes());
        tiff.extend(3u16.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        tiff.extend(orientation.to_le_bytes());
        tiff.extend([0, 0]);
        tiff.extend(0u32.to_le_bytes());
        let mut segment = vec![0xff, 0xe1];
        segment.extend(((2 + 6 + tiff.len()) as u16).to_be_bytes());
        segment.extend(b"Exif\0\0");
        segment.extend(tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend(segment);
        out.extend(&jpeg[2..]);
        out
    }

    fn dimensions(thumbnail: &Thumbnail) -> (u32, u32) {
        let image = image::load_from_memory(&thumbnail.bytes).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn opaque_image_thumbnail_is_a_bounded_jpeg_with_the_original_aspect() {
        let source = jpeg(2000, 1000, |x, y| [(x % 256) as u8, (y % 256) as u8, 120]);
        let thumbnail = thumbnail(&source, 512, 256 * 1024).unwrap();
        assert_eq!(thumbnail.mime, "image/jpeg");
        assert_eq!((thumbnail.width, thumbnail.height), (512, 256));
        assert_eq!(dimensions(&thumbnail), (512, 256));
        assert!(thumbnail.bytes.len() <= 256 * 1024);
        assert_eq!(&thumbnail.bytes[..2], &[0xff, 0xd8]);
        assert_eq!(thumbnail.source_bytes, source.len() as u64);
    }

    #[test]
    fn transparent_image_thumbnail_stays_png_and_small_images_are_not_enlarged() {
        let thumbnail = thumbnail(&png_rgba(100, 40, 128), 512, 64 * 1024).unwrap();
        assert_eq!(thumbnail.mime, "image/png");
        assert_eq!((thumbnail.width, thumbnail.height), (100, 40));
        assert_eq!(&thumbnail.bytes[..8], b"\x89PNG\r\n\x1a\n");
        // An opaque PNG (a copied screenshot) becomes JPEG.
        let opaque = super::thumbnail(&png_rgba(100, 40, 255), 512, 64 * 1024).unwrap();
        assert_eq!(opaque.mime, "image/jpeg");
        assert_eq!(dimensions(&opaque), (100, 40));
    }

    #[test]
    fn exif_orientation_is_applied_to_the_thumbnail() {
        // Stored 40 × 20; orientation 6 displays it rotated 90° clockwise.
        let source = with_orientation(&jpeg(40, 20, |x, _| if x < 20 { [255, 0, 0] } else { [0, 0, 255] }), 6);
        let thumbnail = thumbnail(&source, 512, 64 * 1024).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (20, 40));
        let image = image::load_from_memory(&thumbnail.bytes).unwrap().to_rgb8();
        assert_eq!((image.width(), image.height()), (20, 40));
        // The stored left (red) half is the displayed top half.
        let top = image.get_pixel(10, 5);
        let bottom = image.get_pixel(10, 34);
        assert!(top[0] > 200 && top[2] < 60, "{top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 60, "{bottom:?}");
    }

    #[test]
    fn byte_limit_lowers_quality_then_edge_and_fails_only_below_the_minimum() {
        // Noise does not compress: the limit forces a smaller edge.
        let mut seed = 7u32;
        let source = jpeg(1024, 1024, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed.to_le_bytes()[..3].try_into().unwrap()
        });
        let thumbnail = thumbnail(&source, 512, 16 * 1024).unwrap();
        assert!(thumbnail.bytes.len() <= 16 * 1024);
        assert!(thumbnail.width < 512 && thumbnail.width == thumbnail.height);
        assert_eq!(dimensions(&thumbnail), (thumbnail.width, thumbnail.height));
        assert_eq!(
            super::thumbnail(&source, 512, 100).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
    }

    #[test]
    fn undecodable_bytes_have_no_thumbnail() {
        assert!(thumbnail(b"not an image", 512, 64 * 1024).is_err());
    }
}
