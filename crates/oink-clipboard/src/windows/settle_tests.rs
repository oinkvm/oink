//! Native clipboard tests in a child process with its own noninteractive window
//! station and clipboard. Never open or write WinSta0's/user-session clipboard.
use super::*;
use crate::model::StandardFormat;
use crate::sync::Ordering as ClipboardOrdering;
use oink_foundation::DeviceId;
use std::sync::{
    atomic::{AtomicBool, AtomicIsize, Ordering},
    mpsc,
};
use std::time::Instant;
use windows::Win32::Foundation::{GENERIC_ALL, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::StationsAndDesktops::*;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWLP_WNDPROC, SetWindowLongPtrW, WM_RENDERFORMAT, WNDPROC, WSF_VISIBLE,
};
use windows::core::w;

struct PrivateStation {
    previous: HWINSTA,
    previous_desktop: HDESK,
    station: HWINSTA,
    desktop: HDESK,
}
impl PrivateStation {
    fn new() -> Self {
        // SAFETY: this is the only test in a fresh child. No windows/hooks/clipboard
        // APIs run before switching to an owned station; no desktop is activated.
        unsafe {
            let previous = GetProcessWindowStation().unwrap();
            let previous_desktop = GetThreadDesktop(GetCurrentThreadId()).unwrap();
            let name: Vec<u16> = format!("oink-clipboard-settle-{}\0", std::process::id())
                .encode_utf16()
                .collect();
            let station = CreateWindowStationW(PCWSTR(name.as_ptr()), 0, GENERIC_ALL.0, None).unwrap();
            assert_ne!(station, previous);
            let mut flags = USEROBJECTFLAGS::default();
            GetUserObjectInformationW(
                HANDLE(station.0),
                UOI_FLAGS,
                Some((&mut flags as *mut USEROBJECTFLAGS).cast()),
                size_of::<USEROBJECTFLAGS>() as u32,
                None,
            )
            .unwrap();
            assert_eq!(
                flags.dwFlags & WSF_VISIBLE as u32,
                0,
                "test station must not be interactive"
            );
            SetProcessWindowStation(station).unwrap();
            let desktop = CreateDesktopW(
                w!("default"),
                PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS(0),
                GENERIC_ALL.0,
                None,
            )
            .unwrap();
            SetThreadDesktop(desktop).unwrap();
            Self {
                previous,
                previous_desktop,
                station,
                desktop,
            }
        }
    }
}
impl Drop for PrivateStation {
    fn drop(&mut self) {
        // SAFETY: workers/windows have already been dropped. Restore only this child's
        // association, then destroy its private test desktop and clipboard.
        unsafe {
            let _ = SetProcessWindowStation(self.previous);
            let _ = SetThreadDesktop(self.previous_desktop);
            let _ = CloseDesktop(self.desktop);
            let _ = CloseWindowStation(self.station);
        }
    }
}

fn announced_kinds(source: &mut ClipboardOrdering, reader: &WinClipboard) -> Vec<StandardFormat> {
    let data = tempfile::tempdir().unwrap();
    let captured = reader.capture(data.path()).unwrap();
    let inventory = captured.content.unwrap().inventory();
    source
        .capture(
            crate::wire::inventory(&inventory).unwrap(),
            captured.snapshot.change_count,
            captured.snapshot.sensitivity,
            0,
        )
        .unwrap()
        .expect("changed complete snapshot must announce an offer");
    inventory
        .items
        .into_iter()
        .flat_map(|item| item.representations)
        .filter_map(|rep| rep.standard)
        .fold(Vec::new(), |mut kinds, kind| {
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
            kinds
        })
}

static ORIGINAL_PROC: AtomicIsize = AtomicIsize::new(0);
static RENDERED: AtomicBool = AtomicBool::new(false);
unsafe extern "system" fn render_html(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_RENDERFORMAT && wp.0 == formats().html as usize {
        // The requesting reader holds OpenClipboard. A delayed renderer must set
        // data without opening it again (the documented WM_RENDERFORMAT contract).
        RENDERED.store(
            set_bytes(formats().html, &html::encode_cf_html("<b>lazy</b>")).is_ok(),
            Ordering::Release,
        );
        return LRESULT(0);
    }
    if msg == WM_RENDERFORMAT && wp.0 == formats().rtf as usize {
        set_bytes(formats().rtf, b"{\\rtf1 lazy}\0").unwrap();
        return LRESULT(0);
    }
    // SAFETY: this pointer is the original procedure returned by SetWindowLongPtrW.
    unsafe {
        CallWindowProcW(
            std::mem::transmute::<isize, WNDPROC>(ORIGINAL_PROC.load(Ordering::Acquire)),
            hwnd,
            msg,
            wp,
            lp,
        )
    }
}

