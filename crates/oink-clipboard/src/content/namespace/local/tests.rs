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
fn external_links_follow_but_replacing_an_ancestor_cannot_redirect_a_reader() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("root")).unwrap();
    fs::create_dir(dir.path().join("outside")).unwrap();
    fs::write(dir.path().join("root/file"), b"owned").unwrap();
    fs::write(dir.path().join("outside/file"), b"foreign").unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside"), dir.path().join("root/link")).unwrap();
    let tree = Local::new(vec![(NodeId(1), dir.path().join("root"))]).unwrap();
    let page = tree.list(NodeId(1), None).unwrap();
    assert_eq!(page.entries.len(), if cfg!(target_os = "macos") { 2 } else { 1 });
    let selected = tree
        .select(page.entries.iter().find(|n| n.name == "file").unwrap().id)
        .unwrap();
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

#[cfg(target_os = "macos")]
#[test]
fn safe_symlinks_are_leaf_metadata_and_never_traversed() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Fixture.app");
    fs::create_dir_all(root.join("Resources/nested")).unwrap();
    fs::write(root.join("Resources/nested/content.txt"), b"payload").unwrap();
    for (name, target) in [
        ("current", "nested"),
        ("linked.txt", "nested/content.txt"),
        ("dangling", "missing"),
        ("loop", "."),
        ("escape", "../../outside"),
        ("absolute", "/tmp/outside"),
    ] {
        symlink(target, root.join("Resources").join(name)).unwrap();
    }
    symlink("../Resources", root.join("Resources/back")).unwrap();
    let source = Local::new(vec![(NodeId(1), root)]).unwrap();
    let resources = source.list(NodeId(1), None).unwrap().entries.remove(0);
    let page = source.list(resources.id, None).unwrap();
    assert_eq!(page.entries.len(), 5);
    for link in page.entries.iter().filter(|node| node.kind.symlink_target().is_some()) {
        assert!(source.list(link.id, None).is_err());
        assert!(source.select(link.id).is_err());
        assert!(source.open(link.id, link.version, 0).is_err());
    }
    assert!(
        page.entries
            .iter()
            .all(|node| node.name != "absolute" && node.name != "escape")
    );
    assert_eq!(
        page.entries.iter().find(|node| node.name == "current").unwrap().kind,
        NodeKind::Symlink("nested".into())
    );
}

#[cfg(target_os = "macos")]
#[test]
fn execute_bits_are_file_metadata_and_chmod_changes_the_selected_version() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("executable");
    fs::write(&path, b"payload").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o4755)).unwrap();
    let source = Local::clipboard(vec![(NodeId(1), path.clone())]).unwrap();
    assert_eq!(
        source.roots()[0].exec_bits,
        None,
        "group-wide announcements carry no execution metadata"
    );
    let executable = source.select(NodeId(1)).unwrap();
    assert_eq!(executable.exec_bits, Some(0o111));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let ordinary = source.select(NodeId(1)).unwrap();
    assert_eq!(ordinary.exec_bits, Some(0));
    assert_ne!(ordinary.version, executable.version);
}

#[cfg(target_os = "macos")]
#[test]
fn batch_links_follow_external_targets_and_keep_cross_root_identities() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join("data"), b"inside").unwrap();
    fs::write(temp.path().join("outside"), b"outside").unwrap();
    symlink("../B/data", a.join("relative")).unwrap();
    symlink(b.join("data"), a.join("absolute")).unwrap();
    symlink("../outside", a.join("external")).unwrap();
    symlink("missing", a.join("broken")).unwrap();
    symlink(b.join("data"), temp.path().join("root-link")).unwrap();
    let source = Local::new(vec![
        (NodeId(1), temp.path().join("root-link")),
        (NodeId(2), a),
        (NodeId(3), b),
    ])
    .unwrap();
    assert_eq!(source.roots()[0].kind, NodeKind::Symlink("B/data".into()));
    let page = source.list(NodeId(2), None).unwrap();
    for name in ["relative", "absolute"] {
        assert_eq!(
            page.entries.iter().find(|node| node.name == name).unwrap().kind,
            NodeKind::Symlink("../B/data".into())
        );
    }
    assert!(page.entries.iter().all(|node| node.name != "broken"));
    let outside = page.entries.iter().find(|node| node.name == "external").unwrap();
    assert_eq!(outside.kind, NodeKind::File);
    let selected = source.select(outside.id).unwrap();
    let mut reader = source.open(outside.id, selected.version, 0).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"outside");
    fs::write(temp.path().join("replacement"), b"new").unwrap();
    fs::rename(temp.path().join("replacement"), temp.path().join("outside")).unwrap();
    assert_eq!(
        super::super::super::reason(&reader.read(&mut [0; 1]).unwrap_err()),
        Unavailable::Superseded
    );
}

