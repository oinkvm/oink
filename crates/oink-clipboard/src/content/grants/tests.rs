use super::*;
use crate::content::Provider;
use crate::model::NodeKind;
use namespace::Local;
const PEER: DeviceId = DeviceId([1; 32]);

pub(super) struct OneShot(pub(super) Arc<Local>);
impl Namespace for OneShot {
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.roots().iter().map(|root| root.id).collect()
    }
    fn roots(&self) -> Vec<FileNode> {
        self.0.roots()
    }
    fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.0.list(id, cursor)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        self.0.select(id)
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.0.open(id, version, offset)
    }
}

#[test]
fn one_shot_hover_cannot_read_or_receipt_and_cancel_cleans_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"").unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), path)]).unwrap());
    let store_dir = dir.path().join("journal");
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    let holder = oink_foundation::content::Holder { device: PEER, epoch: 1 };
    let token = store
        .issue_for(PEER, Some(offer), &source, &[NodeId(1)], Demand::Hover(holder))
        .unwrap();
    let receipt = store.fulfilled_roots(PEER, token).unwrap().remove(0);
    let snapshot = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
    let files = store.open(PEER, token).unwrap();
    let entry = files.select(NodeId(1)).unwrap();
    assert_eq!(entry.size, crate::model::ContentSize::Known(0));
    assert!(files.open(entry.id, entry.version, 0).is_err());
    assert!(store.sent(PEER, token, receipt.node).is_err());
    store.discard_pending(offer).unwrap();
    assert!(!snapshot.exists());
    assert!(files.select(NodeId(1)).is_err());
}

#[test]
fn one_shot_replacement_is_never_adopted_and_membership_removal_cleans_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"payload").unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), path)]).unwrap());
    let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, &source, &[NodeId(1)]).unwrap();
    let snapshot = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
    let files = store.open(PEER, token).unwrap();
    fs::rename(snapshot.join("root"), snapshot.join("moved")).unwrap();
    fs::write(snapshot.join("root"), b"replacement").unwrap();
    assert!(files.select(NodeId(1)).is_err());
    store.reconcile_members([]).unwrap();
    assert!(!snapshot.exists());
}

#[test]
fn mixed_real_and_one_shot_roots_keep_distinct_lifetimes() {
    struct Mixed(Arc<Local>);
    impl Namespace for Mixed {
        fn roots(&self) -> Vec<FileNode> {
            self.0.roots()
        }
        fn source_roots(&self) -> io::Result<Vec<SourceRoot>> {
            self.0.source_roots()
        }
        fn one_shot_roots(&self) -> Vec<NodeId> {
            vec![NodeId(2)]
        }
        fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
            self.0.list(id, cursor)
        }
        fn select(&self, id: NodeId) -> io::Result<FileNode> {
            self.0.select(id)
        }
        fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
            self.0.open(id, version, offset)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("document");
    let attachment = dir.path().join("attachment");
    fs::write(&real, b"old").unwrap();
    fs::write(&attachment, b"one shot").unwrap();
    let source = Mixed(Local::new(vec![(NodeId(1), real.clone()), (NodeId(2), attachment.clone())]).unwrap());
    let store_dir = dir.path().join("journal");
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, &source, &[NodeId(1), NodeId(2)]).unwrap();
    let receipts = store.fulfilled_roots(PEER, token).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].node, NodeId(2));
    fs::write(&real, b"edited real document").unwrap();
    fs::remove_file(&attachment).unwrap();
    drop((source, store));
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    let files = store.open(PEER, token).unwrap();
    let entry = files.select(NodeId(1)).unwrap();
    let mut bytes = Vec::new();
    files
        .open(entry.id, entry.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"edited real document");
    assert!(files.select(NodeId(2)).is_err(), "restart discards only the F1 root");
    assert_eq!(fs::read(real).unwrap(), b"edited real document");
}