#[test]
fn multi_step_writer_settles_on_private_window_station() {
    const CHILD: &str = "OINK_CLIPBOARD_SETTLE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Station association is per-process; isolate it from parallel unit tests.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::settle_tests::multi_step_writer_settles_on_private_window_station",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(180);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                panic!("native clipboard fixture timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _station = PrivateStation::new();
    let writer = Arc::new(worker::Worker::spawn().unwrap());
    let reader = WinClipboard {
        cloud: std::sync::Mutex::new(None),
        worker: Arc::new(worker::Worker::spawn().unwrap()),
        reader: Arc::new(worker::Worker::spawn().unwrap()),
        observer: Arc::new(content::current::Observer::default()),
    };
    let (changed, changes) = mpsc::channel();
    reader
        .watch(Arc::new(move || {
            let _ = changed.send(());
        }))
        .unwrap();
    let mut source = ClipboardOrdering::new(DeviceId([32; 32]), 0);
    let at = Instant::now();

    // Separate transactions: clear -> plain text -> HTML, each followed by a real
    // WM_CLIPBOARDUPDATE. An immediate read can legitimately see just the text.
    writer
        .run(|hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            // SAFETY: this worker owns the open clipboard on its private station.
            unsafe {
                EmptyClipboard().unwrap();
            }
        })
        .unwrap();
    changes.recv().unwrap();
    assert!(reader.read().unwrap().items.is_empty());
    assert!(source.local_change(reader.change_count(), at));
    writer
        .run(|hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            let bytes: Vec<u8> = "copy\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
            set_bytes(u32::from(CF_UNICODETEXT.0), &bytes).unwrap();
        })
        .unwrap();
    changes.recv().unwrap();
    let text_count = reader.change_count();
    assert_eq!(reader.read().unwrap().items, vec![ClipData::Text("copy".into())]);
    assert!(source.local_change(text_count, at + Duration::from_millis(100)));
    writer
        .run(|hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            set_bytes(formats().html, &html::encode_cf_html("<b>copy</b>")).unwrap();
        })
        .unwrap();
    changes.recv().unwrap();
    let final_count = reader.change_count();
    assert_ne!(text_count, final_count);
    source.local_change(final_count, at + Duration::from_millis(200));
    assert!(!source.capture_due(at + Duration::from_millis(499)));
    assert!(source.capture_due(at + Duration::from_millis(500)));
    let complete = reader.read().unwrap();
    assert_eq!(complete.change_count, reader.change_count());
    assert_eq!(
        announced_kinds(&mut source, &reader),
        vec![StandardFormat::Text, StandardFormat::Html]
    );

    // A format arriving after an earlier offer has a new sequence/notification
    // and produces a replacement offer, rather than being hidden by content dedup.
    writer
        .run(|hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            set_bytes(formats().rtf, b"{\\rtf1 copy}\0").unwrap();
        })
        .unwrap();
    changes.recv().unwrap();
    source.local_change(reader.change_count(), at + Duration::from_secs(1));
    assert!(source.capture_due(at + Duration::from_millis(1300)));
    assert!(announced_kinds(&mut source, &reader).contains(&StandardFormat::Rtf));

    // One transaction held open across its steps: a reader must fail Busy, never
    // accept the partially filled formats. Release it only after testing this.
    let (entered, writing) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let w = writer.clone();
    let held = std::thread::spawn(move || {
        w.run(move |hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            // SAFETY: this worker owns the open clipboard on its private station.
            unsafe {
                EmptyClipboard().unwrap();
            }
            let bytes: Vec<u8> = "held\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
            set_bytes(u32::from(CF_UNICODETEXT.0), &bytes).unwrap();
            entered.send(()).unwrap();
            released.recv().unwrap();
            set_bytes(formats().html, &html::encode_cf_html("<b>held</b>")).unwrap();
        })
        .unwrap()
    });
    writing.recv().unwrap();
    let busy = reader.read();
    release.send(()).unwrap();
    held.join().unwrap();
    assert!(matches!(busy, Err(ClipboardError::Busy)));
    assert_eq!(reader.read().unwrap().items.len(), 2);

    // The native clipboard path must own metadata only, including a large
    // multi-representation bitmap offer. All writes stay on this private station.
    let root = tempfile::tempdir().unwrap();
    let captured = reader.capture(root.path()).unwrap();
    let provider = captured.content.unwrap();
    assert!(
        std::fs::read_dir(root.path()).unwrap().next().is_none(),
        "clipboard capture must not create source snapshots"
    );
    let representation = provider.inventory().items[0]
        .representations
        .iter()
        .find(|rep| matches!(rep.data, crate::model::RepresentationData::Reader { .. }))
        .unwrap()
        .id;
    writer
        .run(|hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            // SAFETY: private station, the worker owns the clipboard.
            unsafe {
                EmptyClipboard().unwrap();
            }
            set_bytes(13, &[66, 0, 0, 0]).unwrap();
        })
        .unwrap();
    assert_eq!(
        crate::content::reason(&provider.open(representation).err().expect("old offer must fail")),
        crate::model::Unavailable::Superseded
    );

    // CF_HDROP captures retain only the original disk reference. Replacing
    // this private clipboard must not revoke a later read or its latest version.
    let retained_root = tempfile::tempdir().unwrap();
    let retained_path = retained_root.path().join("file");
    std::fs::write(&retained_path, b"old").unwrap();
    let hdrop = files::build_hdrop(std::slice::from_ref(&retained_path));
    writer
        .run(move |hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            // SAFETY: this worker owns the clipboard on its private station.
            unsafe {
                EmptyClipboard().unwrap();
            }
            set_bytes(u32::from(CF_HDROP.0), &hdrop).unwrap();
        })
        .unwrap();
    let provider = reader.capture(root.path()).unwrap().content.unwrap();
    let files = provider.namespace().unwrap();
    assert!(files.real_clipboard_files());
    let registry = crate::content::Registry::default();
    drop(registry.register_clipboard(provider).unwrap());
    writer
        .run(|hwnd| {
            let _open = OpenGuard::open(hwnd).unwrap();
            // SAFETY: this worker owns the clipboard on its private station.
            unsafe {
                EmptyClipboard().unwrap();
            }
            set_bytes(13, &[67, 0, 0, 0]).unwrap();
        })
        .unwrap();
    std::fs::write(&retained_path, b"latest file").unwrap();
    let entry = files.select(crate::model::NodeId(1)).unwrap();
    assert_eq!(entry.size, crate::model::ContentSize::Known(11));
    assert_eq!(
        crate::content::read_all(&mut files.open(entry.id, entry.version, 0).unwrap()).unwrap(),
        b"latest file"
    );
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());

    // Declared-but-lazy HTML is materialized by GetClipboardData, with the writer's
    // actual WM_RENDERFORMAT handler. Retry the snapshot if rendering moved the seq.
    writer
        // SAFETY: hwnd belongs to this worker. The replacement procedure remains
        // valid for the window lifetime; data is set while this worker owns it.
        .run(|hwnd| unsafe {
            ORIGINAL_PROC.store(
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, render_html as *const () as isize),
                Ordering::Release,
            );
            let _open = OpenGuard::open(hwnd).unwrap();
            EmptyClipboard().unwrap();
            let bytes: Vec<u8> = "lazy\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
            set_bytes(u32::from(CF_UNICODETEXT.0), &bytes).unwrap();
            let _ = SetClipboardData(formats().html, None);
            assert!(available(formats().html));
        })
        .unwrap();
    let captured = reader.capture(root.path()).unwrap();
    let announced = captured.snapshot.change_count;
    assert!(
        !RENDERED.load(Ordering::Acquire),
        "inventory must not render lazy payloads"
    );
    let provider = captured.content.unwrap();
    let rep = provider.inventory().items[0]
        .representations
        .iter()
        .find(|rep| rep.standard == Some(crate::model::StandardFormat::Html))
        .unwrap()
        .id;
    let mut bytes = provider.open(rep).unwrap();
    assert!(
        crate::content::read_all(&mut bytes)
            .unwrap()
            .windows(4)
            .any(|w| w == b"lazy")
    );
    assert_eq!(reader.change_count(), announced, "our render is not another copy");
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    let first = reader.read().unwrap();
    assert!(RENDERED.load(Ordering::Acquire));
    assert!(first.items.contains(&ClipData::Html("<b>lazy</b>".into())));
    let stable = if first.change_count == reader.change_count() {
        first
    } else {
        reader.read().unwrap()
    };
    assert_eq!(stable.change_count, reader.change_count());
    assert_eq!(stable.items.len(), 2);
    successive_lazy_reads(&reader, &writer);
    native_provider_bumps_and_failures(&reader, &writer);
    multi_representation_image_has_no_source_store(&reader, &writer);
    virtual_clipboard_files_read_current_object_without_store(&reader, &writer);
    eprintln!(
        "NATIVE_SETTLE windows: staged transactions, reannounce, open contention, delayed rendering passed; private station only"
    );
}

