//! A separate Shell consumer exercises the provider's real cross-process I/O.
use super::*;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, POINTL, WAIT_OBJECT_0, WPARAM},
        System::{
            Com::{CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IStream, TYMED_HGLOBAL},
            Ole::{DROPEFFECT_MOVE, IDropTarget, OleInitialize, OleUninitialize, ReleaseStgMedium},
            StationsAndDesktops::*,
            SystemServices::MK_LBUTTON,
            Threading::*,
        },
        UI::{
            Shell::{
                BHID_Stream, IShellFolder, IShellItem, SHBindToParent, SHCreateItemFromParsingName, SHParseDisplayName,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, PCWSTR, PWSTR},
};

#[test]
#[ignore = "private Cloud Files + separate zipfldr process; run on a reserved logged-in desktop"]
fn native_zip_drop_deletes_only_received_copy() {
    let _ = tracing_subscriber::fmt().with_test_writer().with_ansi(false).try_init();
    for warm in [true, false] {
        zip_drop(warm);
    }
}

fn zip_drop(warm: bool) {
    // SAFETY: private apartment; the parent never publishes clipboard data or UI.
    unsafe {
        OleInitialize(None).unwrap();
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: balances initialization after all parent COM objects drop.
            unsafe {
                OleUninitialize();
            }
        }
    }
    let _apartment = Apartment;
    let temp = tempfile::Builder::new().prefix("oink-zip-").tempdir().unwrap();
    let root = temp.path().join("CloudFiles");
    if warm {
        // Also exercise updating a registration created by an earlier run.
        let first = Service::new(root.clone()).unwrap();
        first.stop();
    }
    let service = Service::new(root).unwrap();
    struct Cleanup(Arc<Service>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.stop();
            cleanup_native_test_root(&self.0.root);
        }
    }
    let _cleanup = Cleanup(service.clone());
    struct Contents(PathBuf, u64);
    impl FileContents for Contents {
        fn open(&self, _: usize) -> io::Result<crate::FileReader> {
            Ok(Box::new(std::fs::File::open(&self.0)?))
        }
        fn placeholder_metadata(&self, _: usize) -> io::Result<Option<(FileMetadata, u64)>> {
            Ok(Some((
                FileMetadata {
                    exec_bits: None,
                    size: self.1,
                    modified_ms: Some(1_760_000_000_000),
                },
                1,
            )))
        }
    }
    let payload = crate::dnd::preview::asymmetric_png();
    let original = temp.path().join("original.png");
    std::fs::write(&original, &payload).unwrap();
    let filename = "image.png";
    let mut entries = vec![entry(filename, payload.len() as u64, false)];
    entries[0].modified_ms = Some(1_760_000_000_000);
    let previous = std::mem::replace(&mut *CURRENT.lock().unwrap(), Arc::downgrade(&service));
    let built = crate::windows::dnd::data_object(&crate::dnd::DragSpec {
        operation: crate::dnd::DragOperation::new(),
        at: Default::default(),
        items: vec![oink_foundation::clipboard::ClipData::Files(entries)],
        files: Some(Arc::new(Contents(original.clone(), payload.len() as u64))),
        native: None,
        ops: crate::dnd::DragOps::COPY,
    });
    *CURRENT.lock().unwrap() = previous;
    let (data, _shared, allowed) = built.unwrap();
    let format = |id| FORMATETC {
        cfFormat: id,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    };
    let read = |id| {
        // SAFETY: private object and a returned medium released once after copying.
        unsafe {
            let mut medium = data.GetData(&format(id)).unwrap();
            let bytes = crate::windows::vfile::hglobal_bytes(medium.u.hGlobal).unwrap();
            ReleaseStgMedium(&mut medium);
            bytes
        }
    };
    let hdrop = read(15);
    // This file/promise offer does not advertise CIDA. Do not synthesize
    // an extra Shell format in the target process; transfer its actual HDROP.
    let delivery = service.deliveries.lock().unwrap().values().next().unwrap().clone();
    reader_catalog(&service, &delivery);
    let preferred = u32::from_le_bytes(
        read(crate::windows::formats().preferred_drop_effect as u16)[..4]
            .try_into()
            .unwrap(),
    );
    let ready = temp.path().join("drop.ready");
    if warm {
        let check: crate::content::ReaderGuard = Arc::new(|| Ok(()));
        crate::content::namespace::NativeFiles::download(&*delivery, &check, &|_, _| {}).unwrap();
    }
    // Production activates NativeFiles::download after the peer accepts the
    // native drop. This fixture has no peer session; use the child's Drop return
    // as that boundary, keeping provider and consumer I/O in separate processes.
    let downloading = (!warm).then(|| {
        let ready = ready.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "private consumer did not accept the drop");
                std::thread::sleep(Duration::from_millis(10));
            }
            let check: crate::content::ReaderGuard = Arc::new(|| Ok(()));
            crate::content::namespace::NativeFiles::download(&*delivery, &check, &|_, _| {}).unwrap();
        })
    });
    let paths = crate::windows::files::parse_hdrop(&hdrop, crate::windows::ansi_to_string);
    assert_eq!(paths.len(), 1);
    let source = paths[0].clone();
    let zip = temp.path().join("archive.zip");
    let mut empty = vec![0u8; 22];
    empty[..4].copy_from_slice(b"PK\x05\x06");
    std::fs::write(&zip, empty).unwrap();
    let hdrop_file = temp.path().join("offer.hdrop");
    std::fs::write(&hdrop_file, hdrop).unwrap();
    let result_file = temp.path().join("result.json");
    let result = run_consumer(&[
        ("OINK_PRIVATE_ZIP_HDROP", hdrop_file.into_os_string()),
        ("OINK_PRIVATE_ZIP_ARCHIVE", zip.clone().into_os_string()),
        ("OINK_PRIVATE_ZIP_RESULT", result_file.clone().into_os_string()),
        ("OINK_PRIVATE_ZIP_READY", ready.into_os_string()),
        ("OINK_PRIVATE_ZIP_PREFERENCE", preferred.to_string().into()),
        ("OINK_PRIVATE_ZIP_ALLOWED", allowed.0.to_string().into()),
    ]);
    if let Some(downloading) = downloading {
        downloading.join().unwrap();
    }
    let receipt = std::fs::read_to_string(&result_file).unwrap_or_default();
    eprintln!(
        "CROSS_PROCESS_ZIP warm={warm} status={result} source_exists={} receipt={receipt}",
        source.exists()
    );
    assert_eq!(result, 0, "private zipfldr consumer failed: {receipt}");
    assert!(
        !source.exists(),
        "zipfldr must delete the received copy after archiving it"
    );
    assert_eq!(
        std::fs::read(&original).unwrap(),
        payload,
        "provider original must remain unchanged"
    );
    // Read back through the ZIP namespace to verify the actual archived bytes.
    // SAFETY: fixture-owned archive path and apartment-owned Shell objects.
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide(zip.join(filename)).as_ptr()), None).unwrap();
        let stream: IStream = item.BindToHandler(None, &BHID_Stream).unwrap();
        let mut contents = vec![0; payload.len() + 1];
        let mut read = 0;
        stream
            .Read(contents.as_mut_ptr().cast(), contents.len() as u32, Some(&mut read))
            .ok()
            .unwrap();
        contents.truncate(read as usize);
        assert_eq!(contents, payload);
    }
}

