use super::*;
use crate::model::DIRECTORY_PAGE_ENTRIES;

#[test]
fn offer_stats_roots_only_and_each_directory_freezes_when_first_requested() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("root")).unwrap();
    fs::create_dir(dir.path().join("root/sub")).unwrap();
    let tree = Local::new(vec![(NodeId(7), dir.path().join("root"))]).unwrap();
    assert_eq!(tree.roots().len(), 1);
    assert_eq!(tree.state.lock().unwrap().nodes.len(), 1);
    let root = tree.list(NodeId(7), None).unwrap();
    assert_eq!(root.entries.len(), 1);
    assert_eq!(tree.state.lock().unwrap().nodes.len(), 2);
    fs::write(dir.path().join("root/sub/late"), b"first list sees this").unwrap();
    let child = tree.list(root.entries[0].id, None).unwrap();
    assert_eq!(child.entries.len(), 1);
    assert_eq!(child.entries[0].name, "late");
    assert!(child.next.is_none());
}

#[test]
fn pages_are_stable_and_cursor_is_scoped_to_its_view() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..DIRECTORY_PAGE_ENTRIES + 3 {
        fs::write(dir.path().join(format!("item-{n:05}")), b"x").unwrap();
    }
    let tree = Local::new(vec![(NodeId(1), dir.path().to_owned())]).unwrap();
    let first = tree.list(NodeId(1), None).unwrap();
    assert_eq!(first.entries.len(), DIRECTORY_PAGE_ENTRIES);
    let second = tree.list(NodeId(1), first.next).unwrap();
    assert_eq!(second.entries.len(), 3);
    assert!(second.next.is_none());
    assert_eq!(tree.list(NodeId(1), first.next).unwrap(), second);
    assert_ne!(first.entries.last().unwrap().id, second.entries[0].id);
    let wrong = DirectoryCursor {
        version: first.version.wrapping_add(1),
        offset: 1,
    };
    assert_eq!(
        tree.list(NodeId(1), Some(wrong)).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    // Removing an already enumerated directory cannot return an empty final page.
    let root = tree.state.lock().unwrap().nodes[&NodeId(1)]
        .location
        .get()
        .unwrap()
        .path
        .clone();
    let elsewhere = dir.path().with_extension("renamed");
    fs::rename(&root, &elsewhere).unwrap();
    assert!(tree.list(NodeId(1), first.next).is_err());
    fs::rename(elsewhere, root).unwrap();
}

#[test]
fn too_many_children_fails_without_publishing_a_partial_view() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..3 {
        fs::write(dir.path().join(format!("item-{n}")), b"x").unwrap();
    }
    let tree = Local::with_limit(vec![(NodeId(1), dir.path().to_owned())], 3).unwrap();
    assert_eq!(
        tree.list(NodeId(1), None).unwrap_err().kind(),
        io::ErrorKind::FileTooLarge
    );
    assert_eq!(tree.state.lock().unwrap().nodes.len(), 1);
    assert!(tree.state.lock().unwrap().nodes[&NodeId(1)].view.is_none());
}

#[test]
fn first_read_selects_current_file_then_changes_fail_including_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"old").unwrap();
    let tree = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
    fs::write(&path, b"new bytes").unwrap();
    let selected = tree.select(NodeId(1)).unwrap();
    assert_eq!(selected.size, ContentSize::Known(9));
    let mut reader = tree.open(NodeId(1), selected.version, 0).unwrap();
    let mut bytes = [0; 9];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"new bytes");
    fs::write(&path, b"changed length after last bytes").unwrap();
    assert_eq!(
        crate::content::reason(&reader.read(&mut [0]).unwrap_err()),
        Unavailable::Superseded
    );
    assert_eq!(tree.select(NodeId(1)).unwrap_err().kind(), io::ErrorKind::Interrupted);
}

#[test]
fn stable_file_identity_ignores_offer_ids_and_cancel_closes_existing_readers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"payload").unwrap();
    let first = Local::new(vec![(NodeId(1), path.clone())]).unwrap();
    let second = Local::new(vec![(NodeId(98), path)]).unwrap();
    assert_eq!(first.roots()[0].version, second.roots()[0].version);
    let selected = first.select(NodeId(1)).unwrap();
    let mut reader = first.open(NodeId(1), selected.version, 2).unwrap();
    let mut bytes = [0; 2];
    reader.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"yl");
    first.cancel();
    assert_eq!(
        crate::content::reason(&reader.read(&mut bytes).unwrap_err()),
        Unavailable::Cancelled
    );
    assert!(first.select(NodeId(1)).is_err());
}

#[cfg(unix)]
#[test]
fn links_are_skipped_and_replacing_an_ancestor_cannot_redirect_a_reader() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("root")).unwrap();
    fs::create_dir(dir.path().join("outside")).unwrap();
    fs::write(dir.path().join("root/file"), b"owned").unwrap();
    fs::write(dir.path().join("outside/file"), b"foreign").unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside"), dir.path().join("root/link")).unwrap();
    let tree = Local::new(vec![(NodeId(1), dir.path().join("root"))]).unwrap();
    let page = tree.list(NodeId(1), None).unwrap();
    assert_eq!(page.entries.len(), 1);
    let selected = tree.select(page.entries[0].id).unwrap();
    let mut reader = tree.open(selected.id, selected.version, 0).unwrap();
    fs::rename(dir.path().join("root"), dir.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside"), dir.path().join("root")).unwrap();
    assert!(reader.read(&mut [0]).is_err());
    assert!(tree.select(selected.id).is_err());
}

#[test]
fn clipboard_selection_can_publish_a_new_version_between_independent_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"old").unwrap();
    let tree = Local::clipboard(vec![(NodeId(1), path.clone())]).unwrap();
    let old = tree.select(NodeId(1)).unwrap();
    let mut reader = tree.open(NodeId(1), old.version, 0).unwrap();
    reader.read_exact(&mut [0; 3]).unwrap();
    fs::write(&path, b"latest content").unwrap();
    assert_eq!(
        crate::content::reason(&reader.read(&mut [0]).unwrap_err()),
        Unavailable::Superseded
    );
    let latest = tree.select(NodeId(1)).unwrap();
    assert_ne!(old.version, latest.version);
    assert_eq!(latest.size, ContentSize::Known(14));
    let mut bytes = Vec::new();
    tree.open(NodeId(1), latest.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"latest content");
}

#[test]
fn moved_deleted_or_replaced_identity_is_no_such_item() {
    for mutation in [0, 1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"original").unwrap();
        let tree = Local::clipboard(vec![(NodeId(1), path.clone())]).unwrap();
        if mutation == 0 {
            fs::remove_file(&path).unwrap();
        } else {
            fs::rename(&path, dir.path().join("moved")).unwrap();
        }
        if mutation == 2 {
            fs::write(&path, b"different identity").unwrap();
        }
        assert_eq!(
            crate::content::reason(&tree.select(NodeId(1)).unwrap_err()),
            Unavailable::NoSuchItem
        );
    }
}