fn multi_representation_image_has_no_source_store(reader: &WinClipboard, writer: &worker::Worker) {
    writer
        .run(|hwnd| {
            let rgba = image::Rgba {
                pixels_per_meter: None,
                width: 3468,
                height: 3174,
                data: vec![0x7f; 3468 * 3174 * 4],
            };
            let png = image::encode_png(&rgba).unwrap();
            let dib32 = image::rgba_to_dibv5(&rgba);
            let dib24 = image::rgba_to_dib24(&rgba);
            let _open = OpenGuard::open(hwnd).unwrap();
            // SAFETY: private station; OS assumes ownership of the successfully set bitmap.
            unsafe {
                EmptyClipboard().unwrap();
            }
            set_bytes(8, &dib24).unwrap();
            set_bytes(17, &dib32).unwrap();
            for (name, bytes) in [
                ("PNG", &png),
                ("Oink.Image.PNG", &png),
                ("Oink.Image.DIB32", &dib32),
                ("Oink.Image.DIB24", &dib24),
            ] {
                let name: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
                // SAFETY: the UTF-16 format name is NUL-terminated and lives through this call.
                let cf = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
                set_bytes(cf, bytes).unwrap();
            }
            // SAFETY: the pixel slice contains the full bitmap; the private clipboard
            // assumes ownership of this new GDI object on successful SetClipboardData.
            unsafe {
                let bitmap =
                    windows::Win32::Graphics::Gdi::CreateBitmap(3468, 3174, 1, 32, Some(rgba.data.as_ptr().cast()));
                assert!(!bitmap.is_invalid());
                SetClipboardData(2, Some(HANDLE(bitmap.0))).unwrap();
            }
        })
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let capture = reader.capture(root.path()).unwrap();
    let source = capture.content.unwrap();
    assert!(
        !source
            .inventory()
            .automatic_pull(oink_foundation::clipboard::ClipPlatform::Windows)
    );
    assert!(source.inventory().items[0].representations.len() >= 7);
    assert!(source.local_batch().is_none());
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    let png = source.inventory().items[0]
        .representations
        .iter()
        .find(|rep| rep.format == crate::model::NativeFormat::WindowsRegistered("PNG".into()))
        .unwrap()
        .id;
    let mut bytes = source.open(png).unwrap();
    assert_eq!(
        &crate::content::read_all(&mut bytes).unwrap()[..8],
        b"\x89PNG\r\n\x1a\n"
    );
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    eprintln!("CURRENT_CLIPBOARD 3468x3174 PNG/DIB24/DIB32/bitmap: no source files at capture or request");
}