#[test]
fn truncated_one_shot_never_publishes_a_grant_or_leaves_partial_bytes() {
    struct Truncated(OneShot);
    impl Namespace for Truncated {
        fn roots(&self) -> Vec<FileNode> {
            self.0.roots()
        }
        fn one_shot_roots(&self) -> Vec<NodeId> {
            self.0.one_shot_roots()
        }
        fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
            self.0.list(id, cursor)
        }
        fn select(&self, id: NodeId) -> io::Result<FileNode> {
            self.0.select(id)
        }
        fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<FileReader> {
            Ok(Box::new(io::Cursor::new(b"x")))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attachment");
    fs::write(&path, b"payload").unwrap();
    let source = Truncated(OneShot(Local::new(vec![(NodeId(1), path)]).unwrap()));
    let store_dir = dir.path().join("journal");
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    assert!(store.issue(PEER, &source, &[NodeId(1)]).is_err());
    assert!(store.0.state.lock().unwrap().journal.grants.is_empty());
    assert_eq!(fs::read_dir(store_dir.join("fulfilled-sources-v1")).unwrap().count(), 0);
}

#[test]
fn durable_grant_follows_original_identity_after_rename_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("original");
    let moved = dir.path().join("renamed");
    fs::write(&old, b"original data").unwrap();
    let source = Local::clipboard(vec![(NodeId(7), old.clone())]).unwrap();
    let store_dir = dir.path().join("journal");
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(7)]).unwrap();
    drop(source);
    drop(store);
    fs::rename(&old, &moved).unwrap();
    fs::write(&old, b"replacement must never be read").unwrap();
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    let selected = namespace.select(NodeId(7)).unwrap();
    let mut bytes = Vec::new();
    namespace
        .open(NodeId(7), selected.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"original data");
}

#[test]
fn device_binding_and_removal_revoke_existing_readers_and_survive_rejoin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"bytes").unwrap();
    let source = Local::new(vec![(NodeId(1), path)]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32]), (DeviceId([2; 32]), [2; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    assert!(store.open(DeviceId([2; 32]), token).is_err());
    let namespace = store.open(PEER, token).unwrap();
    let selected = namespace.select(NodeId(1)).unwrap();
    let mut reader = namespace.open(NodeId(1), selected.version, 0).unwrap();
    store.reconcile_members([]).unwrap();
    assert!(reader.read(&mut [0]).is_err());
    drop(store);
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    assert!(store.open(PEER, token).is_err());
    assert_eq!(format!("{token:?}"), "LinkToken([redacted])");
}

#[test]
fn directory_nodes_persist_but_cannot_escape_the_offered_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("child"), b"child").unwrap();
    let source = Local::new(vec![(NodeId(10), root.clone())]).unwrap();
    let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    assert!(store.issue(PEER, source.as_ref(), &[NodeId(11)]).is_err());
    let token = store.issue(PEER, source.as_ref(), &[NodeId(10)]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    let first = namespace.list(NodeId(10), None).unwrap();
    let id = first.entries[0].id;
    drop(namespace);
    drop(store);
    let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    assert_eq!(namespace.list(NodeId(10), None).unwrap(), first);
    fs::rename(root.join("child"), dir.path().join("outside")).unwrap();
    assert_eq!(
        super::super::reason(&namespace.select(id).unwrap_err()),
        Unavailable::Permission
    );
    assert_eq!(
        super::super::reason(
            &namespace
                .list(
                    NodeId(10),
                    Some(DirectoryCursor {
                        version: first.version,
                        offset: 0
                    })
                )
                .unwrap_err()
        ),
        Unavailable::Superseded
    );
    assert!(namespace.list(NodeId(10), None).unwrap().entries.is_empty());
    assert!(namespace.select(id).is_err());
}

#[test]
fn a_read_is_one_version_and_cancellation_does_not_revoke_durable_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"one").unwrap();
    let source = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    let first = namespace.select(NodeId(1)).unwrap();
    let mut reader = namespace.open(NodeId(1), first.version, 0).unwrap();
    let mut bytes = [0; 3];
    reader.read_exact(&mut bytes).unwrap();
    fs::write(&path, b"a new version").unwrap();
    assert_eq!(
        super::super::reason(&reader.read(&mut bytes).unwrap_err()),
        Unavailable::Superseded
    );
    let second = namespace.select(NodeId(1)).unwrap();
    assert_ne!(first.version, second.version);
    assert!(namespace.open(NodeId(1), first.version, 0).is_err());
    namespace.cancel();
    assert!(namespace.select(NodeId(1)).is_err());
    let namespace = store.open(PEER, token).unwrap();
    assert_eq!(namespace.select(NodeId(1)).unwrap(), second);
}

