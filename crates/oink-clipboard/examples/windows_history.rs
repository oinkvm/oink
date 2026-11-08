//! Desktop diagnostic: publish a synthetic Mac PNG or native Windows bitmap through OLE.
//! Run only with sharing paused so the fixture cannot replace another device's
//! clipboard. Read history with HistoryProbe.cs from the investigation evidence.
#[cfg(not(windows))]
fn main() {}
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1);
    anyhow::ensure!(
        matches!(mode.as_deref(), Some("publish" | "concealed" | "windows")),
        "pass publish, concealed or windows in an isolated desktop test with clipboard sharing paused"
    );
    publisher::run(mode.as_deref() == Some("concealed"), mode.as_deref() == Some("windows"))
}

#[cfg(windows)]
mod publisher {
    use oink_clipboard::{content, model::*, *};
    use oink_foundation::{
        DeviceId,
        clipboard::ClipPlatform,
        content::{ClipId, OfferId},
    };
    use std::{io, sync::Arc, time::Duration};
    struct Source {
        inventory: Inventory,
        bytes: Vec<Vec<u8>>,
    }
    struct Convert;
    impl content::Translator for Convert {
        fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
            content::decode_standard(rep, bytes)
        }
    }
    impl content::Provider for Source {
        fn inventory(&self) -> Inventory {
            self.inventory.clone()
        }
        fn translator(&self) -> Arc<dyn content::Translator> {
            Arc::new(Convert)
        }
        fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
            use windows::Win32::{
                System::DataExchange::GetOpenClipboardWindow, UI::WindowsAndMessaging::GetWindowThreadProcessId,
            };
            let mut pid = 0;
            // SAFETY: metadata queries only; the returned HWND is never modified.
            unsafe {
                if let Ok(window) = GetOpenClipboardWindow() {
                    GetWindowThreadProcessId(window, Some(&mut pid));
                }
            }
            println!("provider open representation={} requester_pid={pid}", id.0);
            Ok(Box::new(io::Cursor::new(
                self.bytes.get(id.0 as usize - 1).cloned().unwrap_or_default(),
            )))
        }
    }
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::DataExchange::{
            CloseClipboard, EnumClipboardFormats, GetClipboardFormatNameW, GetClipboardOwner, OpenClipboard,
        },
        UI::WindowsAndMessaging::{
            CallWindowProcW, GWLP_WNDPROC, GetWindowLongPtrW, SetWindowLongPtrW, WM_RENDERFORMAT, WNDPROC,
        },
    };
    static ORIGINAL: AtomicIsize = AtomicIsize::new(0);
    unsafe extern "system" fn trace(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if message == WM_RENDERFORMAT {
            println!("OLE WM_RENDERFORMAT format={}", wparam.0);
        }
        // SAFETY: the original procedure was saved before installing this callback
        // on our own OLE window and remains valid while the backend is alive.
        unsafe {
            CallWindowProcW(
                std::mem::transmute::<isize, WNDPROC>(ORIGINAL.load(Ordering::Acquire)),
                window,
                message,
                wparam,
                lparam,
            )
        }
    }
    pub fn run(concealed: bool, native: bool) -> anyhow::Result<()> {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 80, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            let pixels: Vec<u8> = (0..64)
                .flat_map(|y| {
                    (0..80).flat_map(move |x| {
                        if native {
                            [140, 140, 140, 255]
                        } else {
                            [140, (y * 3) as u8, (x * 3) as u8, 255]
                        }
                    })
                })
                .collect();
            writer.write_image_data(&pixels)?;
        }
        let mut inventory = Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([42; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            roots: vec![],
            items: vec![ContentItem {
                id: ItemId(0),
                virtual_files: false,
                file: None,
                representations: vec![Representation {
                    id: RepresentationId(1),
                    format: NativeFormat::Mac("public.png".into()),
                    medium: Medium::HGlobal,
                    aspect: 1,
                    index: -1,
                    standard: Some(StandardFormat::Png),
                    data: RepresentationData::Reader {
                        size: ContentSize::Known(png.len() as u64),
                    },
                }],
            }],
        };
        if concealed {
            inventory.items[0].representations.push(Representation {
                id: RepresentationId(2),
                format: NativeFormat::Mac("org.nspasteboard.ConcealedType".into()),
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard: Some(StandardFormat::Privacy),
                data: RepresentationData::Inline(vec![]),
            });
        }
        let bytes = if native {
            // Native Windows sources retain independently readable image formats.
            // V5 first exercises native Windows publishers affected by OLE history.
            let mut dib = vec![0u8; 40 + 80 * 64 * 3];
            dib[..4].copy_from_slice(&40u32.to_le_bytes());
            dib[4..8].copy_from_slice(&80u32.to_le_bytes());
            dib[8..12].copy_from_slice(&64u32.to_le_bytes());
            dib[12..14].copy_from_slice(&1u16.to_le_bytes());
            dib[14..16].copy_from_slice(&24u16.to_le_bytes());
            dib[40..].fill(140);
            let mut v5 = vec![0u8; 124 + 80 * 64 * 4];
            v5[..40].copy_from_slice(&dib[..40]);
            v5[..4].copy_from_slice(&124u32.to_le_bytes());
            v5[14..16].copy_from_slice(&32u16.to_le_bytes());
            v5[16..20].copy_from_slice(&3u32.to_le_bytes());
            for (offset, mask) in [
                (40, 0x00ff0000u32),
                (44, 0x0000ff00),
                (48, 0x000000ff),
                (52, 0xff000000),
                (56, 0x73524742),
            ] {
                v5[offset..offset + 4].copy_from_slice(&mask.to_le_bytes());
            }
            v5[108..112].copy_from_slice(&4u32.to_le_bytes());
            for pixel in v5[124..].as_chunks_mut::<4>().0 {
                pixel.copy_from_slice(&[140, 140, 140, 255]);
            }
            let bytes = vec![v5, dib, png];
            inventory.platform = ClipPlatform::Windows;
            inventory.items[0].representations = bytes
                .iter()
                .enumerate()
                .map(|(i, data)| Representation {
                    id: RepresentationId(i as u32 + 1),
                    format: match i {
                        0 => NativeFormat::WindowsPredefined(17),
                        1 => NativeFormat::WindowsPredefined(8),
                        _ => NativeFormat::WindowsRegistered("PNG".into()),
                    },
                    medium: Medium::HGlobal,
                    aspect: 1,
                    index: -1,
                    standard: Some(StandardFormat::Png),
                    data: RepresentationData::Reader {
                        size: ContentSize::Known(data.len() as u64),
                    },
                })
                .collect();
            bytes
        } else {
            vec![png]
        };
        let backend = platform_backend()?;
        backend.write_native(NativeSpec {
            inventory: inventory.clone(),
            contents: Arc::new(Source { inventory, bytes }),
            files: vec![],
            file_contents: None,
            sensitivity: if concealed {
                Sensitivity::Concealed
            } else {
                Sensitivity::Normal
            },
        })?;
        println!("published synthetic image via product write_native");
        // SAFETY: our backend owns this window in this process. Only the diagnostic
        // subclass changes; it forwards every message and is removed before Drop.
        let owner = unsafe { GetClipboardOwner()? };
        let mut owner_pid = 0;
        // SAFETY: metadata query; verify ownership before subclassing.
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(owner, Some(&mut owner_pid));
        }
        anyhow::ensure!(owner_pid == std::process::id(), "clipboard replaced before observation");
        // SAFETY: ownership was checked above; enumerate only format metadata
        // under the clipboard lock, without requesting payloads.
        unsafe {
            ORIGINAL.store(GetWindowLongPtrW(owner, GWLP_WNDPROC), Ordering::Release);
            SetWindowLongPtrW(owner, GWLP_WNDPROC, trace as *const () as isize);
            if OpenClipboard(None).is_ok() {
                let mut cf = EnumClipboardFormats(0);
                while cf != 0 {
                    let mut name = [0; 256];
                    let n = GetClipboardFormatNameW(cf, &mut name);
                    println!(
                        "clipboard format={cf} name={}",
                        String::from_utf16_lossy(&name[..n.max(0) as usize])
                    );
                    cf = EnumClipboardFormats(cf);
                }
                CloseClipboard()?;
            }
        }
        std::thread::sleep(Duration::from_secs(8));
        // SAFETY: backend is still alive; restore its original procedure.
        unsafe {
            SetWindowLongPtrW(owner, GWLP_WNDPROC, ORIGINAL.load(Ordering::Acquire));
        }
        Ok(())
    }
}
