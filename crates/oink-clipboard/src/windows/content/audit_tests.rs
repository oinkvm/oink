use super::*;
#[test]
fn tiff_only_data_object_projects_pixels_to_mac_and_retains_windows_bytes() {
    super::tests::sta(|| {
        struct NoFiles;
        impl crate::FileContents for NoFiles {
            fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                unreachable!()
            }
        }
        let pixels = [255, 0, 30, 255, 0, 210, 40, 255, 20, 30, 255, 255, 80, 140, 210, 128];
        let mut tiff = io::Cursor::new(Vec::new());
        image::codecs::tiff::TiffEncoder::new(&mut tiff)
            .encode(&pixels, 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let tiff = tiff.into_inner();
        let formats = super::super::formats();
        let (object, _) = super::super::vfile::build(
            super::super::vfile::Spec {
                cloud: None,
                descriptors: super::super::files::descriptors(&[]),
                fixed: vec![(6, tiff.clone())],
                contents: Arc::new(NoFiles),
                cf_descriptor: formats.file_descriptor,
                cf_contents: formats.file_contents,
            },
            false,
        );
        let id = crate::content::tests::fixture().inventory().id;
        let captured = inventory(&object, id).unwrap();
        let wanted: Vec<_> = captured.wanted(ClipPlatform::Mac).collect();
        assert_eq!(wanted.len(), 1, "a CF_TIFF-only drag must publish a Mac image item");
        let rep = wanted[0];
        assert_eq!(rep.format, NativeFormat::WindowsPredefined(6));
        assert_eq!(rep.standard, Some(StandardFormat::Png));
        let dir = tempfile::tempdir().unwrap();
        let source = snapshot(&object, dir.path(), id).unwrap();
        let projected =
            content::read_all(&mut content::open_for(source.as_ref(), rep.id, ClipPlatform::Mac).unwrap()).unwrap();
        let decoded = content::decode_standard(rep, &projected).unwrap();
        let [ClipData::Png(png)] = decoded.as_slice() else {
            panic!("TIFF must project to an image")
        };
        let actual = super::super::image::decode_png(png).unwrap();
        assert_eq!((actual.width, actual.height), (2, 2));
        assert_eq!(actual.data, pixels);
        assert_eq!(
            content::read_all(&mut content::open_for(source.as_ref(), rep.id, ClipPlatform::Windows).unwrap()).unwrap(),
            tiff
        );
        assert_eq!(
            content::reason(&WindowsTranslator.standard(rep, b"invalid TIFF").unwrap_err()),
            Unavailable::InvalidData
        );
    });
}