#[test]
fn corrupted_local_authority_is_rejected_without_panics_or_partial_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"bytes").unwrap();
    let source = Local::new(vec![(NodeId(1), path)]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let mut journal = store.0.state.lock().unwrap().journal.clone();
    journal.grants[0].roots = vec![999];
    assert!(store.persist(&journal).is_err());
    fs::write(&store.0.path, serde_json::to_vec(&journal).unwrap()).unwrap();
    assert!(SourceGrants::load(dir.path(), [(PEER, [1; 32])]).is_err());
}

#[cfg(unix)]
#[test]
fn selected_and_nested_external_links_follow_user_selection() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(dir.path().join("outside"), b"private").unwrap();
    symlink(dir.path().join("outside"), root.join("link")).unwrap();
    assert_eq!(
        Local::new(vec![(NodeId(1), root.join("link"))]).unwrap().roots()[0].kind,
        crate::model::NodeKind::File
    );
    let source = Local::new(vec![(NodeId(1), root)]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    assert_eq!(
        store
            .open(PEER, token)
            .unwrap()
            .list(NodeId(1), None)
            .unwrap()
            .entries
            .len(),
        1
    );
}

#[test]
fn size_or_mtime_change_invalidates_an_open_source_version() {
    for grow in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"before").unwrap();
        let source = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
        let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
        let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
        let namespace = store.open(PEER, token).unwrap();
        let selected = namespace.select(NodeId(1)).unwrap();
        let mut reader = namespace.open(NodeId(1), selected.version, 0).unwrap();
        reader.read_exact(&mut [0; 2]).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let bytes: &[u8] = if grow { b"longer content" } else { b"AFTER!" };
        fs::write(&path, bytes).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(if grow {
                modified
            } else {
                modified + std::time::Duration::from_secs(2)
            })
            .unwrap();
        assert_eq!(
            super::super::reason(&reader.read(&mut [0; 6]).unwrap_err()),
            Unavailable::Superseded
        );
        let current = namespace.select(NodeId(1)).unwrap();
        assert_ne!(current.version, selected.version);
        let mut complete = Vec::new();
        namespace
            .open(NodeId(1), current.version, 0)
            .unwrap()
            .read_to_end(&mut complete)
            .unwrap();
        assert_eq!(complete, bytes);
    }
}

#[test]
fn selection_persists_current_metadata_when_capture_already_has_current_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    fs::write(&path, b"content").unwrap();
    let source = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
    let announced = source.roots()[0].version;
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified + std::time::Duration::from_secs(2))
        .unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let selected = store.open(PEER, token).unwrap().select(NodeId(1)).unwrap();
    assert_ne!(selected.version, announced);
    let restored = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    assert_eq!(restored.open(PEER, token).unwrap().roots()[0], selected);
}

#[cfg(target_os = "macos")]
#[test]
fn source_reads_and_xattr_changes_preserve_the_open_content_version() {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    fs::write(&path, b"original").unwrap();
    let original = local::inspect(&path).unwrap();
    let before = fs::metadata(&path).unwrap();
    let source = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    let selected = namespace.select(NodeId(1)).unwrap();
    let mut reader = namespace.open(NodeId(1), selected.version, 0).unwrap();
    let mut prefix = [0; 3];
    reader.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"ori");
    assert!(
        local::inspect(&path).unwrap() == original,
        "grant/bookmark/read changed source metadata"
    );

    let native = CString::new(path.as_os_str().as_bytes()).unwrap();
    // Only the private fixture changes. This models a metadata writer racing a
    // real open reader, without asserting who changed the user's source file.
    // SAFETY: terminated path/name and the one-byte value stay live for setxattr.
    unsafe {
        assert_eq!(
            libc::setxattr(
                native.as_ptr(),
                c"com.hachiwii.oink.fixture".as_ptr(),
                b"x".as_ptr().cast(),
                1,
                0,
                0
            ),
            0
        );
    }
    let changed = local::inspect(&path).unwrap();
    assert_eq!(changed.identity, original.identity);
    assert_eq!(changed.size, original.size);
    assert_eq!(changed.modified, original.modified);
    assert_eq!(changed.created, original.created);
    let after = fs::metadata(&path).unwrap();
    assert_ne!(
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec())
    );
    assert_eq!(changed.version(), original.version());
    let mut suffix = Vec::new();
    reader.read_to_end(&mut suffix).unwrap();
    assert_eq!(suffix, b"ginal");
    let current = namespace.select(NodeId(1)).unwrap();
    assert_eq!(current.version, selected.version);
    let mut bytes = Vec::new();
    namespace
        .open(NodeId(1), current.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"original");
    assert!(
        local::inspect(&path).unwrap() == changed,
        "refresh/read changed source metadata"
    );
}