fn successive_lazy_reads(reader: &WinClipboard, writer: &worker::Worker) {
    use crate::model::StandardFormat;
    let publish = |two| {
        writer
            .run(move |hwnd| {
                let _open = OpenGuard::open(hwnd).unwrap();
                // SAFETY: private-station owner declares lazy formats handled by render_html.
                unsafe {
                    EmptyClipboard().unwrap();
                    let _ = SetClipboardData(formats().html, None);
                    if two {
                        let _ = SetClipboardData(formats().rtf, None);
                    }
                }
            })
            .unwrap()
    };
    let root = tempfile::tempdir().unwrap();
    publish(true);
    let capture = reader.capture(root.path()).unwrap();
    let count = capture.snapshot.change_count;
    let source = capture.content.unwrap();
    for standard in [StandardFormat::Html, StandardFormat::Rtf] {
        let id = source.inventory().items[0]
            .representations
            .iter()
            .find(|r| r.standard == Some(standard))
            .unwrap()
            .id;
        assert!(source.open(id).is_ok());
        assert_eq!(reader.change_count(), count);
    }
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
}

fn virtual_clipboard_files_read_current_object_without_store(reader: &WinClipboard, writer: &worker::Worker) {
    use std::sync::atomic::AtomicUsize;
    struct Files(Arc<AtomicUsize>);
    impl crate::FileContents for Files {
        fn descriptor_size_known(&self, _: usize) -> bool {
            false
        }
        fn open(&self, _: usize) -> std::io::Result<crate::FileReader> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(std::io::Cursor::new(vec![9; 1024 * 1024 + 3])))
        }
        fn metadata(&self, _: usize) -> std::io::Result<Option<crate::FileMetadata>> {
            Ok(Some(crate::FileMetadata {
                size: 1024 * 1024 + 3,
                modified_ms: None,
            }))
        }
    }
    let reads = Arc::new(AtomicUsize::new(0));
    let content_reads = reads.clone();
    writer
        .run(move |_| {
            let entries = vec![FileEntry {
                path: "current.bin".into(),
                size: 0,
                is_dir: false,
                modified_ms: None,
                local_path: None,
            }];
            vfile::set_clipboard(vfile::Spec {
                cloud: None,
                descriptors: files::descriptors(&entries),
                fixed: Vec::new(),
                contents: Arc::new(Files(content_reads)),
                cf_descriptor: formats().file_descriptor,
                cf_contents: formats().file_contents,
            })
            .unwrap();
        })
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let capture = reader.capture(root.path()).unwrap();
    let source = capture.content.unwrap();
    assert_eq!(reads.load(Ordering::Acquire), 0, "copy must not read any FileContents");
    let files = source.namespace().unwrap();
    let id = files.roots()[0].id;
    let node = files.select(id).unwrap();
    let mut input = files.open(id, node.version, 0).unwrap();
    let bytes = crate::content::read_all(&mut input).unwrap();
    assert_eq!(bytes, vec![9; 1024 * 1024 + 3]);
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    reader
        .write(
            &[ClipData::Text("peer placeholder replaced source".into())],
            Sensitivity::Normal,
        )
        .unwrap();
    assert_eq!(
        crate::content::reason(&files.select(id).err().unwrap()),
        crate::model::Unavailable::Superseded
    );
}