#[test]
fn all_registered_standard_names_match_case_and_png_mime_variants() {
    for (name, expected) in [
        ("html format", StandardFormat::Html),
        ("RICH TEXT FORMAT", StandardFormat::Rtf),
        ("png", StandardFormat::Png),
        ("image/png", StandardFormat::Png),
        ("Image/PNG", StandardFormat::Png),
    ] {
        assert_eq!(
            standard(&NativeFormat::WindowsRegistered(name.into())),
            Some(expected),
            "{name}"
        );
    }
}
#[test]
fn oem_text_uses_the_oem_code_page_and_unicode_rejects_malformed_units() {
    use windows::Win32::Globalization::*;
    let text = "é";
    let wide: Vec<_> = text.encode_utf16().collect();
    let mut bytes = vec![0; 32];
    // SAFETY: sized writable destination and valid UTF-16 input.
    let n = unsafe { WideCharToMultiByte(CP_OEMCP, WC_NO_BEST_FIT_CHARS, &wide, Some(&mut bytes), None, None) };
    bytes.truncate(n as usize);
    bytes.push(0);
    let mut rep = crate::content::tests::fixture().inventory().items[0].representations[1].clone();
    rep.format = NativeFormat::WindowsPredefined(7);
    assert_eq!(
        WindowsTranslator.standard(&rep, &bytes).unwrap(),
        vec![ClipData::Text(text.into())]
    );
    rep.format = NativeFormat::WindowsPredefined(13);
    for bad in [vec![65], vec![0, 216, 0, 0]] {
        assert_eq!(
            content::reason(&WindowsTranslator.standard(&rep, &bad).unwrap_err()),
            Unavailable::InvalidData
        );
    }
}
#[test]
fn foreign_target_retains_later_candidates_for_every_standard_binding() {
    use crate::content::tests::{Portable, fixture};
    super::tests::sta(|| {
        for kind in [
            StandardFormat::Text,
            StandardFormat::Html,
            StandardFormat::Rtf,
            StandardFormat::Png,
        ] {
            let mut inventory = fixture().inventory();
            let png = super::super::image::encode_png(&super::super::image::Rgba {
                pixels_per_meter: None,
                width: 1,
                height: 1,
                data: vec![250, 20, 10, 128],
            })
            .unwrap();
            for (i, rep) in inventory.items[0].representations.iter_mut().enumerate() {
                rep.format = NativeFormat::Mac(format!("test.{i}"));
                rep.standard = Some(kind);
                rep.data = if i == 0 {
                    RepresentationData::Reader {
                        size: ContentSize::Unknown,
                    }
                } else {
                    RepresentationData::Inline(match kind {
                        StandardFormat::Png => png.clone(),
                        StandardFormat::Rtf => b"{\\rtf1\\ansi\\ansicpg1252 caf\\'e9}".to_vec(),
                        _ => b"valid".to_vec(),
                    })
                };
            }
            let target = target::build(NativeSpec {
                contents: Arc::new(Portable(inventory.clone())),
                inventory,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
            let formats = target.formats();
            assert_eq!(
                formats.len(),
                match kind {
                    StandardFormat::Png => 3,
                    StandardFormat::Text => 4,
                    _ => 1,
                }
            );
            for f in formats {
                let mut medium = target
                    .get(&f, || diagnostics::Publication::new().observe(Some(false)))
                    .expect("later candidate was hidden");
                assert!(!medium::serialize(&medium, Medium::HGlobal).unwrap().is_empty());
                // SAFETY: this GetData call transferred ownership of one medium.
                unsafe { ReleaseStgMedium(&mut medium) };
            }
        }
    });
}
#[test]
fn rtf_preserves_declared_code_page_and_binary_nul_bytes() {
    let mut rep = crate::content::tests::fixture().inventory().items[0].representations[1].clone();
    rep.format = NativeFormat::WindowsRegistered("Rich Text Format".into());
    rep.standard = Some(StandardFormat::Rtf);
    let rtf = b"{\\rtf1\\ansi\\ansicpg1251 \\'e0\\bin3 a\0b}";
    assert_eq!(
        WindowsTranslator
            .standard(&rep, &[rtf.as_slice(), &[0, 0]].concat())
            .unwrap(),
        vec![ClipData::Rtf(rtf.to_vec())]
    );
}
#[test]
fn text_conversion_uses_cf_locale_instead_of_receiver_system_locale() {
    let mut inventory = crate::content::tests::fixture().inventory();
    inventory.platform = ClipPlatform::Windows;
    for (rep, cf, bytes) in inventory.items[0]
        .representations
        .iter_mut()
        .zip([1, 16])
        .zip([vec![0xe0, 0], 0x0419u32.to_le_bytes().to_vec()])
        .map(|((rep, cf), bytes)| (rep, cf, bytes))
    {
        rep.format = NativeFormat::WindowsPredefined(cf);
        rep.standard = if cf == 1 { Some(StandardFormat::Text) } else { None };
        rep.medium = Medium::HGlobal;
        rep.data = RepresentationData::Inline(bytes);
    }
    let source = content::clipboard::Clipboard::new(
        inventory,
        Box::new(|_| unreachable!()),
        Arc::new(|| Ok(())),
        Arc::new(WindowsTranslator),
        None,
    )
    .unwrap();
    assert_eq!(
        content::read_all(&mut content::open_for(source.as_ref(), RepresentationId(2), ClipPlatform::Mac).unwrap())
            .unwrap(),
        "а".as_bytes()
    );
}
#[test]
fn file_protocol_names_are_case_insensitive_and_never_opaque_readers() {
    for name in ["filegroupdescriptorw", "FILEGROUPDESCRIPTOR", "filecontents"] {
        assert!(is_file(&NativeFormat::WindowsRegistered(name.into())), "{name}");
    }
}
#[test]
fn native_conversion_matrix_and_same_os_publication_are_independent() {
    super::tests::sta(|| {
        let image = super::super::image::Rgba {
            pixels_per_meter: Some((11811, 11811)),
            width: 1,
            height: 1,
            data: vec![250, 20, 10, 128],
        };
        let png = super::super::image::encode_png(&image).unwrap();
        let mut cases = vec![
            (
                NativeFormat::WindowsPredefined(13),
                content::url::tests::wide_bytes("猫😀\r\né"),
            ),
            (NativeFormat::WindowsPredefined(1), b"plain\r\ntext\0".to_vec()),
            (NativeFormat::WindowsPredefined(7), b"OEM\r\ntext\0".to_vec()),
            (
                NativeFormat::WindowsRegistered("HTML Format".into()),
                super::super::html::encode_cf_html("<b>猫é</b>"),
            ),
            (
                NativeFormat::WindowsRegistered("Rich Text Format".into()),
                b"{\\rtf1\\ansi\\ansicpg1252 caf\\'e9}\0".to_vec(),
            ),
            (
                NativeFormat::WindowsPredefined(8),
                super::super::image::rgba_to_dib24(&image),
            ),
            (
                NativeFormat::WindowsPredefined(17),
                super::super::image::rgba_to_dibv5(&image),
            ),
        ];
        for name in ["PNG", "image/png", "IMAGE/PNG; charset=binary", "image/x-png"] {
            cases.push((NativeFormat::WindowsRegistered(name.into()), png.clone()));
        }
        for name in ["UniformResourceLocatorW", "text/x-moz-url"] {
            cases.push((
                NativeFormat::WindowsRegistered(name.into()),
                content::url::tests::wide_bytes(if name == "text/x-moz-url" {
                    "https://example.invalid/\nTitle"
                } else {
                    "https://example.invalid/"
                }),
            ));
        }
        cases.push((
            NativeFormat::WindowsRegistered("UniformResourceLocator".into()),
            b"https://example.invalid/\0".to_vec(),
        ));
        cases.push((
            NativeFormat::WindowsRegistered("example.private".into()),
            vec![0, 255, 0, 128],
        ));
        for (format, bytes) in cases {
            let mut inventory = crate::content::tests::fixture().inventory();
            inventory.platform = ClipPlatform::Windows;
            inventory.items[0].representations.truncate(1);
            let rep = &mut inventory.items[0].representations[0];
            rep.format = format.clone();
            rep.standard = standard(&format);
            rep.medium = Medium::HGlobal;
            rep.data = RepresentationData::Inline(bytes.clone());
            let rep = rep.clone();
            let native_bytes = bytes.clone();
            let source = content::clipboard::Clipboard::new(
                inventory.clone(),
                Box::new(move |_| Ok(Box::new(io::Cursor::new(native_bytes.clone())))),
                Arc::new(|| Ok(())),
                Arc::new(WindowsTranslator),
                None,
            )
            .unwrap();
            if rep.standard.is_some() {
                let projected =
                    content::read_all(&mut content::open_for(source.as_ref(), rep.id, ClipPlatform::Mac).unwrap())
                        .unwrap();
                if rep.standard == Some(StandardFormat::Url) {
                    content::url::Link::decode(&projected).unwrap();
                } else {
                    content::decode_standard(&rep, &projected).unwrap();
                }
            } else {
                assert!(content::open_for(source.as_ref(), rep.id, ClipPlatform::Mac).is_err());
            }
            let native = target::build(NativeSpec {
                inventory,
                contents: source,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
            let mut medium = native
                .get(&native.formats()[0], || {
                    diagnostics::Publication::new().observe(Some(false))
                })
                .unwrap();
            assert_eq!(
                medium::serialize(&medium, Medium::HGlobal).unwrap(),
                bytes,
                "{format:?}"
            );
            // SAFETY: target.GetData transferred exactly one native medium.
            unsafe { ReleaseStgMedium(&mut medium) };
        }
    });
}
#[test]
fn malformed_ansi_text_is_unavailable_in_its_declared_locale() {
    let mut inventory = crate::content::tests::fixture().inventory();
    inventory.platform = ClipPlatform::Windows;
    for (rep, cf, bytes) in inventory.items[0]
        .representations
        .iter_mut()
        .zip([1, 16])
        .zip([vec![0x81, 0], 0x0804u32.to_le_bytes().to_vec()])
        .map(|((rep, cf), bytes)| (rep, cf, bytes))
    {
        rep.format = NativeFormat::WindowsPredefined(cf);
        rep.standard = if cf == 1 { Some(StandardFormat::Text) } else { None };
        rep.medium = Medium::HGlobal;
        rep.data = RepresentationData::Inline(bytes);
    }
    let source = content::clipboard::Clipboard::new(
        inventory,
        Box::new(|_| unreachable!()),
        Arc::new(|| Ok(())),
        Arc::new(WindowsTranslator),
        None,
    )
    .unwrap();
    let error = content::open_for(source.as_ref(), RepresentationId(2), ClipPlatform::Mac)
        .err()
        .expect("incomplete code-page lead byte was accepted");
    assert_eq!(content::reason(&error), Unavailable::InvalidData);
}