#[cfg(windows)]
#[test]
fn windows_change_time_preserves_the_open_content_version() {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx, SetFileInformationByHandle,
        },
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    fs::write(&path, b"original").unwrap();
    let original = local::inspect(&path).unwrap();
    let source = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    let selected = namespace.select(NodeId(1)).unwrap();
    let mut reader = namespace.open(NodeId(1), selected.version, 0).unwrap();
    reader.read_exact(&mut [0; 3]).unwrap();
    let file = fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let handle = HANDLE(file.as_raw_handle());
    let read_basic = || {
        let mut info = FILE_BASIC_INFO::default();
        // SAFETY: the borrowed live file and writable, sized record survive the call.
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&mut info as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of_val(&info) as u32,
            )
            .unwrap();
        }
        info
    };
    let before = read_basic();
    let changed = FILE_BASIC_INFO {
        ChangeTime: before.ChangeTime + 10_000_000,
        ..Default::default()
    };
    // SAFETY: live writable handle and correctly sized initialized metadata record;
    // zero fields leave size, creation time, mtime and attributes unchanged.
    unsafe {
        SetFileInformationByHandle(
            handle,
            FileBasicInfo,
            (&changed as *const FILE_BASIC_INFO).cast(),
            std::mem::size_of_val(&changed) as u32,
        )
        .unwrap();
    }
    assert_ne!(read_basic().ChangeTime, before.ChangeTime);
    assert!(local::inspect(&path).unwrap() == original);
    assert_eq!(namespace.select(NodeId(1)).unwrap().version, selected.version);
    let mut suffix = Vec::new();
    reader.read_to_end(&mut suffix).unwrap();
    assert_eq!(suffix, b"ginal");
}

#[test]
fn atomic_save_rebinds_only_after_original_identity_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"before").unwrap();
    let source = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    let before = namespace.select(NodeId(1)).unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"replacement content").unwrap();
    let after = namespace.select(NodeId(1)).unwrap();
    assert_ne!(before.version, after.version);
    let mut bytes = Vec::new();
    namespace
        .open(NodeId(1), after.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"replacement content");
    drop(namespace);
    drop(store);
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    assert_eq!(
        store.open(PEER, token).unwrap().select(NodeId(1)).unwrap().version,
        after.version
    );
}

#[test]
fn a_new_membership_cannot_resurrect_a_journal_left_before_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"bytes").unwrap();
    let source = Local::new(vec![(NodeId(1), path)]).unwrap();
    let store = SourceGrants::load(dir.path(), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    drop(store);
    // The verified group now contains a new signed add record for the same key.
    // Even if an old source journal survived a failed removal write, its grant
    // belongs to the old membership and must not be admitted on restart.
    let store = SourceGrants::load(dir.path(), [(PEER, [2; 32])]).unwrap();
    assert!(store.open(PEER, token).is_err());
    assert!(store.0.state.lock().unwrap().journal.grants.is_empty());
}

#[test]
fn hover_grants_allow_metadata_but_require_the_same_drop_before_bytes() {
    use crate::{content::Provider, model::Demand};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"payload").unwrap();
    let source = Local::new(vec![(NodeId(1), path)]).unwrap();
    let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    let holder = oink_foundation::content::Holder {
        device: PEER,
        epoch: 10,
    };
    let token = store
        .issue_for(PEER, Some(offer), source.as_ref(), &[NodeId(1)], Demand::Hover(holder))
        .unwrap();
    let namespace = store.open(PEER, token).unwrap();
    let selected = namespace.select(NodeId(1)).unwrap();
    assert!(namespace.open(NodeId(1), selected.version, 0).is_err());
    let other = oink_foundation::content::Holder { epoch: 11, ..holder };
    let other_token = store
        .issue_for(PEER, Some(offer), source.as_ref(), &[NodeId(1)], Demand::Drop(other))
        .unwrap();
    assert_ne!(other_token, token);
    assert!(namespace.open(NodeId(1), selected.version, 0).is_err());
    assert_eq!(
        token,
        store
            .issue_for(PEER, Some(offer), source.as_ref(), &[NodeId(1)], Demand::Drop(holder))
            .unwrap()
    );
    let mut bytes = Vec::new();
    namespace
        .open(NodeId(1), selected.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"payload");
    store.discard_pending(offer).unwrap();
    drop(store);
    let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    assert!(
        store.open(PEER, token).is_ok(),
        "activated grant survives cancellation/restart"
    );
}

