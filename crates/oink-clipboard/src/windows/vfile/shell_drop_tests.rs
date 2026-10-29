//! Native Shell acceptance contract, using only private files and COM objects.
use super::*;

#[test]
#[ignore = "native Windows Shell probe; run on a reserved logged-in desktop"]
fn shell_folder_drop_receipt() {
    std::thread::spawn(|| {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::{
                Foundation::POINTL,
                System::{
                    Com::CoTaskMemFree,
                    DataExchange::RegisterClipboardFormatW,
                    Ole::{DROPEFFECT_COPY, DROPEFFECT_MOVE, IDropTarget, OleInitialize, OleUninitialize},
                    SystemServices::{MK_LBUTTON, MK_SHIFT},
                },
                UI::Shell::{IShellFolder, SHBindToObject, SHParseDisplayName},
            },
            core::{PCWSTR, w},
        };
        // SAFETY: all Shell objects are private to this apartment and temporary tree.
        unsafe { OleInitialize(None).unwrap() };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                // SAFETY: balances initialization on this thread.
                unsafe { OleUninitialize() };
            }
        }
        let _apartment = Apartment;
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, b"shell drop fixture").unwrap();
        let hdrop = super::super::files::build_hdrop(std::slice::from_ref(&path));
        let cida = super::super::files::shell_id_list(std::slice::from_ref(&path)).unwrap();
        let f = super::super::formats();
        // SAFETY: fixed NUL-terminated names, no clipboard contents are accessed.
        let (cida_id, logical_id, performed_id) = unsafe {
            (
                RegisterClipboardFormatW(w!("Shell IDList Array")),
                RegisterClipboardFormatW(w!("Logical Performed DropEffect")),
                RegisterClipboardFormatW(w!("Performed DropEffect")),
            )
        };
        struct Empty(Arc<std::sync::atomic::AtomicUsize>);
        impl FileContents for Empty {
            fn open(&self, _: usize) -> std::io::Result<crate::FileReader> {
                unreachable!()
            }
            fn cancel(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let cancels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (data, shared) = build(
            Spec {
                cloud: None,
                descriptors: super::super::files::descriptors(&[]),
                contents: Arc::new(Empty(cancels.clone())),
                cf_descriptor: f.file_descriptor,
                cf_contents: f.file_contents,
                fixed: vec![
                    (15, hdrop),
                    (cida_id, cida),
                    (f.preferred_drop_effect, 3u32.to_le_bytes().to_vec()),
                ],
            },
            false,
        );
        shared.host_drag_images();
        let name: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut pidl = std::ptr::null_mut();
        // SAFETY: live parsing name, writable output and valid Shell interfaces.
        let target: IDropTarget = unsafe {
            SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pidl, 0, None).unwrap();
            let folder: IShellFolder = SHBindToObject(None, pidl, None).unwrap();
            CoTaskMemFree(Some(pidl.cast()));
            folder.CreateViewObject(Default::default()).unwrap()
        };
        let mut effect = DROPEFFECT_COPY | DROPEFFECT_MOVE;
        // SAFETY: private Shell target, live data object, ordinary scalar values.
        unsafe {
            target
                .DragEnter(&data, MK_LBUTTON | MK_SHIFT, POINTL::default(), &mut effect)
                .unwrap();
            eprintln!("native folder enter effect={}", effect.0);
            assert_eq!(effect, DROPEFFECT_MOVE);
            target.Drop(&data, MK_SHIFT, POINTL::default(), &mut effect).unwrap();
        }
        let async_data: IDataObjectAsyncCapability = data.cast().unwrap();
        // SAFETY: read-only query on this apartment-owned data object.
        let pending = unsafe { async_data.InOperation() };
        eprintln!("native folder drop effect={} async={pending:?}", effect.0);
        for cf in [logical_id, performed_id] {
            let fe = FORMATETC {
                cfFormat: cf as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            // SAFETY: private object's HGLOBAL return is released exactly once.
            unsafe {
                match data.GetData(&fe) {
                    Ok(mut value) => {
                        eprintln!("receipt cf={cf} value={:?}", hglobal_bytes(value.u.hGlobal));
                        ReleaseStgMedium(&mut value);
                    }
                    Err(error) => eprintln!("receipt cf={cf} error={error}"),
                }
            }
        }
        eprintln!(
            "source_exists={} destination_exists={}",
            path.exists(),
            destination.join("sample.txt").exists()
        );
        let accepted = shared.drag_dropped(DRAGDROP_S_DROP, effect);
        assert!(
            accepted,
            "NONE must not cancel an accepted asynchronous/optimized Shell MOVE"
        );
        shared.drag_ended(accepted, effect == DROPEFFECT_MOVE);
        assert_eq!(cancels.load(Ordering::SeqCst), 0);
        // This fixed-path fixture starts with virtual-file serving disabled;
        // the deterministic hosted-files test covers its flushing flag. Here
        // verify native acceptance avoids cancellation and retains path data.
        for cf in [15, cida_id] {
            let format = FORMATETC {
                cfFormat: cf as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            // SAFETY: the private data object owns each returned medium until release.
            unsafe {
                let mut medium = data.GetData(&format).unwrap();
                ReleaseStgMedium(&mut medium);
            }
        }
        eprintln!("native Shell receipt accepted; CIDA/HDROP remain available after drop");
        // This direct Drop probe verifies acceptance/lifetime, not a completed
        // background file move. The installed-app e2e harness covers delivery.
    })
    .join()
    .unwrap();
}