/// Keep every child thread on an unshown desktop, including Shell worker/UI
/// threads. Setting just the test thread's desktop does not establish this.
fn run_consumer(extra: &[(&str, std::ffi::OsString)]) -> u32 {
    use std::os::windows::ffi::OsStrExt;
    let name = format!("oink-zip-test-{}", std::process::id());
    let mut desktop_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "\"{}\" --exact windows::cloud::tests::zip_drop::external_zip_operation --ignored --nocapture --test-threads=1",
        exe.display()
    );
    let mut command: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let mut environment = std::env::vars_os().collect::<Vec<_>>();
    environment.extend(extra.iter().map(|(key, value)| ((*key).into(), value.clone())));
    environment.sort_by_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut block = Vec::<u16>::new();
    for (key, value) in environment {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    // SAFETY: live UTF-16 command/environment, owned desktop and child handles.
    unsafe {
        let desktop = CreateDesktopW(
            PCWSTR(desktop_name.as_ptr()),
            PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            DESKTOP_CREATEWINDOW.0
                | DESKTOP_CREATEMENU.0
                | DESKTOP_READOBJECTS.0
                | DESKTOP_WRITEOBJECTS.0
                | DESKTOP_ENUMERATE.0,
            None,
        )
        .unwrap();
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_name.as_mut_ptr()),
            ..Default::default()
        };
        let mut child = PROCESS_INFORMATION::default();
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(block.as_ptr().cast()),
            PCWSTR::null(),
            &startup,
            &mut child,
        )
        .unwrap();
        CloseHandle(child.hThread).unwrap();
        let completed = WaitForSingleObject(child.hProcess, 15000) == WAIT_OBJECT_0;
        if !completed {
            TerminateProcess(child.hProcess, 124).unwrap();
            let _ = WaitForSingleObject(child.hProcess, 3000);
        }
        let mut code = 124;
        GetExitCodeProcess(child.hProcess, &mut code).unwrap();
        CloseHandle(child.hProcess).unwrap();
        CloseDesktop(desktop).unwrap();
        assert!(completed, "private Shell child timed out");
        code
    }
}