#[test]
fn provisional_grants_are_discarded_on_cancel_and_source_restart() {
    use crate::{content::Provider, model::Demand};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"payload").unwrap();
    let source = Local::new(vec![(NodeId(1), path)]).unwrap();
    let journal = dir.path().join("journal");
    let store = SourceGrants::load(&journal, [(PEER, [1; 32])]).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    let holder = oink_foundation::content::Holder { device: PEER, epoch: 1 };
    let token = store
        .issue_for(PEER, Some(offer), source.as_ref(), &[NodeId(1)], Demand::Hover(holder))
        .unwrap();
    let namespace = store.open(PEER, token).unwrap();
    store.discard_pending(offer).unwrap();
    assert!(namespace.select(NodeId(1)).is_err());
    let token = store
        .issue_for(PEER, Some(offer), source.as_ref(), &[NodeId(1)], Demand::Hover(holder))
        .unwrap();
    drop(namespace);
    drop(store);
    let store = SourceGrants::load(&journal, [(PEER, [1; 32])]).unwrap();
    assert!(
        store.open(PEER, token).is_err(),
        "restart discards unactivated authority"
    );
}

#[test]
fn failed_activation_commit_does_not_enable_body_reads() {
    use crate::{content::Provider, model::Demand};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"payload").unwrap();
    let source = Local::new(vec![(NodeId(1), path)]).unwrap();
    let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    let holder = oink_foundation::content::Holder { device: PEER, epoch: 1 };
    let token = store
        .issue_for(PEER, Some(offer), source.as_ref(), &[NodeId(1)], Demand::Hover(holder))
        .unwrap();
    let files = store.open(PEER, token).unwrap();
    let selected = files.select(NodeId(1)).unwrap();
    fs::remove_file(&store.0.path).unwrap();
    fs::create_dir(&store.0.path).unwrap();
    assert!(
        store
            .issue_for(PEER, Some(offer), source.as_ref(), &[NodeId(1)], Demand::Drop(holder))
            .is_err()
    );
    assert!(files.open(NodeId(1), selected.version, 0).is_err());
}

#[path = "promises_tests.rs"]
mod promises;

