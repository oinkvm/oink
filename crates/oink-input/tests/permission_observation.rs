#![cfg(debug_assertions)]
//! Architectural regression: permission observation must not create an input worker.
use std::path::Path;

#[test]
fn no_independent_permission_thread_exists() {
    fn inspect(path: &Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                inspect(&path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                assert!(
                    !source.contains("oink-input-permissions"),
                    "permission worker in {}",
                    path.display()
                );
                assert!(
                    !source.contains("permission_changed("),
                    "permission epoch publisher in {}",
                    path.display()
                );
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    inspect(&root);
    assert!(!root.join("macos/permissions.rs").exists());
    let backend = std::fs::read_to_string(root.join("macos/mod.rs")).unwrap();
    assert!(backend.contains("fn permissions(&self) -> InputPermissions {\n        system::permissions()"));
}
