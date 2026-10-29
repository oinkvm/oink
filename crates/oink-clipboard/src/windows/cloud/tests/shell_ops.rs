//! External Shell operations on a private native Cloud Files root.
use super::*;
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::UI::Shell::{IFileSystemBindData, IFileSystemBindData_Impl};

#[windows::core::implement(IFileSystemBindData)]
struct ShellMetadata(Mutex<WIN32_FIND_DATAW>);
impl IFileSystemBindData_Impl for ShellMetadata_Impl {
    fn SetFindData(&self, data: *const WIN32_FIND_DATAW) -> windows::core::Result<()> {
        // SAFETY: Shell supplies a live WIN32_FIND_DATAW for this COM call.
        *self.0.lock().unwrap() = unsafe { *data };
        Ok(())
    }
    fn GetFindData(&self, data: *mut WIN32_FIND_DATAW) -> windows::core::Result<()> {
        // SAFETY: Shell supplies a writable WIN32_FIND_DATAW for this COM call.
        unsafe { *data = *self.0.lock().unwrap() };
        Ok(())
    }
}

struct Contents(Arc<Bytes>);
impl FileContents for Contents {
    fn open(&self, index: usize) -> io::Result<crate::FileReader> {
        self.0.open(index)
    }
    fn placeholder_metadata(&self, index: usize) -> io::Result<Option<(crate::FileMetadata, u64)>> {
        Ok(self.0.metadata(index)?.map(|metadata| (metadata, 1)))
    }
}