#[test]
fn fresh_folder_grant_reads_rewritten_ds_store_and_large_file_on_second_drag() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("folder");
    fs::create_dir(&folder).unwrap();
    let large = vec![0x5a; 179_200];
    fs::write(folder.join("large.bin"), &large).unwrap();
    let store = SourceGrants::load(&temp.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let mut tokens = Vec::new();
    for bytes in [vec![0x11; 6148], vec![0x22; 8196]] {
        fs::write(folder.join(".DS_Store"), &bytes).unwrap();
        let source = Local::new(vec![(NodeId(1), folder.clone())]).unwrap();
        let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
        assert!(!tokens.contains(&token));
        tokens.push(token);
        let grant = store.open(PEER, token).unwrap();
        let page = grant.list(NodeId(1), None).unwrap();
        for (name, expected) in [(".DS_Store", &bytes), ("large.bin", &large)] {
            let node = page.entries.iter().find(|node| node.name == name).unwrap();
            let entry = grant.select(node.id).unwrap();
            let mut received = Vec::new();
            grant
                .open(node.id, entry.version, 0)
                .unwrap()
                .read_to_end(&mut received)
                .unwrap();
            assert_eq!(&received, expected);
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn symlink_grant_views_are_capability_gated_durable_and_digest_bound() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("bundle");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file"), b"payload").unwrap();
    symlink("file", root.join("link")).unwrap();
    symlink("../outside", root.join("escape")).unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), root)]).unwrap());
    let journal = temp.path().join("grants");
    let other = DeviceId([2; 32]);
    let store = SourceGrants::load(&journal, [(PEER, [1; 32]), (other, [2; 32])]).unwrap();
    let baseline = store.list_for_peer(PEER, &source, NodeId(1), None).unwrap();
    assert_eq!(baseline.entries.len(), 1);
    store.set_peer_safe_symlinks(PEER, true);
    assert_eq!(
        store
            .list_for_peer(PEER, &source, NodeId(1), None)
            .unwrap()
            .entries
            .len(),
        2
    );
    let capable = store.issue(PEER, &source, &[NodeId(1)]).unwrap();
    let legacy = store.issue(other, &source, &[NodeId(1)]).unwrap();

    let capable = store.open(PEER, capable).unwrap();
    let legacy = store.open(other, legacy).unwrap();
    let links = capable.list(NodeId(1), None).unwrap();
    assert_eq!(links.entries.len(), 2);
    assert_eq!(legacy.list(NodeId(1), None).unwrap().entries.len(), 1);
    let link = links
        .entries
        .iter()
        .find(|node| node.kind.symlink_target().is_some())
        .unwrap();
    assert!(capable.select(link.id).is_err());
    assert!(capable.open(link.id, link.version, 0).is_err());
    assert!(legacy.select(link.id).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn executable_grants_are_independent_of_symlink_capability_and_bind_receipt_views() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("executable");
    fs::write(&path, b"payload").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o4755)).unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), path)]).unwrap());
    let peer2 = DeviceId([2; 32]);
    let journal = dir.path().join("journal");
    let store = SourceGrants::load(&journal, [(PEER, [1; 32]), (peer2, [2; 32])]).unwrap();
    store.set_peer_safe_symlinks(PEER, true);
    let legacy = store.issue(PEER, &source, &[NodeId(1)]).unwrap();
    assert_eq!(
        store.open(PEER, legacy).unwrap().select(NodeId(1)).unwrap().exec_bits,
        None
    );
    store.set_peer_exec_bits(peer2, true);
    let capable = store.issue(peer2, &source, &[NodeId(1)]).unwrap();
    let namespace = store.open(peer2, capable).unwrap();
    assert_eq!(namespace.roots()[0].exec_bits, Some(0o111));
    assert_eq!(namespace.select(NodeId(1)).unwrap().exec_bits, Some(0o111));
    drop(namespace);
    assert_eq!(
        store.open(peer2, capable).unwrap().select(NodeId(1)).unwrap().exec_bits,
        Some(0o111)
    );
}

#[cfg(windows)]
#[test]
fn windows_source_does_not_enable_posix_execution_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    fs::write(&path, b"payload").unwrap();
    let source = Local::new(vec![(NodeId(1), path)]).unwrap();
    let store = SourceGrants::load(&temp.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    store.set_peer_exec_bits(PEER, true);
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let namespace = store.open(PEER, token).unwrap();
    assert_eq!(namespace.roots()[0].exec_bits, None);
    assert_eq!(namespace.select(NodeId(1)).unwrap().exec_bits, None);
}

#[cfg(target_os = "macos")]
#[test]
fn batch_grants_keep_cross_roots_follow_external_and_pin_target_identity() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    for name in ["A", "B", "outside"] {
        fs::create_dir(temp.path().join(name)).unwrap();
    }
    fs::write(temp.path().join("B/file"), b"internal").unwrap();
    fs::write(temp.path().join("outside/data"), b"external").unwrap();
    symlink("../B/file", temp.path().join("A/internal")).unwrap();
    symlink("../outside", temp.path().join("A/follow")).unwrap();
    symlink(temp.path().join("B/file"), temp.path().join("top-link")).unwrap();
    let source = Local::new(vec![
        (NodeId(1), temp.path().join("A")),
        (NodeId(2), temp.path().join("B")),
        (NodeId(3), temp.path().join("top-link")),
    ])
    .unwrap();
    let journal = temp.path().join("journal");
    let store = SourceGrants::load(&journal, [(PEER, [1; 32])]).unwrap();
    store.set_peer_safe_symlinks(PEER, true);
    let token = store
        .issue(PEER, source.as_ref(), &[NodeId(1), NodeId(2), NodeId(3)])
        .unwrap();
    let grant = store.open(PEER, token).unwrap();
    assert_eq!(grant.roots()[2].kind, NodeKind::Symlink("B/file".into()));
    let page = grant.list(NodeId(1), None).unwrap();
    assert_eq!(
        page.entries.iter().find(|n| n.name == "internal").unwrap().kind,
        NodeKind::Symlink("../B/file".into())
    );
    let follow = page.entries.iter().find(|n| n.name == "follow").unwrap();
    let data = grant.list(follow.id, None).unwrap().entries.remove(0);
    let selected = grant.select(data.id).unwrap();
    let mut reader = grant.open(data.id, selected.version, 0).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"external");
    drop(grant);
    drop(store);
    let store = SourceGrants::load(&journal, [(PEER, [1; 32])]).unwrap();
    store.set_peer_safe_symlinks(PEER, true);
    let grant = store.open(PEER, token).unwrap();
    fs::create_dir(temp.path().join("replacement")).unwrap();
    fs::rename(temp.path().join("outside"), temp.path().join("old")).unwrap();
    fs::rename(temp.path().join("replacement"), temp.path().join("outside")).unwrap();
    assert_eq!(
        super::super::reason(&grant.select(data.id).unwrap_err()),
        Unavailable::Superseded
    );
    assert_eq!(
        super::super::reason(&reader.read(&mut [0; 1]).unwrap_err()),
        Unavailable::Superseded
    );
}