#[cfg(target_os = "macos")]
#[test]
fn selected_link_file_directory_mixed_broken_roots_and_nested_cycles() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("target");
    fs::create_dir_all(dir.join("nested")).unwrap();
    fs::write(dir.join("nested/data"), b"directory").unwrap();
    fs::write(temp.path().join("file"), b"file").unwrap();
    symlink(&dir, temp.path().join("directory-alias")).unwrap();
    symlink(temp.path().join("file"), temp.path().join("file-alias")).unwrap();
    symlink("missing", temp.path().join("broken")).unwrap();
    symlink(".", dir.join("cycle")).unwrap();
    symlink(temp.path().join("file"), dir.join("outside-file")).unwrap();
    let source = Local::new(vec![
        (NodeId(1), temp.path().join("directory-alias")),
        (NodeId(2), temp.path().join("broken")),
        (NodeId(3), temp.path().join("file-alias")),
        (NodeId(4), temp.path().join("absent")),
    ])
    .unwrap();
    assert_eq!(
        source.roots().iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        ["directory-alias", "file-alias"]
    );
    assert_eq!(source.roots()[0].kind, NodeKind::Directory);
    assert_eq!(source.roots()[1].kind, NodeKind::File);
    let page = source.list(NodeId(1), None).unwrap();
    assert_eq!(
        page.entries.iter().find(|n| n.name == "cycle").unwrap().kind,
        NodeKind::Symlink(".".into())
    );
    let nested = page.entries.iter().find(|n| n.name == "nested").unwrap();
    let file = source.list(nested.id, None).unwrap().entries.remove(0);
    let selected = source.select(file.id).unwrap();
    let mut bytes = Vec::new();
    source
        .open(file.id, selected.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"directory");
    let selected = source.select(NodeId(3)).unwrap();
    let mut bytes = Vec::new();
    source
        .open(NodeId(3), selected.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"file");
}

#[cfg(target_os = "macos")]
#[test]
fn recursive_external_directory_follow_is_bounded_and_target_retargeting_is_superseded() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    for name in ["selected", "external", "other"] {
        fs::create_dir(temp.path().join(name)).unwrap();
    }
    fs::write(temp.path().join("other/data"), b"third").unwrap();
    symlink("../external", temp.path().join("selected/follow")).unwrap();
    symlink("../other", temp.path().join("external/recurse")).unwrap();
    symlink("../external", temp.path().join("other/cycle")).unwrap();
    let source = Local::new(vec![(NodeId(1), temp.path().join("selected"))]).unwrap();
    let follow = source.list(NodeId(1), None).unwrap().entries.remove(0);
    assert_eq!(follow.kind, NodeKind::Directory);
    let recurse = source.list(follow.id, None).unwrap().entries.remove(0);
    assert_eq!(recurse.kind, NodeKind::Directory);
    let children = source.list(recurse.id, None).unwrap().entries;
    assert_eq!(
        children.iter().find(|n| n.name == "cycle").unwrap().kind,
        NodeKind::Symlink("..".into())
    );
    let data = children.iter().find(|n| n.name == "data").unwrap();
    let selected = source.select(data.id).unwrap();
    let mut reader = source.open(data.id, selected.version, 0).unwrap();
    fs::remove_file(temp.path().join("selected/follow")).unwrap();
    symlink("../other", temp.path().join("selected/follow")).unwrap();
    assert_eq!(
        super::super::super::reason(&reader.read(&mut [0; 1]).unwrap_err()),
        Unavailable::Superseded
    );
}

