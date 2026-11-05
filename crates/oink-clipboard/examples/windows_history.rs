//! Desktop diagnostic: publish a synthetic Mac PNG through the real OLE backend.
//! Run only with sharing paused so the fixture cannot replace another device's
//! clipboard. Read history with HistoryProbe.cs from the investigation evidence.
#[cfg(not(windows))]
fn main() {}
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1);
    anyhow::ensure!(
        matches!(mode.as_deref(), Some("publish" | "concealed")),
        "pass publish only in an isolated desktop test with clipboard sharing paused"
    );
    publisher::run(mode.as_deref() == Some("concealed"))
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
        png: Vec<u8>,
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
            Ok(Box::new(io::Cursor::new(if id == RepresentationId(2) {
                vec![]
            } else {
                self.png.clone()
            })))
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
    pub fn run(concealed: bool) -> anyhow::Result<()> {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 80, 64);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            let pixels: Vec<u8> = (0..64)
                .flat_map(|y| (0..80).flat_map(move |x| [140, (y * 3) as u8, (x * 3) as u8, 255]))
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
        let backend = platform_backend()?;
        backend.write_native(NativeSpec {
            inventory: inventory.clone(),
            contents: Arc::new(Source { inventory, png }),
            files: vec![],
            file_contents: None,
            sensitivity: if concealed {
                Sensitivity::Concealed
            } else {
                Sensitivity::Normal
            },
        })?;
        println!("published synthetic Mac PNG via product write_native");
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