#[cfg(target_os = "macos")]
#[test]
fn legacy_receiver_skips_in_batch_links_but_receives_followed_external_files() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("root")).unwrap();
    fs::write(temp.path().join("root/file"), b"inside").unwrap();
    fs::write(temp.path().join("outside"), b"outside").unwrap();
    symlink("file", temp.path().join("root/inside-link")).unwrap();
    symlink("../outside", temp.path().join("root/outside-link")).unwrap();
    let source = Local::new(vec![(NodeId(1), temp.path().join("root"))]).unwrap();
    let store = SourceGrants::load(&temp.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1)]).unwrap();
    let grant = store.open(PEER, token).unwrap();
    let children = grant.list(NodeId(1), None).unwrap().entries;
    assert_eq!(children.len(), 2);
    let link = children.iter().find(|n| n.name == "outside-link").unwrap();
    assert_eq!(link.kind, NodeKind::File);
    let selected = grant.select(link.id).unwrap();
    let mut bytes = Vec::new();
    grant
        .open(link.id, selected.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"outside");
}

#[cfg(target_os = "macos")]
#[test]
fn failed_root_is_omitted_and_subset_grant_reclassifies_top_link() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("target"), b"payload").unwrap();
    fs::write(temp.path().join("gone"), b"gone").unwrap();
    symlink("target", temp.path().join("alias")).unwrap();
    let source = Local::new(vec![
        (NodeId(1), temp.path().join("gone")),
        (NodeId(2), temp.path().join("target")),
        (NodeId(3), temp.path().join("alias")),
    ])
    .unwrap();
    fs::remove_file(temp.path().join("gone")).unwrap();
    let store = SourceGrants::load(&temp.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, source.as_ref(), &[NodeId(1), NodeId(3)]).unwrap();
    let grant = store.open(PEER, token).unwrap();
    assert_eq!(grant.roots().len(), 1);
    assert_eq!(grant.roots()[0].name, "alias");
    assert_eq!(grant.roots()[0].kind, NodeKind::File);
    let selected = grant.select(NodeId(3)).unwrap();
    let mut bytes = Vec::new();
    let mut reader = grant.open(NodeId(3), selected.version, 0).unwrap();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"payload");
    fs::remove_file(temp.path().join("alias")).unwrap();
    fs::write(temp.path().join("other"), b"replacement").unwrap();
    symlink("other", temp.path().join("alias")).unwrap();
    assert_eq!(
        super::super::reason(&reader.read(&mut [0; 1]).unwrap_err()),
        Unavailable::Superseded
    );
}

#[test]
fn sender_restart_clears_one_shot_cache_and_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"payload").unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), path)]).unwrap());
    let store_dir = dir.path().join("journal");
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, &source, &[NodeId(1)]).unwrap();
    let snapshot = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
    assert!(snapshot.exists());
    drop(store);
    let store = SourceGrants::load(&store_dir, [(PEER, [1; 32])]).unwrap();
    assert!(!snapshot.exists(), "restart must discard every F1 cache");
    assert!(store.open(PEER, token).is_err());
}