#[cfg(windows)]
#[test]
fn windows_selected_and_nested_junctions_follow_external_targets_but_skip_cycles() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("file"), b"payload").unwrap();
    let create = |link: &Path, to: &Path| {
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(to)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    };
    create(&temp.path().join("selected"), &target);
    create(&target.join("nested"), &target);
    let external = temp.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("external-file"), b"outside").unwrap();
    create(&target.join("follow"), &external);
    create(&external.join("cycle"), &target);
    let source = Local::new(vec![(NodeId(1), temp.path().join("selected"))]).unwrap();
    assert_eq!(source.roots()[0].name, "selected");
    assert_eq!(source.roots()[0].kind, NodeKind::Directory);
    let page = source.list(NodeId(1), None).unwrap();
    assert_eq!(page.entries.len(), 2);
    let follow = page.entries.iter().find(|node| node.name == "follow").unwrap();
    let followed = source.list(follow.id, None).unwrap();
    assert_eq!(followed.entries.len(), 1);
    assert_eq!(followed.entries[0].name, "external-file");
    let selected = source.select(followed.entries[0].id).unwrap();
    let mut reader = source.open(selected.id, selected.version, 0).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"outside");
    fs::create_dir(temp.path().join("replacement")).unwrap();
    fs::remove_dir(target.join("follow")).unwrap();
    create(&target.join("follow"), &temp.path().join("replacement"));
    assert_eq!(
        super::super::super::reason(&reader.read(&mut [0; 1]).unwrap_err()),
        Unavailable::Superseded
    );
}

#[cfg(target_os = "macos")]
#[test]
fn repeated_directory_identity_is_a_link_and_followed_trees_obey_limits() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("root")).unwrap();
    fs::create_dir(temp.path().join("external")).unwrap();
    fs::write(temp.path().join("external/file"), b"payload").unwrap();
    for name in ["a", "b"] {
        symlink("../external", temp.path().join("root").join(name)).unwrap();
    }
    let source = Local::with_limit(vec![(NodeId(1), temp.path().join("root"))], 3).unwrap();
    let page = source.list(NodeId(1), None).unwrap();
    assert_eq!(page.entries[0].kind, NodeKind::Directory);
    assert_eq!(page.entries[1].kind, NodeKind::Symlink("a".into()));
    assert_eq!(
        super::super::super::reason(&source.list(page.entries[0].id, None).unwrap_err()),
        Unavailable::LimitExceeded
    );
}

#[cfg(windows)]
#[test]
fn windows_selected_and_nested_file_symlinks_follow_outside_batch() {
    use std::os::windows::fs::symlink_file;
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("root")).unwrap();
    fs::write(temp.path().join("outside"), b"outside").unwrap();
    symlink_file(temp.path().join("outside"), temp.path().join("root/link")).unwrap();
    let source = Local::new(vec![(NodeId(1), temp.path().join("root"))]).unwrap();
    let item = source.list(NodeId(1), None).unwrap().entries.remove(0);
    assert_eq!(item.kind, NodeKind::File);
    let selected = source.select(item.id).unwrap();
    let mut bytes = Vec::new();
    source
        .open(item.id, selected.version, 0)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"outside");
    let selected = Local::new(vec![(NodeId(1), temp.path().join("root/link"))]).unwrap();
    assert_eq!(selected.roots()[0].kind, NodeKind::File);
    let internal = Local::new(vec![
        (NodeId(1), temp.path().join("outside")),
        (NodeId(2), temp.path().join("root/link")),
    ])
    .unwrap();
    assert_eq!(internal.roots().len(), 1);
}

#[cfg(target_os = "macos")]
#[test]
fn unreadable_roots_and_link_targets_do_not_cancel_readable_items() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("root")).unwrap();
    fs::write(temp.path().join("root/good"), b"good").unwrap();
    fs::write(temp.path().join("private"), b"private").unwrap();
    symlink("../private", temp.path().join("root/unreadable")).unwrap();
    fs::set_permissions(temp.path().join("private"), fs::Permissions::from_mode(0o0)).unwrap();
    let source = Local::new(vec![
        (NodeId(1), temp.path().join("private")),
        (NodeId(2), temp.path().join("root")),
    ])
    .unwrap();
    assert_eq!(source.roots().len(), 1);
    let page = source.list(NodeId(2), None).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "good");
    fs::set_permissions(temp.path().join("private"), fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn batch_link_chains_preserve_current_indirection() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Framework");
    fs::create_dir_all(root.join("Versions/A/Resources")).unwrap();
    symlink("A", root.join("Versions/Current")).unwrap();
    symlink("Versions/Current/Resources", root.join("Resources")).unwrap();
    symlink("Versions/Current", root.join("Alias")).unwrap();
    let source = Local::new(vec![(NodeId(1), root)]).unwrap();
    let page = source.list(NodeId(1), None).unwrap();
    assert_eq!(
        page.entries.iter().find(|node| node.name == "Alias").unwrap().kind,
        NodeKind::Symlink("Versions/Current".into())
    );
    assert_eq!(
        page.entries.iter().find(|node| node.name == "Resources").unwrap().kind,
        NodeKind::Symlink("Versions/Current/Resources".into())
    );
}
