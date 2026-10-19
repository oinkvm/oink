#![cfg(debug_assertions)]
//! A new native placement call must opt into the capture-side accounting API.
//! This walks production sources, including newly added modules, on either host.
use std::path::Path;

fn sources(dir: &Path, visit: &mut impl FnMut(&Path, &str)) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, visit);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            let code: String = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .flat_map(|line| line.chars().filter(|c| !c.is_whitespace()))
                .collect();
            visit(&path, &code);
        }
    }
}

#[test]
fn windows_cursor_placement_never_uses_untaggable_set_cursor_pos() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/windows");
    sources(&root, &mut |path, code| {
        assert!(
            !code.contains("SetCursorPos("),
            "untagged cursor input in {}",
            path.display()
        );
    });
}

#[test]
fn every_windows_native_warp_uses_the_accounting_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/windows");
    let mut calls = 0;
    sources(&root, &mut |path, code| {
        for primitive in ["ClipCursor(Some", "SendInput("] {
            if code.contains(primitive) {
                assert_eq!(
                    path.file_name().unwrap(),
                    "warp.rs",
                    "unaccounted {primitive} in {}",
                    path.display()
                );
                calls += 1;
            }
        }
    });
    assert_eq!(calls, 2, "keep the native-boundary inventory current");
}

#[test]
fn macos_direct_warp_cannot_separate_placement_from_registration() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/macos");
    let mut calls = 0;
    sources(&root, &mut |path, code| {
        if code.contains("CGWarpMouseCursorPosition(") {
            assert_eq!(path.file_name().unwrap(), "cursor.rs");
            assert!(code.contains("fix:&std::sync::Mutex<crate::warp_delta::WarpDeltaFix>"));
            assert!(code.contains(".warped(at,done_ns)"));
            calls += code.matches("CGWarpMouseCursorPosition(").count();
        }
    });
    assert_eq!(
        calls, 2,
        "entry and the optional same-point nudge share one registered operation"
    );
}

#[test]
fn macos_accepted_cursor_events_share_the_absolute_displacement_registration() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/macos");
    sources(&root, &mut |path, code| {
        if path.file_name().unwrap() == "tap.rs" {
            assert_eq!(code.matches(".posted_absolute_motion(").count(), 1);
            assert_eq!(
                code.matches("account_cursor_placement(s,ev,").count(),
                4,
                "owner relocation, CG motion/button, native button and native motion must all register"
            );
            assert!(
                !code.contains(".posted("),
                "clearing a warp alone loses absolute displacement"
            );
        }
    });
}
