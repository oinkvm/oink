use super::*;
use crate::{FileContents, FileEntry, FileMetadata, content::native_metadata::NativeFileMetadata};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Bodies(AtomicUsize);
impl FileContents for Bodies {
    fn open(&self, index: usize) -> io::Result<crate::FileReader> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(io::Cursor::new(vec![index as u8 + 1; index + 4])))
    }
}
struct Consumer(Arc<AtomicUsize>);
impl Drop for Consumer {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Received {
    consumers: Arc<AtomicUsize>,
    nodes: Vec<FileNode>,
    paths: HashMap<NodeId, PathBuf>,
    native: NativeFileMetadata,
}
impl Namespace for Received {
    fn native_lease(&self) -> io::Result<namespace::NativeLease> {
        self.consumers.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Consumer(self.consumers.clone())))
    }
    fn requires_native_files(&self) -> bool {
        true
    }
    fn native_file_metadata(&self) -> NativeFileMetadata {
        self.native.clone()
    }
    fn roots(&self) -> Vec<FileNode> {
        self.nodes
            .iter()
            .filter(|node| node.parent.is_none())
            .cloned()
            .collect()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        namespace::page(
            node,
            1,
            &self
                .nodes
                .iter()
                .filter(|entry| entry.parent == Some(node))
                .cloned()
                .collect::<Vec<_>>(),
            cursor,
        )
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.nodes
            .iter()
            .find(|entry| entry.id == node)
            .cloned()
            .ok_or(io::ErrorKind::NotFound.into())
    }
    fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
        panic!("native output must read the received copy")
    }
    fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<crate::FileReader>> {
        Some((|| {
            let mut file = File::open(&self.paths[&node])?;
            file.seek(SeekFrom::Start(offset))?;
            Ok(Box::new(file) as crate::FileReader)
        })())
    }
    fn native_metadata(&self, node: NodeId) -> Option<io::Result<FileMetadata>> {
        Some(std::fs::metadata(&self.paths[&node]).map(|meta| FileMetadata {
            exec_bits: None,
            size: meta.len(),
            modified_ms: None,
        }))
    }
}