#[derive(Default)]
struct Dialogs {
    count: AtomicUsize,
    text: Mutex<Vec<String>>,
}

#[test]
#[ignore = "child of private Cloud Files zipfldr fixture"]
fn external_zip_operation() {
    let Some(hdrop) = std::env::var_os("OINK_PRIVATE_ZIP_HDROP") else {
        return;
    };
    let error_file = PathBuf::from(std::env::var_os("OINK_PRIVATE_ZIP_RESULT").unwrap());
    std::panic::set_hook(Box::new(move |panic| {
        if !error_file.exists() {
            let _ = std::fs::write(&error_file, serde_json::json!({"panic": panic.to_string()}).to_string());
        }
    }));
    // SAFETY: the parent assigned an unshown desktop at process creation.
    unsafe {
        OleInitialize(None).unwrap();
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: balances this child's private apartment.
            unsafe {
                OleUninitialize();
            }
        }
    }
    let _apartment = Apartment;
    let zip = PathBuf::from(std::env::var_os("OINK_PRIVATE_ZIP_ARCHIVE").unwrap());
    let result = PathBuf::from(std::env::var_os("OINK_PRIVATE_ZIP_RESULT").unwrap());
    let preferred: u32 = std::env::var("OINK_PRIVATE_ZIP_PREFERENCE").unwrap().parse().unwrap();
    let allowed: u32 = std::env::var("OINK_PRIVATE_ZIP_ALLOWED").unwrap().parse().unwrap();
    let hdrop = std::fs::read(hdrop).unwrap();
    let paths = crate::windows::files::parse_hdrop(&hdrop, crate::windows::ansi_to_string);
    let f = crate::windows::formats();
    let (data, shared) = crate::windows::vfile::build(
        crate::windows::vfile::Spec {
            cloud: None,
            descriptors: crate::windows::files::descriptors(&[]),
            contents: Bytes::new(0),
            cf_descriptor: f.file_descriptor,
            cf_contents: f.file_contents,
            fixed: vec![(15, hdrop), (f.preferred_drop_effect, preferred.to_le_bytes().to_vec())],
        },
        false,
    );
    shared.host_drag_images();
    let mut pidl = std::ptr::null_mut();
    let mut last = std::ptr::null_mut();
    // SAFETY: private ZIP path and live apartment-owned Shell objects.
    let target: IDropTarget = unsafe {
        SHParseDisplayName(PCWSTR(wide(&zip).as_ptr()), None, &mut pidl, 0, None).unwrap();
        let folder: IShellFolder = SHBindToParent(pidl, Some(&mut last)).unwrap();
        let target = folder.GetUIObjectOf(HWND::default(), &[last], None).unwrap();
        CoTaskMemFree(Some(pidl.cast()));
        target
    };
    let dialogs = Arc::new(Dialogs::default());
    let seen = dialogs.clone();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    // SAFETY: this child and all its workers inherited the same private desktop.
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()).unwrap() }.0 as usize;
    let watcher = std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            // SAFETY: only this child's unshown desktop and process are observed.
            unsafe {
                let _ = EnumDesktopWindows(
                    Some(HDESK(desktop as _)),
                    Some(close_dialog),
                    LPARAM(Arc::as_ptr(&seen) as isize),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let mut effect = windows::Win32::System::Ole::DROPEFFECT(allowed);
    // SAFETY: isolated native OLE drop with no injected input or Explorer window.
    unsafe {
        target
            .DragEnter(&data, MK_LBUTTON, POINTL::default(), &mut effect)
            .unwrap();
        eprintln!("ZIP_ENTER effect={}", effect.0);
        effect = windows::Win32::System::Ole::DROPEFFECT(allowed);
        target
            .Drop(&data, Default::default(), POINTL::default(), &mut effect)
            .unwrap();
    }
    std::fs::write(std::env::var_os("OINK_PRIVATE_ZIP_READY").unwrap(), []).unwrap();
    let accepted = shared.drag_dropped(windows::Win32::Foundation::DRAGDROP_S_DROP, effect);
    shared.drag_ended(accepted, effect == DROPEFFECT_MOVE);
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        // SAFETY: pump only this apartment's private messages.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if dialogs.count.load(Ordering::Acquire) > 0 || !paths[0].exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done.store(true, Ordering::Release);
    watcher.join().unwrap();
    let count = dialogs.count.load(Ordering::Acquire);
    let receipt = serde_json::json!({"accepted":accepted,"effect":effect.0,"dialogs":count,"text":*dialogs.text.lock().unwrap(),"source_exists":paths[0].exists(),"zip_length":std::fs::metadata(&zip).unwrap().len()});
    std::fs::write(result, receipt.to_string()).unwrap();
    eprintln!("ZIP_RESULT {receipt}");
    assert!(accepted);
    assert_eq!(count, 0, "zipfldr must not display an error after adding the file");
}

unsafe extern "system" fn close_dialog(hwnd: HWND, context: LPARAM) -> BOOL {
    // SAFETY: live enumerated test window and retained callback state.
    unsafe {
        let mut process = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process));
        let mut class = [0u16; 64];
        let length = GetClassNameW(hwnd, &mut class);
        if process == std::process::id() && String::from_utf16_lossy(&class[..length as usize]) == "#32770" {
            let captured = Dialogs::default();
            let _ = EnumChildWindows(Some(hwnd), Some(dialog_text), LPARAM(&captured as *const _ as isize));
            let text = captured.text.lock().unwrap();
            // Shell also uses #32770 for its asynchronous progress UI. It has
            // Pause/Cancel, not OK; do not dismiss it or stop the operation.
            if !text
                .iter()
                .any(|label| matches!(label.as_str(), "确定" | "确定(&O)" | "OK" | "&OK"))
            {
                return true.into();
            }
            let state = &*(context.0 as *const Dialogs);
            if state.count.fetch_add(1, Ordering::AcqRel) == 0 {
                *state.text.lock().unwrap() = text.clone();
            }
            let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(1), LPARAM(0));
        }
    }
    true.into()
}
unsafe extern "system" fn dialog_text(hwnd: HWND, context: LPARAM) -> BOOL {
    // SAFETY: only this private child dialog and retained callback state.
    unsafe {
        let mut text = [0u16; 2048];
        let n = GetWindowTextW(hwnd, &mut text);
        if n > 0 {
            (&*(context.0 as *const Dialogs))
                .text
                .lock()
                .unwrap()
                .push(String::from_utf16_lossy(&text[..n as usize]));
        }
    }
    true.into()
}