fn native_provider_bumps_and_failures(reader: &WinClipboard, writer: &worker::Worker) {
    use crate::content::{Provider, Translator};
    use crate::model::*;
    use std::sync::{Mutex, atomic::AtomicU8};
    struct Source {
        inventory: Inventory,
        mode: Arc<AtomicU8>,
        hwnd: isize,
        observer: Arc<content::current::Observer>,
        observed_during_read: Arc<Mutex<(u64, u64)>>,
    }
    struct NoTranslation;
    impl Translator for NoTranslation {
        fn standard(&self, _: &Representation, _: &[u8]) -> std::io::Result<Vec<ClipData>> {
            unreachable!()
        }
    }
    impl Provider for Source {
        fn inventory(&self) -> Inventory {
            self.inventory.clone()
        }
        fn translator(&self) -> Arc<dyn Translator> {
            Arc::new(NoTranslation)
        }
        fn open(&self, id: RepresentationId) -> std::io::Result<crate::FileReader> {
            match self.mode.load(Ordering::Acquire) {
                1 | 2 => {
                    let _open = OpenGuard::open(HWND(self.hwnd as _)).unwrap();
                    if self.mode.load(Ordering::Acquire) == 2 {
                        // A different actual copy changes the owner and format list.
                        // SAFETY: OpenGuard owns the private-station clipboard.
                        unsafe {
                            EmptyClipboard().unwrap();
                        }
                        set_bytes(13, &[88, 0, 0, 0]).unwrap();
                    } else {
                        // Materialize the already-offered format without changing
                        // ownership. This exercises an actual sequence bump on read.
                        let cf =
                            content::format_id(&crate::content::find(&self.inventory, id).unwrap().format).unwrap();
                        set_bytes(u32::from(cf), &[1, 2, 3, 4]).unwrap();
                    }
                    // Observe the notification before the old provider returns.
                    // A hung old reader must not hide another owner's real copy.
                    let physical = native_sequence();
                    *self.observed_during_read.lock().unwrap() = (physical, self.observer.logical(physical));
                }
                3 => return Err(crate::content::unavailable(Unavailable::ReadFailed)),
                _ => {}
            }
            Ok(Box::new(std::io::Cursor::new(vec![1, 2, 3, 4])))
        }
    }
    let root = tempfile::tempdir().unwrap();
    for mode in [1, 2, 3] {
        let behavior = Arc::new(AtomicU8::new(0));
        let test_behavior = behavior.clone();
        let observed = Arc::new(Mutex::new((0, 0)));
        let observed_during_read = observed.clone();
        let observer = reader.observer.clone();
        writer
            .run(move |hwnd| {
                let format = NativeFormat::WindowsRegistered("Oink.Current.Provider".into());
                let mut inventory = Inventory {
                    id: OfferId {
                        clip: oink_foundation::content::ClipId {
                            origin: DeviceId([7; 32]),
                            seq: 1,
                        },
                        generation: 1,
                    },
                    platform: ClipPlatform::Windows,
                    roots: vec![],
                    items: vec![ContentItem {
                        virtual_files: false,
                        id: ItemId(0),
                        file: None,
                        representations: vec![Representation {
                            id: RepresentationId(1),
                            format,
                            medium: Medium::HGlobal,
                            aspect: 1,
                            index: -1,
                            standard: None,
                            data: RepresentationData::Reader {
                                size: ContentSize::Unknown,
                                hash: None,
                            },
                        }],
                    }],
                };
                let mut second = inventory.items[0].representations[0].clone();
                second.id = RepresentationId(2);
                second.format = NativeFormat::WindowsRegistered("Oink.Current.Provider2".into());
                inventory.items[0].representations.push(second);
                let source = Arc::new(Source {
                    inventory: inventory.clone(),
                    mode: test_behavior,
                    hwnd: hwnd.0 as isize,
                    observer,
                    observed_during_read,
                });
                content::target::set_clipboard_guarded(
                    crate::NativeSpec {
                        inventory,
                        contents: source,
                        files: vec![],
                        file_contents: None,
                        sensitivity: Sensitivity::Normal,
                    },
                    Arc::new(|| Ok(())),
                )
                .unwrap();
            })
            .unwrap();
        let capture = reader.capture(root.path()).unwrap();
        let announced = capture.snapshot.change_count;
        let source = capture.content.unwrap();
        behavior.store(mode, Ordering::Release);
        let result = source.open(RepresentationId(1));
        match mode {
            1 => {
                assert!(result.is_ok());
                assert_eq!(reader.change_count(), announced);
                assert_eq!(observed.lock().unwrap().1, announced, "own rendering stays quiet");
                assert_ne!(native_sequence(), announced, "fixture must bump the raw sequence");
                let first = native_sequence();
                assert!(source.open(RepresentationId(2)).is_ok());
                assert_ne!(native_sequence(), first, "each lazy format can bump again");
                assert_eq!(reader.change_count(), announced);
            }
            2 => {
                assert_eq!(crate::content::reason(&result.err().unwrap()), Unavailable::Superseded);
                assert_ne!(native_sequence(), announced);
                let (physical, logical) = *observed.lock().unwrap();
                assert_ne!(physical, announced);
                assert_eq!(
                    logical, physical,
                    "a real new owner is visible before the old native read completes"
                );
            }
            3 => assert_eq!(crate::content::reason(&result.err().unwrap()), Unavailable::ReadFailed),
            _ => unreachable!(),
        }
    }
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
}

fn native_sequence() -> u64 {
    // SAFETY: parameter-free query of this test's private window station.
    u64::from(unsafe { GetClipboardSequenceNumber() })
}
