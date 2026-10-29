use super::tests::OneShot;
use super::*;
use crate::model::{ContentSize, DIRECTORY_PAGE_ENTRIES, NodeKind};
use crate::policy::Settings;

const MAC: DeviceId = DeviceId([10; 32]);
const WINDOWS: DeviceId = DeviceId([20; 32]);

fn store(path: &Path) -> SourceGrants {
    SourceGrants::load(path, [(MAC, [1; 32]), (WINDOWS, [2; 32])]).unwrap()
}
fn configure(store: &SourceGrants, enabled: bool) {
    let filters = Settings {
        remove_ds_store: enabled,
        ..Default::default()
    };
    store.set_peer_file_filter(MAC, filters.omit_ds_store_for("macos", "macos"));
    store.set_peer_file_filter(WINDOWS, filters.omit_ds_store_for("macos", "windows"));
}
fn all(files: &dyn Namespace, root: NodeId) -> Vec<FileNode> {
    let mut result = vec![];
    let mut cursor = None;
    loop {
        let page = files.list(root, cursor).unwrap();
        page.validate(cursor).unwrap();
        result.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            return result;
        }
    }
}
#[test]
fn ds_store_policy_only_filters_mac_sources_to_other_known_systems() {
    let filters = Settings::default();
    assert!(filters.remove_ds_store);
    for (source, target, expected) in [
        ("macos", "windows", true),
        ("macos", "linux", true),
        ("macos", "macos", false),
        ("windows", "macos", false),
        ("windows", "windows", false),
        ("macos", "", false),
    ] {
        assert_eq!(filters.omit_ds_store_for(source, target), expected);
        assert!(
            !Settings {
                remove_ds_store: false,
                ..filters
            }
            .omit_ds_store_for(source, target)
        );
    }
}
#[test]
fn ds_store_peer_views_filter_nested_children_before_pagination_and_survive_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("folder");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join(".DS_Store"), b"root metadata").unwrap();
    fs::write(root.join("nested/.DS_Store"), b"nested metadata").unwrap();
    fs::write(root.join("nested/keep"), b"payload").unwrap();
    // Prefix/suffix matches must remain ordinary files (also on case-insensitive volumes).
    fs::write(root.join("nested/.DS_Store.keep"), b"keep").unwrap();
    for index in 0..DIRECTORY_PAGE_ENTRIES {
        fs::write(root.join(format!("file-{index:04}")), b"x").unwrap();
    }
    let source = namespace::Local::new(vec![(NodeId(1), root.clone())]).unwrap();
    let path = temp.path().join("grants");
    let grants = store(&path);
    configure(&grants, true);
    let win = grants.issue(WINDOWS, source.as_ref(), &[NodeId(1)]).unwrap();
    let mac = grants.issue(MAC, source.as_ref(), &[NodeId(1)]).unwrap();
    let transient = grants.list_for_peer(WINDOWS, source.as_ref(), NodeId(1), None).unwrap();
    assert_eq!(transient.entries.len(), DIRECTORY_PAGE_ENTRIES);
    assert!(!transient.entries.iter().any(|entry| entry.name == ".DS_Store"));
    let transient_last = grants
        .list_for_peer(WINDOWS, source.as_ref(), NodeId(1), transient.next)
        .unwrap();
    assert_eq!(transient_last.entries.len(), 1);
    assert!(transient_last.next.is_none());
    let files = grants.open(WINDOWS, win).unwrap();
    let first = files.list(NodeId(1), None).unwrap();
    assert_eq!(first.entries.len(), DIRECTORY_PAGE_ENTRIES);
    assert_eq!(first.next.unwrap().offset as usize, DIRECTORY_PAGE_ENTRIES);
    let second = files.list(NodeId(1), first.next).unwrap();
    assert_eq!(second.entries.len(), 1);
    assert!(second.next.is_none());
    let entries = all(files.as_ref(), NodeId(1));
    assert_eq!(entries.len(), DIRECTORY_PAGE_ENTRIES + 1);
    assert!(!entries.iter().any(|entry| entry.name == ".DS_Store"));
    let nested = entries.iter().find(|entry| entry.name == "nested").unwrap();
    let children = all(files.as_ref(), nested.id);
    assert_eq!(
        children.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(),
        [".DS_Store.keep", "keep"]
    );
    assert_eq!(
        children
            .iter()
            .map(|entry| match entry.size {
                ContentSize::Known(n) => n,
                _ => panic!(),
            })
            .sum::<u64>(),
        11
    );
    let mac_files = grants.open(MAC, mac).unwrap();
    assert_eq!(all(mac_files.as_ref(), NodeId(1)).len(), DIRECTORY_PAGE_ENTRIES + 2);
    let mac_nested = all(mac_files.as_ref(), NodeId(1))
        .into_iter()
        .find(|entry| entry.name == "nested")
        .unwrap();
    assert!(
        all(mac_files.as_ref(), mac_nested.id)
            .iter()
            .any(|entry| entry.name == ".DS_Store")
    );
    // Settings affect a new request, while an existing token/cursor stays stable.
    configure(&grants, false);
    let unfiltered = grants.issue(WINDOWS, source.as_ref(), &[NodeId(1)]).unwrap();
    assert_eq!(
        all(grants.open(WINDOWS, unfiltered).unwrap().as_ref(), NodeId(1)).len(),
        DIRECTORY_PAGE_ENTRIES + 2
    );
    drop((files, mac_files, grants));
    let grants = store(&path);
    let files = grants.open(WINDOWS, win).unwrap();
    assert_eq!(files.list(NodeId(1), first.next).unwrap().entries.len(), 1);
    assert_eq!(fs::read(root.join(".DS_Store")).unwrap(), b"root metadata");
    assert_eq!(fs::read(root.join("nested/.DS_Store")).unwrap(), b"nested metadata");
}
#[test]
fn ds_store_explicit_root_is_kept_for_clipboard_and_drag() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".DS_Store");
    fs::write(&path, b"explicit").unwrap();
    let source = namespace::Local::new(vec![(NodeId(1), path)]).unwrap();
    let grants = store(&temp.path().join("grants"));
    configure(&grants, true);
    let offer = oink_foundation::content::OfferId {
        clip: oink_foundation::content::ClipId { origin: MAC, seq: 1 },
        generation: 1,
    };
    let holder = oink_foundation::content::Holder {
        device: WINDOWS,
        epoch: 1,
    };
    for demand in [Demand::Clipboard, Demand::Hover(holder), Demand::Drop(holder)] {
        let token = grants
            .issue_for(WINDOWS, Some(offer), source.as_ref(), &[NodeId(1)], demand)
            .unwrap();
        let files = grants.open(WINDOWS, token).unwrap();
        assert_eq!(files.roots()[0].name, ".DS_Store");
        if !matches!(demand, Demand::Hover(_)) {
            let entry = files.select(NodeId(1)).unwrap();
            let mut bytes = vec![];
            files
                .open(entry.id, entry.version, 0)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, b"explicit");
        }
    }
}
#[test]
fn ds_store_upgrade_discards_build_52_journal_once_and_immediately_serves_new_sources() {
    #[derive(Clone)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".DS_Store"), b"metadata").unwrap();
    fs::write(root.join("body"), b"new copy").unwrap();
    let source = OneShot(namespace::Local::new(vec![(NodeId(1), root.clone())]).unwrap());
    let path = temp.path().join("grants");
    let grants = store(&path);
    let old_token = grants.issue(WINDOWS, &source, &[NodeId(1)]).unwrap();
    let old_snapshot = grants.0.state.lock().unwrap().journal.grants[0].snapshots[0].path(&grants.0.path);
    assert!(old_snapshot.exists());
    // Build 52 has these same schema-1 fields, without the per-peer view flag
    // or the second snapshot digest. This fixture contains real captured paths.
    let journal_path = path.join("source-links-v1.json");
    let mut build_52 = serde_json::to_value(&grants.0.state.lock().unwrap().journal).unwrap();
    for grant in build_52["grants"].as_array_mut().unwrap() {
        grant.as_object_mut().unwrap().remove("omit_ds_store");
        for snapshot in grant["snapshots"].as_array_mut().unwrap() {
            snapshot.as_object_mut().unwrap().remove("filtered_digest");
        }
    }
    fs::write(&journal_path, serde_json::to_vec(&build_52).unwrap()).unwrap();
    drop(grants);
    let bytes = Arc::new(Mutex::new(vec![]));
    let output = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || Output(output.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let grants = store(&path); // Startup succeeds; no file-service fault.
        assert!(grants.0.state.lock().unwrap().healthy);
        assert!(grants.0.state.lock().unwrap().journal.grants.is_empty());
        assert!(grants.open(WINDOWS, old_token).is_err());
        assert!(!old_snapshot.exists());
        let persisted: Journal = serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
        assert!(persisted.grants.is_empty());
        configure(&grants, true);
        let fresh = namespace::Local::new(vec![(NodeId(1), root.clone())]).unwrap();
        let offer = oink_foundation::content::OfferId {
            clip: oink_foundation::content::ClipId { origin: MAC, seq: 2 },
            generation: 1,
        };
        let holder = oink_foundation::content::Holder {
            device: WINDOWS,
            epoch: 2,
        };
        for demand in [Demand::Clipboard, Demand::Drop(holder)] {
            let token = grants
                .issue_for(WINDOWS, Some(offer), fresh.as_ref(), &[NodeId(1)], demand)
                .unwrap();
            let files = grants.open(WINDOWS, token).unwrap();
            let entries = all(files.as_ref(), NodeId(1));
            assert_eq!(entries.len(), 1);
            let entry = files.select(entries[0].id).unwrap();
            let mut bytes = vec![];
            files
                .open(entry.id, entry.version, 0)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, b"new copy");
        }
        drop(grants);
        let grants = store(&path); // No repeated reset or repeated log on restart.
        assert_eq!(grants.0.state.lock().unwrap().journal.grants.len(), 2);
        assert!(grants.open(WINDOWS, old_token).is_err());
    });
    let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert_eq!(output.lines().count(), 1, "{output}");
    assert!(output.contains("source_links_schema_reset"));
    assert_eq!(fs::read(root.join("body")).unwrap(), b"new copy");
}

#[test]
fn ds_store_filter_is_exact_and_an_empty_filtered_directory_is_complete() {
    let entries: Vec<_> = [".DS_Store", ".ds_store", ".DS_STORE", ".DS_Store.backup"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| FileNode {
            exec_bits: None,
            id: NodeId(index as u64 + 2),
            parent: Some(NodeId(1)),
            name: name.into(),
            kind: NodeKind::File,
            size: ContentSize::Known(1),
            version: 1,
            modified_ms: None,
        })
        .collect();
    let filtered = file_filter::page(true, NodeId(1), 17, &entries, None).unwrap();
    assert_eq!(
        filtered
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        [".ds_store", ".DS_STORE", ".DS_Store.backup"]
    );
    assert_eq!(filtered.version, 17);
    assert!(filtered.next.is_none());
    let empty = file_filter::page(true, NodeId(1), 17, &entries[..1], None).unwrap();
    empty.validate(None).unwrap();
    assert!(empty.entries.is_empty());
    assert!(empty.next.is_none());
    assert_eq!(empty.version, 17);
}
