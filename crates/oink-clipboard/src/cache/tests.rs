use super::*;
use crate::store::{Batch, BatchId, StoreKind, usage};
const KIND: StoreKind = StoreKind::CompletedDelivery;
fn complete(data: &Path) -> PathBuf {
    let batch = Batch::create(data, KIND, BatchId([91; 16])).unwrap();
    batch.complete().unwrap();
    batch.path().to_owned()
}
#[test]
fn cache_accounts_nested_sidecars_and_clears_only_owned_batches() {
    let data = tempfile::tempdir().unwrap();
    let owned = complete(data.path());
    fs::create_dir(owned.join("nested")).unwrap();
    fs::write(owned.join("nested/0.part"), vec![7; 9000]).unwrap();
    fs::write(owned.join("0.meta"), vec![0; 64]).unwrap();
    let root = data.path().join(KIND.directory());
    let unknown = root.join("unknown");
    fs::create_dir(&unknown).unwrap();
    fs::write(unknown.join("user"), b"keep").unwrap();
    let saved = data.path().join("Received");
    fs::create_dir(&saved).unwrap();
    fs::write(saved.join("user"), b"untouched").unwrap();
    let before = tree_bytes(&root).unwrap();
    let cleared = usage(data.path(), KIND, true);
    assert_eq!(before, cleared.0.bytes + cleared.1);
    assert_eq!(cleared.0.bytes, cleared.0.kept_bytes);
    assert!(cleared.2, "an unowned directory is retained and reported");
    assert!(!owned.exists());
    assert!(unknown.join("user").exists());
    assert!(saved.join("user").exists());
}
#[test]
fn cache_clear_preserves_current_source_paths_inside_storage() {
    let data = tempfile::tempdir().unwrap();
    let folder = complete(data.path());
    let file = folder.join("current");
    fs::write(&file, vec![2; 5000]).unwrap();
    let current = SourceLease::current(file.clone());
    let report = usage(data.path(), KIND, true);
    assert_eq!(report.1, 0);
    assert_eq!(report.0.bytes, report.0.kept_bytes);
    assert!(file.exists());
    drop(current);
    assert!(usage(data.path(), KIND, true).1 > 0);
}
#[cfg(windows)]
#[test]
fn cache_accounts_allocated_clusters_instead_of_logical_eof() {
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("ordinary");
    fs::write(&file, vec![7; 8193]).unwrap();
    let bytes = allocated(&file, &fs::metadata(&file).unwrap()).unwrap();
    assert!(bytes >= 8193);
    assert_eq!(bytes % 512, 0);
}
#[cfg(windows)]
#[test]
fn cache_clear_keeps_busy_files_and_reports_partial_removal() {
    use std::os::windows::fs::OpenOptionsExt;
    let data = tempfile::tempdir().unwrap();
    let folder = complete(data.path());
    let busy = folder.join("busy");
    fs::write(&busy, vec![1; 8000]).unwrap();
    fs::write(folder.join("idle"), vec![2; 9000]).unwrap();
    let handle = fs::OpenOptions::new().read(true).share_mode(3).open(&busy).unwrap();
    let before = tree_bytes(&folder).unwrap();
    let report = usage(data.path(), KIND, true);
    assert!(report.2);
    assert_eq!(report.0.bytes, tree_bytes(&folder).unwrap());
    assert_eq!(report.0.kept_bytes, report.0.bytes);
    assert_eq!(before, report.0.bytes + report.1);
    assert!(busy.exists());
    drop(handle);
    assert!(usage(data.path(), KIND, true).1 > 0);
}
#[cfg(unix)]
#[test]
fn cache_clear_refuses_symlink_roots_and_linked_children() {
    use std::os::unix::fs::symlink;
    let data = tempfile::tempdir().unwrap();
    let outside = data.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("user"), b"safe").unwrap();
    let root = data.path().join(KIND.directory());
    symlink(&outside, &root).unwrap();
    assert!(usage(data.path(), KIND, true).2);
    assert!(outside.join("user").exists());
    fs::remove_file(&root).unwrap();
    let folder = complete(data.path());
    symlink(&outside, folder.join("link")).unwrap();
    assert!(usage(data.path(), KIND, true).2);
    assert!(outside.join("user").exists());
}
