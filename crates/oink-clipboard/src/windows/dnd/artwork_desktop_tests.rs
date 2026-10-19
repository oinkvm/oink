use super::*;
use std::sync::atomic::AtomicBool;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Ole::RevokeDragDrop;
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper};
use windows::Win32::UI::WindowsAndMessaging::*;

static CAPTURE_ORIGIN: OnceLock<(i32, i32)> = OnceLock::new();
fn capture_origin() -> (i32, i32) {
    *CAPTURE_ORIGIN.get_or_init(|| (300, 250))
}
thread_local! { static MANUAL_DATA: std::cell::RefCell<Option<IDataObject>> = const { std::cell::RefCell::new(None) }; }
thread_local! { static MANUAL_REFRESH: std::cell::RefCell<Option<IDropTargetHelper>> = const { std::cell::RefCell::new(None) }; }
unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_APP + 3 {
        MANUAL_REFRESH.with(|helper| {
            MANUAL_DATA.with(|data| {
                if let (Some(helper), Some(data)) = (helper.borrow().as_ref(), data.borrow().as_ref()) {
                    // SAFETY: private destination reenters with the same local proxy and fixed visual point.
                    unsafe {
                        helper.DragLeave().unwrap();
                        helper
                            .DragEnter(
                                window,
                                data,
                                &POINT {
                                    x: capture_origin().0 + 200,
                                    y: capture_origin().1 + 150,
                                },
                                DROPEFFECT_COPY,
                            )
                            .unwrap();
                    }
                }
            })
        });
        return LRESULT(0);
    }
    if message == WM_APP + 2 {
        let began = Instant::now();
        MANUAL_DATA.with(|data| {
            if let Some(data) = data.borrow().as_ref() {
                for cf in [
                    13,
                    formats().html as u16,
                    formats().png as u16,
                    super::super::content::format_id(&oink_foundation::content::NativeFormat::WindowsRegistered(
                        "UniformResourceLocatorW".into(),
                    ))
                    .unwrap(),
                ] {
                    // SAFETY: private IDataObject proxy and independently owned medium on this target STA.
                    unsafe {
                        let mut medium = data
                            .GetData(&windows::Win32::System::Com::FORMATETC {
                                cfFormat: cf,
                                dwAspect: 1,
                                lindex: -1,
                                tymed: 1,
                                ..Default::default()
                            })
                            .unwrap();
                        windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
                    }
                }
            }
        });
        eprintln!("chrome-like hover_us={}", began.elapsed().as_micros());
        MANUAL_REFRESH.with(|helper| {
            if let Some(helper) = helper.borrow().as_ref() {
                // SAFETY: retained helper and physical point belong to this destination STA.
                unsafe {
                    let point = POINT {
                        x: capture_origin().0 + 200,
                        y: capture_origin().1 + 150,
                    };
                    helper.DragOver(&point, DROPEFFECT_COPY).unwrap();
                }
            }
        });
        return LRESULT(0);
    }
    if message == WM_CLOSE || message == WM_TIMER {
        // SAFETY: quits only this private destination thread.
        unsafe {
            PostQuitMessage(0);
        }
        return LRESULT(0);
    }
    // SAFETY: test window delegates standard message handling.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