/// Real IDataObject/IStream calls, private temporary files and no system-slot
/// publication. The received-copy fixture is not a Cloud Files hydration test.
#[test]
fn original_wide_ansi_headers_and_indices_survive_source_receipts() {
    std::thread::spawn(|| {
        // SAFETY: this isolated thread owns every COM object until uninitialization.
        unsafe {
            windows::Win32::System::Ole::OleInitialize(None).unwrap();
        }
        let directory = tempfile::tempdir().unwrap();
        let formats = super::super::super::formats();
        let ansi_cf = format_id(&NativeFormat::WindowsRegistered("FileGroupDescriptor".into())).unwrap() as u32;
        let bodies = Arc::new(Bodies(AtomicUsize::new(0)));
        let entries: Vec<_> = ["z.bin", "folder/a.bin"]
            .into_iter()
            .enumerate()
            .map(|(index, path)| FileEntry {
                path: path.into(),
                is_dir: false,
                size: index as u64 + 4,
                modified_ms: None,
                local_path: None,
            })
            .collect();
        let mut descriptors = super::super::super::files::descriptors(&entries);
        let mut ansi = 2u32.to_le_bytes().to_vec();
        for (index, alias) in [b"z-alias.bin\0".as_slice(), b"folder\\a-alias.bin\0"]
            .into_iter()
            .enumerate()
        {
            let start = 4 + index * 592;
            let header = &mut descriptors.bytes[start..start + 72];
            // All timestamp/attribute bits are deliberately present, including
            // sub-millisecond FILETIME ticks that FileNode cannot represent.
            header[..4].copy_from_slice(&0x8000_007fu32.to_le_bytes());
            header[4..36].fill(index as u8 + 9);
            header[36..40].copy_from_slice(&0x21u32.to_le_bytes());
            for (slot, ticks) in [
                (40, 130_000_000_000_000_003u64),
                (48, 130_000_000_000_000_005),
                (56, 130_000_000_000_000_007),
            ] {
                header[slot..slot + 8].copy_from_slice(&(ticks + index as u64).to_le_bytes());
            }
            let mut narrow = header.to_vec();
            narrow[..4].copy_from_slice(&0x0000_007fu32.to_le_bytes());
            narrow.resize(332, 0);
            narrow[72..72 + alias.len()].copy_from_slice(alias);
            ansi.extend(narrow);
        }
        let expected_wide = descriptors.bytes.clone();
        let expected_ansi = ansi.clone();
        let (original, _) = super::super::super::vfile::build(
            super::super::super::vfile::Spec {
                cloud: None,
                descriptors,
                contents: bodies.clone(),
                fixed: vec![(ansi_cf, ansi)],
                cf_descriptor: formats.file_descriptor,
                cf_contents: formats.file_contents,
            },
            true,
        );
        let source = Virtual::metadata(&original).unwrap().unwrap();
        assert_eq!(source.native_file_metadata().windows.len(), 2);
        assert_eq!(bodies.0.load(Ordering::Acquire), 0, "metadata does not read contents");
        let offer = crate::content::tests::fixture().inventory().id;
        let mut inventory = super::super::inventory(&original, offer).unwrap();
        source.snapshot(&original, directory.path()).unwrap();
        let peer = DeviceId([3; 32]);
        let grants = content::grants::SourceGrants::load(&directory.path().join("journal"), [(peer, [4; 32])]).unwrap();
        let roots: Vec<_> = source.roots().iter().map(|node| node.id).collect();
        let token = grants.issue(peer, source.as_ref(), &roots).unwrap();
        let files = grants.open(peer, token).unwrap();
        let mut nodes = files.roots();
        for node in nodes
            .clone()
            .into_iter()
            .filter(|node| node.kind == NodeKind::Directory)
        {
            nodes.extend(files.list(node.id, None).unwrap().entries);
        }
        let mut paths = HashMap::new();
        for node in nodes.iter().filter(|node| node.kind == NodeKind::File) {
            let path = directory.path().join(format!("received-{}", node.id.0));
            let mut output = File::create(&path).unwrap();
            io::copy(&mut files.open(node.id, node.version, 0).unwrap(), &mut output).unwrap();
            output.sync_all().unwrap();
            paths.insert(node.id, path);
        }
        let consumers = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Received {
            consumers: consumers.clone(),
            nodes,
            paths,
            native: files.native_file_metadata(),
        });
        for receipt in grants.fulfilled_roots(peer, token).unwrap() {
            grants.sent(peer, token, receipt.node).unwrap();
        }
        source.cancel();
        drop((original, source, files));
        assert_eq!(bodies.0.load(Ordering::Acquire), 2, "one read per original native file");
        inventory.roots = received.roots();
        let batch = Batch::create(directory.path(), StoreKind::SourceSnapshot, BatchId::random().unwrap()).unwrap();
        let provider =
            Snapshot::from_parts(inventory.clone(), batch, Arc::new(super::super::WindowsTranslator)).unwrap();
        let target = super::super::target::build(NativeSpec {
            inventory,
            contents: provider,
            files: vec![],
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        })
        .unwrap();
        let contents = namespace::Files::new(received, crate::sync::files::TargetOs::Windows).unwrap();
        let (target, _) = super::super::super::vfile::build_inner(
            super::super::super::vfile::Spec {
                cloud: None,
                descriptors: super::super::super::files::descriptors(&[]),
                contents,
                fixed: vec![],
                cf_descriptor: formats.file_descriptor,
                cf_contents: formats.file_contents,
            },
            true,
            Some(target),
        );
        let get = |cf: u32| {
            let format = FORMATETC {
                cfFormat: cf as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            // SAFETY: this STA owns the object and always releases its HGLOBAL.
            unsafe {
                target.QueryGetData(&format).ok().unwrap();
                let mut value = target.GetData(&format).unwrap();
                let bytes = medium::global(value.u.hGlobal).unwrap();
                ReleaseStgMedium(&mut value);
                bytes
            }
        };
        let wide = get(formats.file_descriptor);
        assert_eq!(
            &wide[..expected_wide.len()],
            expected_wide,
            "implicit folder is not an extra descriptor"
        );
        let ansi = get(ansi_cf);
        assert_eq!(
            &ansi[..expected_ansi.len()],
            expected_ansi,
            "original ANSI aliases and valid fields survive"
        );
        for index in 0..2 {
            let format = FORMATETC {
                cfFormat: formats.file_contents as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: index,
                tymed: TYMED_ISTREAM.0 as u32,
                ..Default::default()
            };
            // SAFETY: apartment-owned returned stream and bounded output buffer.
            unsafe {
                let mut value = target.GetData(&format).unwrap();
                assert_eq!(
                    consumers.load(Ordering::SeqCst),
                    1,
                    "unread OLE stream must pin its received copy"
                );
                let stream = value.u.pstm.as_ref().unwrap();
                let mut bytes = [0u8; 16];
                let mut count = 0;
                stream
                    .Read(bytes.as_mut_ptr().cast(), 16, Some(&mut count))
                    .ok()
                    .unwrap();
                assert_eq!(&bytes[..count as usize], vec![index as u8 + 1; index as usize + 4]);
                // Keep the returned stream alive after releasing the STGMEDIUM.
                let retained = stream.clone();
                ReleaseStgMedium(&mut value);
                assert_eq!(consumers.load(Ordering::SeqCst), 1);
                drop(retained);
            }
            // Marshaled final release may be queued on the MTA.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while consumers.load(Ordering::SeqCst) != 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(consumers.load(Ordering::SeqCst), 0);
        }
        drop(target);
        // SAFETY: all apartment-owned objects have been released above.
        unsafe {
            windows::Win32::System::Ole::OleUninitialize();
        }
    })
    .join()
    .unwrap();
}