#[test]
#[ignore = "child of private native Shell operation fixture"]
fn external_operation() {
    let Some(source) = std::env::var_os("OINK_PRIVATE_SHELL_SOURCE") else {
        return;
    };
    let source = PathBuf::from(source);
    let target = PathBuf::from(std::env::var_os("OINK_PRIVATE_SHELL_TARGET").unwrap());
    let mode = std::env::var("OINK_PRIVATE_SHELL_MODE").unwrap();
    assert!(source.starts_with(std::env::temp_dir()) && target.starts_with(std::env::temp_dir()));
    eprintln!(
        "PRIVATE_SHELL_CHILD mode={mode} source={source:?} metadata={:?}",
        std::fs::symlink_metadata(&source)
    );
    if mode == "rename" || mode == "rename-children" {
        let moves = if mode == "rename-children" {
            std::fs::create_dir(&target).unwrap();
            std::fs::read_dir(&source)
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    let destination = target.join(path.file_name().unwrap());
                    (path, destination)
                })
                .collect()
        } else {
            vec![(source, target)]
        };
        for (source, target) in moves {
            assert_ne!(
                // SAFETY: only this fixture's private same-volume paths.
                unsafe { MoveFileExW(wide(&source).as_ptr(), wide(&target).as_ptr(), 0) },
                0,
                "external MoveFileEx: {}",
                io::Error::last_os_error()
            );
        }
        return;
    }
    use windows::Win32::{System::Com::*, UI::Shell::*};
    use windows::core::PCWSTR;
    // SAFETY: this child owns its apartment, objects, and private path strings.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        {
            if mode == "drop" {
                use windows::Win32::{
                    Foundation::POINTL,
                    System::{
                        DataExchange::RegisterClipboardFormatW,
                        Ole::*,
                        SystemServices::{MK_LBUTTON, MK_SHIFT},
                    },
                    UI::WindowsAndMessaging::*,
                };
                use windows::core::w;
                OleInitialize(None).unwrap();
                std::fs::create_dir(&target).unwrap();
                let paths = std::fs::read_dir(&source)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect::<Vec<_>>();
                assert_eq!(paths.len(), 2);
                let cida_id = RegisterClipboardFormatW(w!("Shell IDList Array"));
                let formats = crate::windows::formats();
                let fixed = vec![
                    (15, crate::windows::files::build_hdrop(&paths)),
                    (formats.preferred_drop_effect, 3u32.to_le_bytes().to_vec()),
                    (
                        cida_id,
                        std::fs::read(std::env::var_os("OINK_PRIVATE_SHELL_CIDA_FILE").unwrap()).unwrap(),
                    ),
                ];
                let (data, shared) = crate::windows::vfile::build(
                    crate::windows::vfile::Spec {
                        cloud: None,
                        descriptors: crate::windows::files::descriptors(&[]),
                        contents: Bytes::new(0),
                        cf_descriptor: formats.file_descriptor,
                        cf_contents: formats.file_contents,
                        fixed,
                    },
                    false,
                );
                shared.host_drag_images();
                let mut pidl = std::ptr::null_mut();
                SHParseDisplayName(PCWSTR(wide(&target).as_ptr()), None, &mut pidl, 0, None).unwrap();
                let folder: IShellFolder = SHBindToObject(None, pidl, None).unwrap();
                CoTaskMemFree(Some(pidl.cast()));
                let drop: IDropTarget = folder.CreateViewObject(Default::default()).unwrap();
                let mut effect = DROPEFFECT_COPY | DROPEFFECT_MOVE;
                let keys = MK_SHIFT;
                drop.DragEnter(&data, MK_LBUTTON | keys, POINTL::default(), &mut effect)
                    .unwrap();
                drop.Drop(&data, keys, POINTL::default(), &mut effect).unwrap();
                let accepted = shared.drag_dropped(windows::core::HRESULT(DRAGDROP_S_DROP), effect);
                shared.drag_ended(accepted, effect == DROPEFFECT_MOVE);
                let deadline = Instant::now() + Duration::from_secs(4);
                while Instant::now() < deadline {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    if target.join("left").is_dir() && target.join("right/data.bin").is_file() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                eprintln!(
                    "PRIVATE_SHELL_DROP accepted={accepted} effect={} left_source={} left_target={} right_target={}",
                    effect.0,
                    source.join("left").exists(),
                    target.join("left").exists(),
                    target.join("right/data.bin").exists()
                );
                OleUninitialize();
                CoUninitialize();
                return;
            }
            let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER).unwrap();
            operation
                .SetOperationFlags(FOF_SILENT | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_NOCONFIRMMKDIR)
                .unwrap();
            if mode == "children" {
                std::fs::create_dir(&target).unwrap();
            }
            let destination = if mode == "children" {
                target.as_path()
            } else {
                target.parent().unwrap()
            };
            let folder: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide(destination).as_ptr()), None).unwrap();
            let items = if mode == "children" {
                std::fs::read_dir(&source)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect()
            } else {
                vec![source.clone()]
            };
            for path in items {
                let item: IShellItem =
                    SHCreateItemFromParsingName(PCWSTR(wide(&path).as_ptr()), None).unwrap_or_else(|error| {
                        // Windows 11's default parser rejects this private root's
                        // cloud ancestors. Try its documented metadata bind context;
                        // IFileOperation still performs the real filesystem I/O.
                        eprintln!("PRIVATE_SHELL_PARSE_DEFAULT {error}");
                        let mut found = WIN32_FIND_DATAW::default();
                        let find = windows::Win32::Storage::FileSystem::FindFirstFileW(
                            PCWSTR(wide(&path).as_ptr()),
                            &mut found,
                        )
                        .unwrap();
                        windows::Win32::Storage::FileSystem::FindClose(find).unwrap();
                        let context = CreateBindCtx(0).unwrap();
                        let data: IFileSystemBindData = ShellMetadata(Mutex::new(found)).into();
                        context.RegisterObjectParam(STR_FILE_SYS_BIND_DATA, &data).unwrap();
                        SHCreateItemFromParsingName(PCWSTR(wide(&path).as_ptr()), &context).unwrap()
                    });
                let name = wide(if mode == "children" {
                    path.file_name().unwrap()
                } else {
                    target.file_name().unwrap()
                });
                if mode == "copy" {
                    operation.CopyItem(&item, &folder, PCWSTR(name.as_ptr()), None).unwrap();
                } else {
                    operation.MoveItem(&item, &folder, PCWSTR(name.as_ptr()), None).unwrap();
                }
            }
            operation.PerformOperations().unwrap();
            assert!(!operation.GetAnyOperationsAborted().unwrap().as_bool());
        }
        CoUninitialize();
    }
}