#[implement(IDropTarget, Agile = false)]
struct Destination {
    window: isize,
    helper: IDropTargetHelper,
    move_image: bool,
    data: std::cell::RefCell<Option<IDataObject>>,
}
impl IDropTarget_Impl for Destination_Impl {
    fn DragEnter(
        &self,
        data: Ref<'_, IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // SAFETY: OLE supplies valid output storage and STA interfaces.
        unsafe {
            eprintln!("destination DragEnter at {},{}", point.x, point.y);
            *self.data.borrow_mut() = Some(data.ok()?.clone());
            *effect = DROPEFFECT_COPY;
            self.helper.DragEnter(
                HWND(self.window as _),
                data.ok()?,
                &POINT { x: point.x, y: point.y },
                *effect,
            )
        }
    }
    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, point: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        // SAFETY: OLE supplies valid output storage.
        unsafe {
            *effect = DROPEFFECT_COPY;
            if let Some(data) = self
                .data
                .borrow()
                .as_ref()
                .filter(|_| std::env::var_os("OINK_DND_NO_HOVER_READS").is_none())
            {
                // Real HTML/text destinations read their preferred formats while hovering.
                for cf in [13, formats().html as u16] {
                    let mut medium = data.GetData(&windows::Win32::System::Com::FORMATETC {
                        cfFormat: cf,
                        dwAspect: 1,
                        lindex: -1,
                        tymed: 1,
                        ..Default::default()
                    })?;
                    windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
                }
            }
            if self.move_image {
                self.helper.DragOver(&POINT { x: point.x, y: point.y }, *effect)
            } else {
                Ok(())
            }
        }
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        // SAFETY: the helper belongs to this OLE destination STA.
        unsafe { self.helper.DragLeave() }
    }
    fn Drop(
        &self,
        _: Ref<'_, IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // SAFETY: OLE provides the effect output and this STA owns the helper.
        unsafe {
            *effect = DROPEFFECT_NONE;
            self.helper.DragLeave()
        }
    }
}
#[implement(IDropSource, Agile = false)]
struct Source {
    stop: Arc<AtomicBool>,
    deadline: Instant,
}
impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, _: BOOL, _: MODIFIERKEYS_FLAGS) -> HRESULT {
        if self.stop.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            DRAGDROP_S_CANCEL
        } else {
            S_OK
        }
    }
    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        drag_feedback(effect)
    }
}
// Capture only our private, white destination and its drag image.
unsafe fn snapshot() -> super::super::image::Rgba {
    use windows::Win32::Graphics::Gdi::*;
    let width = 400;
    let height = 300;
    // SAFETY: acquire the desktop DC; released below on this same thread.
    let screen = unsafe { GetDC(None) };
    // SAFETY: screen is valid and the returned memory DC is released below.
    let dc = unsafe { CreateCompatibleDC(Some(screen)) };
    let info = BITMAPINFO {
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
    // SAFETY: fixed positive RGBA8 dimensions; bits receives the owned DIB storage.
    let bitmap = unsafe { CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0) }.unwrap();
    // SAFETY: this unselected bitmap is owned locally; restore the previous object below.
    let old = unsafe { SelectObject(dc, bitmap.into()) };
    // SAFETY: both DCs and the selected 400x300 destination DIB are valid.
    unsafe {
        BitBlt(
            dc,
            0,
            0,
            width,
            height,
            Some(screen),
            capture_origin().0,
            capture_origin().1,
            SRCCOPY | CAPTUREBLT,
        )
    }
    .unwrap();
    // SAFETY: the selected 32-bit DIB owns exactly width*height*4 bytes.
    let mut bytes = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize) }.to_vec();
    for p in bytes.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
        p[3] = 255;
    }
    // SAFETY: restore the old object, then dispose the unselected bitmap and both DCs.
    unsafe {
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
    }
    super::super::image::Rgba {
        width: width as u32,
        height: height as u32,
        data: bytes,
        pixels_per_meter: None,
    }
}
fn red_bounds(pixels: &super::super::image::Rgba) -> Option<(u32, u32)> {
    let (mut left, mut top, mut right, mut bottom) = (pixels.width, pixels.height, 0, 0);
    for y in 0..pixels.height {
        for x in 0..pixels.width {
            let i = ((y * pixels.width + x) * 4) as usize;
            let p = &pixels.data[i..i + 4];
            if p[0] > 180 && p[1] < 100 && p[2] < 100 {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x);
                bottom = bottom.max(y);
            }
        }
    }
    (left <= right && top <= bottom).then(|| (right - left + 1, bottom - top + 1))
}
enum Marshaled {
    Process(Vec<u8>),
}
fn destination_thread(stream: Option<Marshaled>) -> (isize, std::thread::JoinHandle<()>) {
    let (ready, started) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        // SAFETY: isolated OLE apartment owns its private window/helper/message pump.
        unsafe {
            windows::Win32::System::Ole::OleInitialize(None).unwrap();
            windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
            let class = w!("OinkThumbnailDesktopTest");
            let instance = GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                hInstance: instance.into(),
                lpfnWndProc: Some(window_proc),
                lpszClassName: class,
                hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(6 as _),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let window = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!("Thumbnail desktop check"),
                WS_POPUP | WS_VISIBLE,
                capture_origin().0,
                capture_origin().1,
                400,
                300,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let refresh: IDropTargetHelper =
                CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).unwrap();
            if let Some(stream) = stream {
                let data: IDataObject = match stream {
                    Marshaled::Process(bytes) => {
                        use windows::Win32::System::Com::*;
                        let stream = windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal(
                            windows::Win32::Foundation::HGLOBAL::default(),
                            true,
                        )
                        .unwrap();
                        stream
                            .Write(bytes.as_ptr().cast(), bytes.len() as u32, None)
                            .ok()
                            .unwrap();
                        stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
                        windows::Win32::System::Com::Marshal::CoUnmarshalInterface(&stream).unwrap()
                    }
                };
                let point = POINT {
                    x: capture_origin().0 + 200,
                    y: capture_origin().1 + 150,
                };
                refresh.DragEnter(window, &data, &point, DROPEFFECT_COPY).unwrap();
                MANUAL_DATA.with(|slot| *slot.borrow_mut() = Some(data));
            }
            MANUAL_REFRESH.with(|slot| *slot.borrow_mut() = Some(refresh.clone()));
            let destination: IDropTarget = Destination {
                window: window.0 as isize,
                helper: refresh,
                data: std::cell::RefCell::new(None),
                move_image: std::env::var_os("OINK_DND_PASSIVE_HELPER").is_none(),
            }
            .into();
            RegisterDragDrop(window, &destination).unwrap();
            let _ = SetTimer(Some(window), 1, 8000, None);
            ready.send(window.0 as isize).unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            MANUAL_REFRESH.with(|slot| {
                if let Some(helper) = slot.borrow_mut().take() {
                    let _ = helper.DragLeave();
                }
            });
            MANUAL_DATA.with(|slot| slot.borrow_mut().take());
            RevokeDragDrop(window).unwrap();
            drop(destination);
            DestroyWindow(window).unwrap();
            windows::Win32::System::Ole::OleUninitialize();
        }
    });
    (started.recv_timeout(WAIT).unwrap(), thread)
}
fn desktop_backend() -> Arc<WinDnd> {
    let backend = Arc::new(WinDnd {
        shared: Arc::new(Shared {
            native_jobs: OnceLock::new(),
            sink: OnceLock::new(),
            inject: OnceLock::new(),
            source_press: OnceLock::new(),
            source_start: OnceLock::new(),
            st: Mutex::new(St::default()),
            cv: Condvar::new(),
        }),
        thread: OnceLock::new(),
    });
    backend.start(Arc::new(|_| {}), Arc::new(|_| Ok(()))).unwrap();
    backend
}
fn published_window(backend: &WinDnd) -> usize {
    backend
        .run(|| {
            let data = LOCAL.with(|l| l.borrow().as_ref().unwrap().live_drag.as_ref().unwrap().data.clone());
            super::super::drag_image::drag_window(&data).unwrap().0 as usize
        })
        .unwrap()
}
fn visible_shell_images(target_process: u32) -> u32 {
    struct Count {
        source: u32,
        target: u32,
        images: u32,
    }
    unsafe extern "system" fn visit(window: HWND, context: LPARAM) -> BOOL {
        // SAFETY: synchronous EnumWindows callback receives a live Count and a native window.
        unsafe {
            let count = &mut *(context.0 as *mut Count);
            let mut process = 0;
            GetWindowThreadProcessId(window, Some(&mut process));
            if (process == count.source || process == count.target) && IsWindowVisible(window).as_bool() {
                let mut class = [0u16; 64];
                let n = GetClassNameW(window, &mut class);
                if class[..n as usize] == "SysDragImage".encode_utf16().collect::<Vec<_>>() {
                    count.images += 1;
                }
            }
        }
        BOOL(1)
    }
    // SAFETY: read-only IDs and enumeration of this test's two processes, with stack context retained synchronously.
    unsafe {
        let mut count = Count {
            source: windows::Win32::System::Threading::GetCurrentProcessId(),
            target: target_process,
            images: 0,
        };
        EnumWindows(Some(visit), LPARAM((&mut count as *mut Count) as isize)).unwrap();
        count.images
    }
}
#[test]
#[ignore = "subprocess endpoint for the bounded Shell image regression"]
fn shell_destination_process() {
    let Some(path) = std::env::var_os("OINK_DND_SHELL_CHILD") else {
        return;
    };
    let origin = std::env::var("OINK_DND_TEST_ORIGIN").unwrap();
    let (x, y) = origin.split_once(',').unwrap();
    let _ = CAPTURE_ORIGIN.set((x.parse().unwrap(), y.parse().unwrap()));
    let path = std::path::PathBuf::from(path);
    let (window, thread) = destination_thread(Some(Marshaled::Process(std::fs::read(&path).unwrap())));
    std::fs::write(path.with_extension("hwnd"), window.to_string()).unwrap();
    thread.join().unwrap();
}
#[test]
#[ignore = "bounded desktop check: two OLE processes, private window, no injected input"]
fn cross_process_thumbnail_replaces_old_shell_visual() {
    use windows::core::Interface;
    // SAFETY: private sampling thread uses physical desktop coordinates.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut point = POINT::default();
    // SAFETY: read-only pointer query; this test injects no input.
    unsafe {
        GetPhysicalCursorPos(&mut point).unwrap();
    }
    let _ = CAPTURE_ORIGIN.set((point.x - 200, point.y - 150));
    let backend = desktop_backend();
    let operation = DragOperation::new();
    let (stream, opens) = backend
        .run(move || {
            let (native, opens) = super::super::content::target::tests::chrome_native(Duration::from_millis(15));
            let spec = DragSpec {
                operation,
                at: Point::new(0., 0.),
                items: vec![ClipData::Text("default".into())],
                files: None,
                native: Some(native),
                ops: DragOps::COPY,
            };
            let (data, _) = data_object(&spec).unwrap();
            let helper = super::super::drag_image::initialize(&data, &spec.items, None).unwrap();
            // SAFETY: marshal this private IDataObject for a single local-process consumer.
            let bytes = unsafe {
                use windows::Win32::System::Com::*;
                let stream =
                    StructuredStorage::CreateStreamOnHGlobal(windows::Win32::Foundation::HGLOBAL::default(), true)
                        .unwrap();
                Marshal::CoMarshalInterface(
                    &stream,
                    &IDataObject::IID,
                    &data,
                    MSHCTX_LOCAL.0 as u32,
                    None,
                    MSHLFLAGS_NORMAL.0 as u32,
                )
                .unwrap();
                let mut size = STATSTG::default();
                stream.Stat(&mut size, STATFLAG_NONAME).unwrap();
                let mut bytes = vec![0; size.cbSize as usize];
                stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
                stream
                    .Read(bytes.as_mut_ptr().cast(), bytes.len() as u32, None)
                    .ok()
                    .unwrap();
                bytes
            };
            LOCAL.with(|l| l.borrow_mut().as_mut().unwrap().live_drag = Some(LiveDrag::new(operation, data, helper)));
            (bytes, opens)
        })
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("data-object");
    std::fs::write(&path, stream).unwrap();
    let mut target = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "windows::dnd::artwork_desktop_tests::shell_destination_process",
            "--nocapture",
        ])
        .env("OINK_DND_SHELL_CHILD", &path)
        .env(
            "OINK_DND_TEST_ORIGIN",
            format!("{},{}", capture_origin().0, capture_origin().1),
        )
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let window = loop {
        if let Ok(window) = std::fs::read_to_string(path.with_extension("hwnd")) {
            break window.parse::<isize>().unwrap();
        }
        assert!(Instant::now() < deadline, "child must establish its native helper");
        std::thread::sleep(Duration::from_millis(20));
    };
    backend
        .run(|| {
            feedback_at(
                POINT {
                    x: capture_origin().0 + 200,
                    y: capture_origin().1 + 150,
                },
                DROPEFFECT_COPY,
            );
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: temporary GDI snapshot owns and releases its DCs and bitmap.
    let before = unsafe { snapshot() };
    let pixels = super::super::image::Rgba {
        width: 256,
        height: 192,
        data: [255, 0, 0, 255].repeat(256 * 192),
        pixels_per_meter: None,
    };
    backend
        .run(move || {
            replace_live_image(
                operation,
                oink_foundation::dnd::DragImage {
                    png: super::super::image::encode_png(&pixels).unwrap().into(),
                    width: 256.,
                    height: 192.,
                    hotspot_x: 256.,
                    hotspot_y: 96.,
                },
                pixels,
                POINT {
                    x: capture_origin().0 + 200,
                    y: capture_origin().1 + 150,
                },
            )
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let original_window = published_window(&backend);
    backend
        .run(move || {
            let pixels = super::super::image::Rgba {
                width: 256,
                height: 192,
                data: [0, 0, 255, 255].repeat(256 * 192),
                pixels_per_meter: None,
            };
            let image = oink_foundation::dnd::DragImage {
                png: super::super::image::encode_png(&pixels).unwrap().into(),
                width: 256.,
                height: 192.,
                hotspot_x: 256.,
                hotspot_y: 96.,
            };
            replace_live_image(
                operation,
                image,
                pixels,
                POINT {
                    x: capture_origin().0 + 200,
                    y: capture_origin().1 + 150,
                },
            );
        })
        .unwrap();
    assert_eq!(
        published_window(&backend),
        original_window,
        "only one thumbnail swap is allowed"
    );

    for _ in 0..3 {
        // SAFETY: private destination refreshes its original helper, as real hover feedback does.
        unsafe {
            PostMessageW(Some(HWND(window as _)), WM_APP + 2, WPARAM(0), LPARAM(0)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
        backend
            .run(|| {
                feedback_at(
                    POINT {
                        x: capture_origin().0 + 200,
                        y: capture_origin().1 + 150,
                    },
                    DROPEFFECT_COPY,
                );
            })
            .unwrap();
    }
    assert_eq!(
        published_window(&backend),
        original_window,
        "feedback must not recreate image windows"
    );
    // SAFETY: temporary GDI snapshot owns and releases its DCs and bitmap.
    let after = unsafe { snapshot() };
    assert_eq!(
        visible_shell_images(target.id()),
        1,
        "only the replacement may remain visible"
    );
    // SAFETY: simulate a fresh destination entry without moving or injecting the pointer.
    unsafe {
        PostMessageW(Some(HWND(window as _)), WM_APP + 3, WPARAM(0), LPARAM(0)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(
        visible_shell_images(target.id()),
        1,
        "a fresh native destination must take over the same thumbnail without another visual"
    );
    let reentered_window = published_window(&backend);
    for _ in 0..3 {
        // SAFETY: refresh this private native helper; no pointer input is injected.
        unsafe {
            PostMessageW(Some(HWND(window as _)), WM_APP + 2, WPARAM(0), LPARAM(0)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
        backend
            .run(|| {
                feedback_at(
                    POINT {
                        x: capture_origin().0 + 200,
                        y: capture_origin().1 + 150,
                    },
                    DROPEFFECT_COPY,
                )
            })
            .unwrap();
    }
    assert_eq!(
        published_window(&backend),
        reentered_window,
        "reentry must not cause per-feedback image recreation"
    );
    assert_eq!(visible_shell_images(target.id()), 1);

    // SAFETY: orderly shutdown of the test's destination/helper on its owning STA.
    unsafe {
        PostMessageW(Some(HWND(window as _)), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
    }
    assert!(target.wait().unwrap().success());
    eprintln!("chrome-like provider opens={}", opens.load(Ordering::Relaxed));
    assert_eq!(
        opens.load(Ordering::Relaxed),
        4,
        "hover callbacks must render each requested native format only once"
    );
    backend
        .run(|| {
            let live = LOCAL
                .with(|l| l.borrow_mut().as_mut().unwrap().live_drag.take())
                .unwrap();
            // SAFETY: balances the replacement manager's entry on its owning STA.
            unsafe {
                if let Some(helper) = live.refresh {
                    let _ = helper.DragLeave();
                }
            }
        })
        .unwrap();
    if let Some(path) = std::env::var_os("OINK_DND_ARTWORK_EVIDENCE") {
        let path = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&path).unwrap();
        for (name, pixels) in [("cross-default.png", &before), ("cross-thumbnail.png", &after)] {
            std::fs::write(path.join(name), super::super::image::encode_png(pixels).unwrap()).unwrap();
        }
    }
    let ink = |p: &super::super::image::Rgba| {
        p.data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] < 230 && p[1] < 230 && p[2] < 230)
            .count()
    };
    assert!(
        ink(&before) > 0,
        "default text must be shown by the other apartment's helper"
    );
    assert!(red_bounds(&after).is_some(), "late thumbnail must become visible");
    assert_eq!(
        ink(&after),
        0,
        "the other apartment's default image must disappear after replacement"
    );
}
#[test]
#[ignore = "bounded desktop check: moves pointer over a private test window, starts and cancels OLE"]
fn live_drag_replaces_default_image_at_128_logical_pixels() {
    // SAFETY: private sampling thread uses physical coordinates.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let backend = Arc::new(WinDnd {
        shared: Arc::new(Shared {
            native_jobs: OnceLock::new(),
            sink: OnceLock::new(),
            inject: OnceLock::new(),
            source_press: OnceLock::new(),
            source_start: OnceLock::new(),
            st: Mutex::new(St::default()),
            cv: Condvar::new(),
        }),
        thread: OnceLock::new(),
    });
    backend.start(Arc::new(|_| {}), Arc::new(|_| Ok(()))).unwrap();
    let operation = DragOperation::new();
    let stop = Arc::new(AtomicBool::new(false));
    let cancel = stop.clone();
    let (started, ready) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    // SAFETY: runs on the initialized OLE STA; all COM/windows are local and disposed here.
    let job = Box::new(move || unsafe {
        let class = w!("OinkThumbnailDesktopTest");
        let instance = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            hInstance: instance.into(),
            lpfnWndProc: Some(window_proc),
            lpszClassName: class,
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(6 as _),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("Thumbnail desktop check"),
            WS_POPUP | WS_VISIBLE,
            300,
            250,
            400,
            300,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();
        let refresh: IDropTargetHelper = CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).unwrap();
        let destination: IDropTarget = Destination {
            window: window.0 as isize,
            helper: refresh,
            data: std::cell::RefCell::new(None),
            move_image: std::env::var_os("OINK_DND_PASSIVE_HELPER").is_none(),
        }
        .into();
        RegisterDragDrop(window, &destination).unwrap();
        let mut previous = POINT::default();
        GetCursorPos(&mut previous).unwrap();
        SetCursorPos(500, 400).unwrap();
        let (native, opens) = super::super::content::target::tests::chrome_native(Duration::from_millis(15));
        let spec = DragSpec {
            operation,
            at: Point::new(500., 400.),
            items: vec![ClipData::Text("default".into())],
            files: None,
            native: Some(native),
            ops: DragOps::COPY,
        };
        let (data, _) = data_object(&spec).unwrap();
        let helper = super::super::drag_image::initialize(&data, &spec.items, None).unwrap();
        LOCAL.with(|l| {
            l.borrow_mut().as_mut().unwrap().live_drag = Some(LiveDrag::new(operation, data.clone(), helper))
        });
        let source: IDropSource = Source {
            stop: cancel,
            deadline: Instant::now() + Duration::from_secs(5),
        }
        .into();
        started.send(()).unwrap();
        let mut effect = DROPEFFECT_NONE;
        let result = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        if let Some(refresh) = LOCAL
            .with(|l| l.borrow_mut().as_mut().unwrap().live_drag.take())
            .filter(|l| l.feedback_entered)
            .and_then(|l| l.refresh)
        {
            let _ = refresh.DragLeave();
        }
        RevokeDragDrop(window).unwrap();
        DestroyWindow(window).unwrap();
        SetCursorPos(previous.x, previous.y).unwrap();
        eprintln!("chrome-like provider opens={}", opens.load(Ordering::Relaxed));
        finished.send(result.0).unwrap();
    });
    let thread = backend.thread().unwrap();
    thread.jobs.send(job).unwrap();
    // SAFETY: nonblocking wake of our own STA window.
    unsafe { PostMessageW(Some(HWND(thread.catcher as _)), WM_APP_JOB, WPARAM(0), LPARAM(0)) }.unwrap();
    ready.recv_timeout(WAIT).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: establish OLE's destination/feedback before sampling its default image.
    backend.run(|| unsafe { SetCursorPos(501, 400).unwrap() }).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: capture allocates and releases its private DC/bitmap on the STA.
    let before = backend.run(|| unsafe { snapshot() }).unwrap();
    let text_tracking = std::env::var_os("OINK_DND_TRACKING_EVIDENCE").map(|_| track_frames(false));
    let pixels = super::super::image::Rgba {
        width: 256,
        height: 192,
        data: [255, 0, 0, 255].repeat(256 * 192),
        pixels_per_meter: None,
    };
    let image = oink_foundation::dnd::DragImage {
        png: super::super::image::encode_png(&pixels).unwrap().into(),
        width: 256.,
        height: 192.,
        hotspot_x: 128.,
        hotspot_y: 96.,
    };
    let at = Instant::now();
    backend.update_target_image(operation, image);
    std::thread::sleep(Duration::from_millis(250));
    // SAFETY: capture allocates and releases its private DC/bitmap on the STA.
    let after = backend.run(|| unsafe { snapshot() }).unwrap();
    let image_tracking = text_tracking.as_ref().map(|_| track_frames(true));
    // SAFETY: read-only DPI/window queries on the drag STA.
    let dpi = backend
        .run(|| unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(WindowFromPoint(POINT { x: 500, y: 400 })) })
        .unwrap()
        .max(96);
    // SAFETY: bounded movement stays within the private destination window.
    backend.run(|| unsafe { SetCursorPos(520, 410).unwrap() }).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: capture owns its temporary DC/bitmap on the STA.
    let moved = backend.run(|| unsafe { snapshot() }).unwrap();
    stop.store(true, Ordering::Release);
    // SAFETY: nonblocking wake of our own STA window.
    unsafe { PostMessageW(Some(HWND(thread.catcher as _)), WM_MOUSEMOVE, WPARAM(0), LPARAM(0)) }.unwrap();
    done.recv_timeout(Duration::from_secs(6)).unwrap();
    if let Some(path) = std::env::var_os("OINK_DND_ARTWORK_EVIDENCE") {
        let path = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&path).unwrap();
        for (name, pixels) in [
            ("default.png", &before),
            ("thumbnail.png", &after),
            ("moved.png", &moved),
        ] {
            std::fs::write(path.join(name), super::super::image::encode_png(pixels).unwrap()).unwrap();
        }
    }
    eprintln!(
        "default_red={:?} thumbnail_red={:?} dpi={dpi} update_sample_us={}",
        red_bounds(&before),
        red_bounds(&after),
        at.elapsed().as_micros()
    );
    if let Some(path) = std::env::var_os("OINK_DND_TRACKING_EVIDENCE") {
        let path = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&path).unwrap();
        let rows = serde_json::json!({ "text": text_tracking, "image": image_tracking });
        std::fs::write(path.join("tracking.json"), serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
        let error = |value: &serde_json::Value| {
            let mut errors: Vec<_> = value["frames"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| Some((v["cursor_x"].as_i64()? - v["image_x"].as_i64()?).abs()))
                .collect();
            errors.sort();
            errors[errors.len() * 95 / 100]
        };
        let text = error(&rows["text"]);
        let image = error(&rows["image"]);
        eprintln!("tracking p95 text_px={text} image_px={image}");
        assert!(
            text <= image + 8,
            "initial text artwork must track like a late image even when the destination does not update the helper"
        );
    }
    let old_ink = after
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] < 230 && p[1] < 230 && p[2] < 230)
        .count();
    assert_eq!(old_ink, 0, "default text must disappear after the swap");
    assert_eq!(red_bounds(&before), None);
    assert_eq!(red_bounds(&after), Some((128 * dpi / 96, 96 * dpi / 96)));
    assert_eq!(
        red_bounds(&moved),
        red_bounds(&after),
        "replacement survives pointer motion and target feedback"
    );
    assert_ne!(after.data, moved.data, "the image must follow the cursor");
}

