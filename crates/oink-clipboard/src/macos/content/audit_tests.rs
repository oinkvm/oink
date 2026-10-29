use super::*;

fn representation(name: &str) -> Representation {
    Representation {
        id: RepresentationId(1),
        format: NativeFormat::Mac(name.into()),
        standard: standard(name),
        medium: Medium::Bytes,
        aspect: 1,
        index: -1,
        data: RepresentationData::Inline(vec![]),
    }
}
fn png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut bytes, 1, 1);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&[250, 20, 10, 128])
            .unwrap();
    }
    bytes
}
#[test]
fn text_conversion_pairs_validate_utf8_and_both_utf16_byte_orders() {
    for name in [
        "public.utf8-plain-text",
        "public.utf16-plain-text",
        "public.utf16-external-plain-text",
        "NSStringPboardType",
    ] {
        let rep = representation(name);
        let text = "猫😀\r\né";
        let mut cases = vec![text.as_bytes().to_vec()];
        if name.contains("utf16") {
            cases = vec![
                text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                [vec![255, 254], text.encode_utf16().flat_map(u16::to_le_bytes).collect()].concat(),
                [vec![254, 255], text.encode_utf16().flat_map(u16::to_be_bytes).collect()].concat(),
            ];
        }
        for bytes in cases {
            assert_eq!(
                MacTranslator.standard(&rep, &bytes).unwrap(),
                vec![ClipData::Text("猫😀\né".into())],
                "{name}"
            );
        }
        for bad in [vec![255], vec![255, 254, 0], vec![255, 254, 0, 216]] {
            assert_eq!(
                content::reason(&MacTranslator.standard(&rep, &bad).unwrap_err()),
                Unavailable::InvalidData,
                "{name}"
            );
        }
    }
}
#[test]
fn generated_bitmap_conversion_pairs() {
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
    autoreleasepool(|_| {
        let source = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(&png())).unwrap();
        for (name, kind) in [
            ("public.png", NSBitmapImageFileType::PNG),
            ("public.tiff", NSBitmapImageFileType::TIFF),
            ("public.jpeg", NSBitmapImageFileType::JPEG),
        ] {
            // SAFETY: empty encoding properties are valid for these bitmap types.
            let bytes =
                unsafe { source.representationUsingType_properties(kind, &objc2_foundation::NSDictionary::new()) }
                    .unwrap();
            let rep = representation(name);
            assert_eq!(rep.standard, Some(StandardFormat::Png));
            let converted = MacTranslator.standard(&rep, &bytes.to_vec()).unwrap();
            let ClipData::Png(bytes) = &converted[0] else { panic!() };
            let image = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(bytes)).unwrap();
            assert_eq!((image.pixelsWide(), image.pixelsHigh()), (1, 1));
            assert!(MacTranslator.standard(&rep, b"broken image").is_err(), "{name}");
        }
    });
}
#[test]
fn every_foreign_binding_remains_readable_after_an_earlier_candidate_fails() {
    use crate::content::tests::{Portable, fixture};
    autoreleasepool(|_| {
        for kind in [
            StandardFormat::Text,
            StandardFormat::Html,
            StandardFormat::Rtf,
            StandardFormat::Png,
        ] {
            let mut inventory = fixture().inventory();
            inventory.platform = ClipPlatform::Windows;
            for (i, rep) in inventory.items[0].representations.iter_mut().enumerate() {
                rep.format = NativeFormat::WindowsRegistered(format!("test.{i}"));
                rep.standard = Some(kind);
                rep.data = if i == 0 {
                    RepresentationData::Reader {
                        size: ContentSize::Unknown,
                    }
                } else {
                    RepresentationData::Inline(match kind {
                        StandardFormat::Png => png(),
                        StandardFormat::Rtf => b"{\\rtf1\\ansi\\ansicpg1252 caf\\'e9}".to_vec(),
                        _ => b"valid".to_vec(),
                    })
                };
            }
            let spec = NativeSpec {
                contents: Arc::new(Portable(inventory.clone())),
                inventory,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            let name = match kind {
                StandardFormat::Text => "public.utf8-plain-text",
                StandardFormat::Html => "public.html",
                StandardFormat::Rtf => "public.rtf",
                _ => "public.png",
            };
            let board = crate::macos::test_pasteboard::new();
            write(Some(&board.name().to_string()), &spec, &HashMap::new(), &|| Ok(())).unwrap();
            let read = |item: &NSPasteboardItem| {
                item.dataForType(&NSString::from_str(name))
                    .expect("later candidate was hidden")
                    .to_vec()
            };
            let expected = match &spec.inventory.items[0].representations[1].data {
                RepresentationData::Inline(bytes) => bytes.clone(),
                _ => unreachable!(),
            };
            assert_eq!(read(&board.pasteboardItems().unwrap().objectAtIndex(0)), expected);
            for (_, item) in promise_items(&spec).unwrap() {
                assert_eq!(read(&item), expected);
            }
            board.clearContents();
        }
    });
}
#[test]
fn bitmap_conversion_preserves_resolution_and_type_classification_is_explicit() {
    use objc2_app_kit::NSBitmapImageRep;
    autoreleasepool(|_| {
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 2, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_pixel_dims(Some(png::PixelDimensions {
                xppu: 11811,
                yppu: 5906,
                unit: png::Unit::Meter,
            }));
            enc.write_header()
                .unwrap()
                .write_image_data(&[250, 20, 10, 255].repeat(4))
                .unwrap();
        }
        let source = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(&bytes)).unwrap();
        let tiff = source.TIFFRepresentation().unwrap();
        let converted = MacTranslator
            .standard(&representation("public.tiff"), &tiff.to_vec())
            .unwrap();
        let ClipData::Png(bytes) = &converted[0] else { panic!() };
        let output = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(bytes)).unwrap();
        assert!((source.size().width - output.size().width).abs() < 0.001);
        assert!((source.size().height - output.size().height).abs() < 0.001);
        for name in [
            "public.png",
            "public.tiff",
            "public.jpeg",
            "public.heic",
            "com.apple.pict",
            "com.apple.flat-rtfd",
            "public.url-name",
            "public.file-url",
            "example.private",
        ] {
            println!("NATIVE_CLASSIFICATION {name} {:?}", standard(name));
        }
        for name in [
            "com.apple.flat-rtfd",
            "public.url-name",
            "public.file-url",
            "example.private",
        ] {
            assert_eq!(standard(name), None);
        }
    });
}
#[test]
fn generated_heic_converts_when_os_advertises_decoder() {
    use objc2_app_kit::NSBitmapImageRep;
    use objc2_foundation::NSMutableData;
    use std::ffi::c_void;
    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageSourceCreateWithData(data: *const c_void, options: *const c_void) -> *const c_void;
        fn CGImageSourceCreateImageAtIndex(source: *const c_void, index: usize, options: *const c_void) -> *mut c_void;
        fn CGImageDestinationCreateWithData(
            data: *mut c_void,
            ty: *const c_void,
            count: usize,
            options: *const c_void,
        ) -> *mut c_void;
        fn CGImageDestinationAddImage(destination: *mut c_void, image: *const c_void, properties: *const c_void);
        fn CGImageDestinationFinalize(destination: *mut c_void) -> bool;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
    }
    autoreleasepool(|_| {
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 32, 32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()
                .unwrap()
                .write_image_data(&[250, 20, 10].repeat(1024))
                .unwrap();
        }
        assert_eq!(standard("public.heic"), Some(StandardFormat::Png));
        let input = NSData::with_bytes(&bytes);
        let output = NSMutableData::new();
        let ty = NSString::from_str("public.heic");
        // SAFETY: ImageIO accepts toll-free bridged NSMutableData/NSString;
        // image is borrowed from a retained bitmap for the duration of encoding.
        unsafe {
            let image_source = CGImageSourceCreateWithData(Retained::as_ptr(&input).cast(), std::ptr::null());
            assert!(!image_source.is_null());
            let image = CGImageSourceCreateImageAtIndex(image_source, 0, std::ptr::null());
            CFRelease(image_source);
            assert!(!image.is_null());
            let destination = CGImageDestinationCreateWithData(
                Retained::as_ptr(&output).cast_mut().cast(),
                Retained::as_ptr(&ty).cast(),
                1,
                std::ptr::null(),
            );
            assert!(!destination.is_null(), "OS HEIC encoder unavailable");
            CGImageDestinationAddImage(destination, image, std::ptr::null());
            CFRelease(image);
            let success = CGImageDestinationFinalize(destination);
            CFRelease(destination);
            assert!(success, "OS HEIC encoder failed");
        }
        let converted = MacTranslator
            .standard(&representation("public.heic"), &output.to_vec())
            .unwrap();
        let ClipData::Png(bytes) = &converted[0] else { panic!() };
        let image = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(bytes)).unwrap();
        assert_eq!((image.pixelsWide(), image.pixelsHigh()), (32, 32));
    });
}
#[test]
fn url_candidates_prefer_unicode_and_keep_valid_siblings_after_conversion_failure() {
    use content::url::tests::{Projected, source, wide_bytes};
    autoreleasepool(|_| {
        for unicode in ["https://preferred.invalid/", "relative"] {
            let source = source(
                ClipPlatform::Windows,
                &[
                    ("UniformResourceLocator", b"https://fallback.invalid/\0".to_vec()),
                    ("UniformResourceLocatorW", wide_bytes(unicode)),
                    ("text/x-moz-url", wide_bytes("https://third.invalid/\nTitle")),
                ],
            );
            assert_eq!(source.inventory().wanted(ClipPlatform::Mac).count(), 3);
            let spec = NativeSpec {
                inventory: source.inventory(),
                contents: Arc::new(Projected(source, ClipPlatform::Mac)),
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            let expected = if unicode.starts_with("https:") {
                unicode
            } else {
                "https://fallback.invalid/"
            };
            for (_, item) in promise_items(&spec).unwrap() {
                assert_eq!(
                    item.stringForType(&NSString::from_str("public.url"))
                        .unwrap()
                        .to_string(),
                    expected
                );
            }
        }
    });
}
#[test]
fn html_rtf_and_opaque_same_os_conversion_pairs() {
    autoreleasepool(|_| {
        let html = "<b>猫😀é</b>";
        for bytes in [
            html.as_bytes().to_vec(),
            [vec![255, 254], html.encode_utf16().flat_map(u16::to_le_bytes).collect()].concat(),
            [vec![254, 255], html.encode_utf16().flat_map(u16::to_be_bytes).collect()].concat(),
        ] {
            assert_eq!(
                MacTranslator.standard(&representation("public.html"), &bytes).unwrap(),
                vec![ClipData::Html(html.into())]
            );
        }
        assert!(
            MacTranslator
                .standard(&representation("public.html"), b"<p>\xff</p>")
                .is_err()
        );
        let rtf = b"{\\rtf1\\ansi\\ansicpg1251 \\'e0\\bin3 a\0b}";
        assert_eq!(
            MacTranslator.standard(&representation("public.rtf"), rtf).unwrap(),
            vec![ClipData::Rtf(rtf.to_vec())]
        );
        let source = crate::macos::test_pasteboard::new();
        let item = NSPasteboardItem::new();
        for (name, bytes) in [
            ("public.utf8-plain-text", b"text".as_slice()),
            ("public.html", html.as_bytes()),
            ("public.rtf", rtf.as_slice()),
            ("com.apple.flat-rtfd", &[0, 255, 0, 9]),
            ("com.apple.pict", &[0, 255, 0, 10]),
            ("example.private", &[0, 255, 0, 11]),
        ] {
            assert!(item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(name)));
        }
        source.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)]));
        let dir = tempfile::tempdir().unwrap();
        let captured = capture(Some(&source.name().to_string()), dir.path())
            .unwrap()
            .content
            .unwrap();
        let spec = NativeSpec {
            inventory: captured.inventory(),
            contents: captured,
            files: vec![],
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        };
        for (_, copy) in promise_items(&spec).unwrap() {
            let original = source.pasteboardItems().unwrap().objectAtIndex(0);
            for ty in original.types() {
                assert_eq!(
                    copy.dataForType(&ty).unwrap().to_vec(),
                    original.dataForType(&ty).unwrap().to_vec()
                );
            }
        }
        source.clearContents();
    });
}
#[test]
fn compressed_image_dimensions_are_bounded_before_pixel_decoding() {
    let mut bytes = Vec::new();
    let mut enc = png::Encoder::new(&mut bytes, 8192, 8193);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    let tiny = png();
    let mut offset = 8;
    while offset < tiny.len() {
        let n = u32::from_be_bytes(tiny[offset..offset + 4].try_into().unwrap()) as usize;
        if &tiny[offset + 4..offset + 8] == b"IDAT" {
            writer
                .write_chunk(png::chunk::IDAT, &tiny[offset + 8..offset + 8 + n])
                .unwrap();
        }
        offset += n + 12;
    }
    writer.finish().unwrap();
    assert_eq!(
        content::reason(
            &MacTranslator
                .standard(&representation("public.png"), &bytes)
                .unwrap_err()
        ),
        Unavailable::LimitExceeded
    );
}
#[test]
fn same_os_native_aliases_publish_independently_with_source_values() {
    autoreleasepool(|_| {
        let source = crate::macos::test_pasteboard::new();
        let item = NSPasteboardItem::new();
        for name in [
            "public.utf8-plain-text",
            "NSStringPboardType",
            "public.html",
            "NSHTMLPboardType",
            "public.rtf",
            "NSRTFPboardType",
        ] {
            item.setData_forType(&NSData::with_bytes(b"{\\rtf1 text}"), &NSString::from_str(name));
        }
        assert!(source.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
        let dir = tempfile::tempdir().unwrap();
        let captured = capture(Some(&source.name().to_string()), dir.path())
            .unwrap()
            .content
            .unwrap();
        let spec = NativeSpec {
            inventory: captured.inventory(),
            contents: captured,
            files: vec![],
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        };
        for (_, copy) in promise_items(&spec).unwrap() {
            let original = source.pasteboardItems().unwrap().objectAtIndex(0);
            for ty in original.types() {
                assert_eq!(
                    copy.dataForType(&ty).unwrap().to_vec(),
                    original.dataForType(&ty).unwrap().to_vec()
                );
            }
        }
        source.clearContents();
    });
}

#[test]
fn approved_tiff_preserves_pixel_color_and_alpha() {
    autoreleasepool(|_| {
        let original = png();
        let tiff = png_to_tiff(&original).unwrap();
        let decode = |bytes: &[u8]| {
            let rep = objc2_app_kit::NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(bytes)).unwrap();
            let mut pixel = [0usize; 4];
            // SAFETY: the fixture has four samples per pixel; the output is large enough.
            unsafe { rep.getPixel_atX_y(std::ptr::NonNull::new(pixel.as_mut_ptr()).unwrap(), 0, 0) };
            (rep.pixelsWide(), rep.pixelsHigh(), pixel)
        };
        assert_eq!(decode(&original), decode(&tiff));
        assert!(png_to_tiff(b"not an image").is_err());
    });
}