#[test]
#[ignore = "private native Cloud Files + Shell: run on a reserved logged-in desktop"]
fn native_copy_move_matrix() {
    let _ = tracing_subscriber::fmt().with_test_writer().with_ansi(false).try_init();
    struct Cleanup(Arc<Service>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.stop();
            cleanup_native_test_root(&self.0.root);
        }
    }
    for folder in [false, true] {
        for warm in [false, true] {
            for mode in ["copy", "move", "rename", "children", "rename-children"] {
                if mode.ends_with("children") && !folder {
                    continue;
                }
                if std::env::var("OINK_SHELL_MODE").is_ok_and(|only| only != mode) {
                    continue;
                }
                let temp = tempfile::Builder::new().prefix("oink-shell-").tempdir().unwrap();
                let service = Service::new(temp.path().join("CloudFiles")).unwrap();
                let _cleanup = Cleanup(service.clone());
                let bytes = Bytes::new(1024);
                let entries = if folder {
                    vec![
                        entry("tree", 0, true),
                        entry("tree/child.bin", 1024, false),
                        entry("tree/empty", 0, true),
                    ]
                } else {
                    vec![entry("file.bin", 1024, false)]
                };
                let delivery = service
                    .publish(entries, Arc::new(Contents(bytes.clone())), Instant::now())
                    .unwrap();
                reader_catalog(&service, &delivery);
                let source = delivery.file_path(0);
                let target = temp.path().join("destination");
                if warm {
                    let check: crate::content::ReaderGuard = Arc::new(|| Ok(()));
                    crate::content::namespace::NativeFiles::download(&*delivery, &check, &|_, _| {}).unwrap();
                }
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "windows::cloud::tests::shell_ops::external_operation",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("OINK_PRIVATE_SHELL_SOURCE", &source)
                    .env("OINK_PRIVATE_SHELL_TARGET", &target)
                    .env("OINK_PRIVATE_SHELL_MODE", mode)
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(20);
                let status = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status;
                    }
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        child.wait().unwrap();
                        panic!("private Shell operation timed out: folder={folder} warm={warm} mode={mode}");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let source_exists = source.exists();
                eprintln!("PRIVATE_SHELL_OPENS {:?}", service.received.lock().unwrap().opens);
                eprintln!(
                    "PRIVATE_SHELL folder={folder} warm={warm} mode={mode} status={status} source_exists={source_exists}"
                );
                assert!(status.success());
                assert_eq!(
                    std::fs::read(if folder {
                        target.join("child.bin")
                    } else {
                        target.clone()
                    })
                    .unwrap(),
                    bytes.bytes
                );
                if folder {
                    assert!(target.join("empty").is_dir());
                }
                if mode == "copy" {
                    assert!(source_exists);
                } else {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while source.exists() && Instant::now() < deadline {
                        service.received_sweep();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    eprintln!("PRIVATE_SHELL_FINAL_OPENS {:?}", service.received.lock().unwrap().opens);
                    assert!(
                        !source.exists(),
                        "completed MOVE must leave no received root after cleanup"
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "private native Cloud Files + Shell: run on a reserved logged-in desktop"]
fn native_empty_selected_root_move() {
    // The real renderer runs on its OLE STA; metadata-bound PIDLs need that
    // apartment even though the Cloud Files service itself uses plain cfapi.
    use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
    // SAFETY: this fixture owns its test thread's apartment and all COM lifetimes.
    unsafe {
        OleInitialize(None).unwrap();
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: balances the successful initialization on this thread.
            unsafe {
                OleUninitialize();
            }
        }
    }
    let _apartment = Apartment;
    use crate::content::{
        Provider,
        linked::Descriptor,
        namespace::{Files, Local, Namespace},
    };
    use crate::model::{DirectoryCursor, DirectoryPage, FileNode, NodeId};
    struct Selection(Arc<Local>, Arc<AtomicUsize>, Arc<AtomicUsize>);
    impl Namespace for Selection {
        fn roots(&self) -> Vec<FileNode> {
            self.0.roots()
        }
        fn source_link(&self) -> Option<Descriptor> {
            Some(Descriptor {
                offer: crate::content::tests::fixture().inventory().id,
                token: crate::content::grants::LinkToken([91; 32]),
                roots: self.roots(),
                native: Default::default(),
                provisional: false,
                fulfilled: vec![],
            })
        }
        fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
            self.1.fetch_add(1, Ordering::Relaxed);
            self.0.list(node, cursor)
        }
        fn select(&self, node: NodeId) -> io::Result<FileNode> {
            self.0.select(node)
        }
        fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
            self.2.fetch_add(1, Ordering::Relaxed);
            self.0.open(node, version, offset)
        }
    }
    let _ = tracing_subscriber::fmt().with_test_writer().with_ansi(false).try_init();
    let temp = tempfile::Builder::new().prefix("oink-empty-root-").tempdir().unwrap();
    std::fs::create_dir(temp.path().join("left")).unwrap();
    std::fs::create_dir(temp.path().join("right")).unwrap();
    std::fs::write(temp.path().join("right/data.bin"), b"ordinary right contents").unwrap();
    let service = Service::new(temp.path().join("CloudFiles")).unwrap();
    struct Cleanup(Arc<Service>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.stop();
            cleanup_native_test_root(&self.0.root);
        }
    }
    let _cleanup = Cleanup(service.clone());
    let lists = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let tree = Arc::new(Selection(
        Local::new(vec![
            (NodeId(1), temp.path().join("left")),
            (NodeId(2), temp.path().join("right")),
        ])
        .unwrap(),
        lists.clone(),
        opens.clone(),
    ));
    let contents = Files::new(tree, crate::sync::files::TargetOs::Windows).unwrap();
    let offer = Offer {
        service: service.clone(),
        entries: contents.roots().to_vec(),
        contents,
        created: Instant::now(),
        prepared: Mutex::new(None),
        cancelled: AtomicBool::new(false),
        virtual_files: false,
    };
    let hdrop = offer.hdrop().unwrap();
    let delivery = offer.prepared.lock().unwrap().clone().unwrap();
    let paths = crate::windows::files::parse_hdrop(&hdrop, crate::windows::ansi_to_string);
    assert_eq!(paths, delivery.roots);
    assert_eq!(paths.len(), 2);
    use std::os::windows::fs::MetadataExt;
    let attributes = std::fs::symlink_metadata(&paths[0]).unwrap().file_attributes();
    assert_ne!(attributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);
    let cida = crate::windows::files::shell_id_list(&paths).unwrap();
    assert_eq!(
        std::fs::symlink_metadata(&paths[0]).unwrap().file_attributes(),
        attributes
    );
    assert_eq!(u32::from_le_bytes(cida[..4].try_into().unwrap()), 2);
    assert_eq!(
        lists.load(Ordering::Relaxed),
        0,
        "Shell metadata must not expand source directories"
    );
    assert_eq!(
        opens.load(Ordering::Relaxed),
        0,
        "Shell metadata must not read source payloads"
    );
    let cida_file = temp.path().join("selection.cida");
    std::fs::write(&cida_file, &cida).unwrap();
    let target = temp.path().join("destination");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "windows::cloud::tests::shell_ops::external_operation",
            "--ignored",
            "--nocapture",
        ])
        .env("OINK_PRIVATE_SHELL_SOURCE", paths[0].parent().unwrap())
        .env("OINK_PRIVATE_SHELL_TARGET", &target)
        .env("OINK_PRIVATE_SHELL_CIDA_FILE", &cida_file)
        .env("OINK_PRIVATE_SHELL_MODE", "drop")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("empty-root Shell MOVE timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    eprintln!(
        "EMPTY_ROOT_MOVE left_source={} left_target={} right_target={}",
        delivery.file_path(0).exists(),
        target.join("left").exists(),
        target.join("right/data.bin").exists()
    );
    assert!(
        target.join("left").is_dir(),
        "Shell must deliver the independently selected empty root"
    );
    assert_eq!(
        std::fs::read(target.join("right/data.bin")).unwrap(),
        b"ordinary right contents"
    );
    // The Shell now moves the directory itself. Its final copy must stay usable
    // after the private provider disconnects, without any further callbacks.
    service.stop();
    assert_eq!(std::fs::read_dir(target.join("left")).unwrap().count(), 0);
    assert_eq!(
        std::fs::read(target.join("right/data.bin")).unwrap(),
        b"ordinary right contents"
    );
}