/// One short trajectory per kind, sampled at actual DWM presentation boundaries.
/// This is opt-in desktop evidence, never a timing assertion in the unit suite.
fn track_frames(red: bool) -> serde_json::Value {
    // SAFETY: this test thread owns no windows; match the production per-monitor coordinate space.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let ink_left = |pixels: &super::super::image::Rgba| {
        pixels
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter_map(|(n, p)| {
                let ink = if red {
                    p[0] > 180 && p[1] < 100 && p[2] < 100
                } else {
                    p[0] < 230 && p[1] < 230 && p[2] < 230
                };
                ink.then_some(n as u32 % pixels.width)
            })
            .min()
    };
    // SAFETY: bounded pointer path remains over the private destination.
    unsafe {
        SetCursorPos(501, 400).unwrap();
    }
    std::thread::sleep(Duration::from_millis(80));
    // SAFETY: capture owns all temporary GDI storage and DCs.
    let reference = ink_left(&unsafe { snapshot() }).expect("drag image must be visible");
    let start = Instant::now();
    let controller = std::thread::spawn(move || {
        // SAFETY: only this private destination is visited, in physical coordinates.
        unsafe {
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let mut commands = Vec::new();
        for n in 0..48 {
            let x = 501 + 4 * if n < 24 { n } else { 48 - n };
            // SAFETY: bounded synthetic pointer motion, no buttons or keys.
            unsafe {
                move_tracking_pointer(x, 400);
            }
            let mut actual = POINT::default();
            // SAFETY: initialized output storage; avoids DPI-virtualized cursor coordinates.
            unsafe { GetPhysicalCursorPos(&mut actual) }.unwrap();
            commands.push((start.elapsed().as_micros() as u64, x, actual.x));
            std::thread::sleep(Duration::from_millis(8));
        }
        commands
    });
    let mut frames = Vec::new();
    while start.elapsed() < Duration::from_millis(420) {
        // SAFETY: wait only for this test thread's DWM frame boundary, never an input/native owner.
        unsafe { windows::Win32::Graphics::Dwm::DwmFlush() }.unwrap();
        let mut cursor = POINT::default();
        // SAFETY: initialized output storage.
        unsafe { GetPhysicalCursorPos(&mut cursor) }.unwrap();
        // SAFETY: capture allocates and releases its GDI objects on this thread.
        let at = ink_left(&unsafe { snapshot() });
        // SAFETY: sample the physical pointer after copying the presented frame.
        unsafe { GetPhysicalCursorPos(&mut cursor) }.unwrap();
        frames.push(serde_json::json!({"at_us": start.elapsed().as_micros() as u64, "cursor_x": cursor.x, "image_x": at.map(|x| x as i32 - reference as i32 + 501)}));
    }
    let commands = controller.join().unwrap();
    // SAFETY: reset the synthetic trajectory to its private-window starting point.
    unsafe {
        SetCursorPos(501, 400).unwrap();
    }
    std::thread::sleep(Duration::from_millis(80));
    serde_json::json!({"frames":frames,"commands":commands})
}

unsafe fn move_tracking_pointer(x: i32, y: i32) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    // SAFETY: system metrics query the current virtual desktop, without modifying it.
    let (ox, oy, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let normalize =
        |p: i32, o: i32, extent: i32| ((i64::from(p - o) * 65536 + i64::from(extent) - 1) / i64::from(extent)) as i32;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: normalize(x, ox, width),
                dy: normalize(y, oy, height),
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                dwExtraInfo: 0x4F49_4E4B,
                ..Default::default()
            },
        },
    };
    // SAFETY: exactly one bounded absolute move. The production tag prevents
    // an installed capture hook from rerouting this private synthetic trajectory.
    assert_eq!(unsafe { SendInput(&[input], size_of::<INPUT>() as i32) }, 1);
}