#[test]
fn non_clipboard_cache_is_released_on_complete_send_or_failure_without_ack() {
    for fail in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"payload").unwrap();
        let source = OneShot(Local::new(vec![(NodeId(1), path.clone())]).unwrap());
        let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
        let token = store.issue(PEER, &source, &[NodeId(1)]).unwrap();
        let snapshot = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
        let files = store.open(PEER, token).unwrap();
        let entry = files.select(NodeId(1)).unwrap();
        let mut bytes = Vec::new();
        files
            .open(entry.id, entry.version, 0)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(snapshot.exists(), "reading into a buffer does not complete a send");
        if fail {
            store.abandon(PEER, token).unwrap();
        } else {
            store.sent(PEER, token, entry.id).unwrap();
        }
        assert!(!snapshot.exists());
        assert!(files.select(entry.id).is_err());
        assert_eq!(fs::read(path).unwrap(), b"payload");
        let journal: serde_json::Value = serde_json::from_slice(&fs::read(&store.0.path).unwrap()).unwrap();
        assert!(
            journal["grants"].as_array().unwrap().is_empty(),
            "F1 send authority is never persisted"
        );
    }
}
#[test]
fn current_clipboard_cache_survives_send_and_failure_for_late_receivers_until_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"clipboard bytes").unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), path.clone())]).unwrap());
    let other = DeviceId([2; 32]);
    let members = [(PEER, [1; 32]), (other, [2; 32])];
    let store = SourceGrants::load(&dir.path().join("journal"), members).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    store.observe_clipboard(Some((offer, Some([7; 32])))).unwrap();
    let first = store
        .issue_for(PEER, Some(offer), &source, &[NodeId(1)], Demand::Clipboard)
        .unwrap();
    let root = store.fulfilled_roots(PEER, first).unwrap()[0].clone();
    let snapshot = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
    fs::remove_file(path).unwrap();
    store.sent(PEER, first, root.node).unwrap();
    assert!(snapshot.exists());
    let second = store
        .issue_for(other, Some(offer), &source, &[NodeId(1)], Demand::Clipboard)
        .unwrap();
    assert_eq!(
        store.fulfilled_roots(other, second).unwrap(),
        std::slice::from_ref(&root)
    );
    store.abandon(other, second).unwrap();
    assert!(snapshot.exists());
    let third = store
        .issue_for(PEER, Some(offer), &source, &[NodeId(1)], Demand::Clipboard)
        .unwrap();
    let files = store.open(PEER, third).unwrap();
    let entry = files.select(root.node).unwrap();
    let mut bytes = Vec::new();
    files
        .open(entry.id, entry.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"clipboard bytes");
    store.observe_clipboard(None).unwrap();
    assert!(!snapshot.exists());
    assert!(files.select(root.node).is_err());
}
#[test]
fn directory_send_waits_for_every_visible_file_and_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree");
    fs::create_dir(&path).unwrap();
    fs::create_dir(path.join("empty")).unwrap();
    fs::write(path.join("body"), b"x").unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), path)]).unwrap());
    let store = SourceGrants::load(&dir.path().join("journal"), [(PEER, [1; 32])]).unwrap();
    let token = store.issue(PEER, &source, &[NodeId(1)]).unwrap();
    let snapshot = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
    let files = store.open(PEER, token).unwrap();
    let children = files.list(NodeId(1), None).unwrap().entries;
    store.sent(PEER, token, NodeId(1)).unwrap();
    assert!(snapshot.exists());
    let file = children.iter().find(|entry| entry.kind == NodeKind::File).unwrap();
    store.sent(PEER, token, file.id).unwrap();
    assert!(snapshot.exists());
    let empty = children.iter().find(|entry| entry.kind == NodeKind::Directory).unwrap();
    store.sent(PEER, token, empty.id).unwrap();
    assert!(!snapshot.exists());
}

#[test]
fn sender_restart_discards_even_the_current_clipboard_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    fs::write(&path, b"x").unwrap();
    let source = OneShot(Local::new(vec![(NodeId(1), path)]).unwrap());
    let journal = dir.path().join("journal");
    let store = SourceGrants::load(&journal, [(PEER, [1; 32])]).unwrap();
    let offer = crate::content::tests::fixture().inventory().id;
    store.observe_clipboard(Some((offer, Some([7; 32])))).unwrap();
    let token = store
        .issue_for(PEER, Some(offer), &source, &[NodeId(1)], Demand::Clipboard)
        .unwrap();
    let path = store.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&store.0.path);
    store.sent(PEER, token, NodeId(1)).unwrap();
    assert!(path.exists());
    drop(store);
    let store = SourceGrants::load(&journal, [(PEER, [1; 32])]).unwrap();
    assert!(!path.exists());
    assert!(store.open(PEER, token).is_err());
}
