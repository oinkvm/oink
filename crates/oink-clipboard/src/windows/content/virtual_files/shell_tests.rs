//! Windows compressed-folder integration. Uses only a private ZIP and COM objects.
use super::*;

#[test]
#[ignore = "native Windows Shell probe; run on a reserved logged-in desktop"]
fn compressed_folder_virtual_source() {
    std::thread::spawn(|| {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::{
                System::Ole::{OleInitialize, OleUninitialize},
                UI::Shell::{IShellFolder, SHBindToParent, SHParseDisplayName},
            },
            core::PCWSTR,
        };
        // SAFETY: all apartment-owned interfaces are released before uninitialization.
        unsafe { OleInitialize(None).unwrap() };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                // SAFETY: balances this thread's successful initialization.
                unsafe { OleUninitialize() };
            }
        }
        let _apartment = Apartment;
        let temp = tempfile::tempdir().unwrap();
        // Stored ZIP containing sample.txt = "zip source fixture". No external tool,
        // user file, clipboard slot, Explorer window or network is involved.
        let zip = [
            80, 75, 3, 4, 20, 0, 0, 0, 0, 0, 0, 0, 33, 92, 163, 194, 247, 17, 18, 0, 0, 0, 18, 0, 0, 0, 10, 0, 0, 0,
            115, 97, 109, 112, 108, 101, 46, 116, 120, 116, 122, 105, 112, 32, 115, 111, 117, 114, 99, 101, 32, 102,
            105, 120, 116, 117, 114, 101, 80, 75, 1, 2, 20, 3, 20, 0, 0, 0, 0, 0, 0, 0, 33, 92, 163, 194, 247, 17, 18,
            0, 0, 0, 18, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 1, 0, 0, 0, 0, 115, 97, 109, 112, 108, 101,
            46, 116, 120, 116, 80, 75, 5, 6, 0, 0, 0, 0, 1, 0, 1, 0, 56, 0, 0, 0, 58, 0, 0, 0, 0, 0,
        ];
        let path = temp.path().join("source.zip");
        std::fs::write(&path, zip).unwrap();
        let selected = path.join("sample.txt");
        let name: Vec<u16> = selected.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut pidl = std::ptr::null_mut();
        let mut child = std::ptr::null_mut();
        // SAFETY: owned NUL-terminated parsing name and writable PIDL outputs.
        let data: IDataObject = unsafe {
            SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pidl, 0, None).unwrap();
            let folder: IShellFolder = SHBindToParent(pidl, Some(&mut child)).unwrap();
            let data = folder.GetUIObjectOf(Default::default(), &[child], None).unwrap();
            CoTaskMemFree(Some(pidl.cast()));
            data
        };
        let id = crate::content::tests::fixture().inventory().id;
        let inventory = super::super::inventory(&data, id).unwrap();
        assert!(inventory.items.iter().any(|item| item.virtual_files));
        for name in ["FileGroupDescriptorW", "FileGroupDescriptor"] {
            let cf = format_id(&NativeFormat::WindowsRegistered(name.into())).unwrap();
            let format = FORMATETC {
                cfFormat: cf,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            // SAFETY: metadata-only capability query on our private Shell object.
            eprintln!("{name} query={:?}", unsafe { data.QueryGetData(&format) });
        }
        let files = Virtual::metadata(&data)
            .expect("compressed-folder metadata")
            .expect("virtual namespace");
        let roots = files.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].size, ContentSize::Known(18));
        assert!(
            files.state.lock().unwrap().is_none(),
            "metadata must not fulfil FileContents"
        );
        let source = content::Deferred::new(inventory, Arc::new(super::super::WindowsTranslator)).unwrap();
        assert!(source.attach_files(files.clone()));
        assert!(
            !content::items(&source.inventory()).is_empty(),
            "native catcher can emit Detected"
        );
        files.snapshot(&data, temp.path()).unwrap();
        let node = files.select(roots[0].id).unwrap();
        let mut body = Vec::new();
        files
            .open(node.id, node.version, 0)
            .unwrap()
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, b"zip source fixture");
        eprintln!("compressed-folder metadata and native FileContents verified");
    })
    .join()
    .unwrap();
}
