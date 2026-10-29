fn main() {
    println!("cargo:rerun-if-env-changed=OINK_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=OINK_RELEASE_BUILD");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" && target_os != "windows" {
        return;
    }
    if target_os == "macos" {
        // The UI is a LaunchServices helper inside the host app. Sparkle lives
        // in the outer app's Frameworks directory, shared with the backend.
        println!("cargo:rustc-link-arg-bin=oink=-Wl,-rpath,@executable_path/../../../../Frameworks");
    }
    #[cfg(any(target_os = "macos", windows))]
    if target_os == "windows" {
        #[cfg(windows)]
        {
            println!("cargo:rerun-if-changed=native/package-drop-windows.cpp");
            println!("cargo:rerun-if-changed=native/package-drop-windows.h");
            cc::Build::new()
                .cpp(true)
                // Keep the receiver standalone on clean Windows installations;
                // C++ containers must not add an MSVCP140.dll prerequisite.
                .static_crt(true)
                .file("native/package-drop-windows.cpp")
                .flag("/std:c++17")
                .compile("oink-package-drop");
            for library in ["ole32", "shell32", "comctl32", "user32", "uuid"] {
                println!("cargo:rustc-link-lib={library}");
            }
        }
        // Link the same Common Controls v6 manifest into every executable,
        // including libtest harnesses. Tauri's binary-only .res manifest leaves
        // library unit tests loading v5, which lacks TaskDialogIndirect and
        // fails before the harness runs on Windows 11 (0xc0000139).
        let manifest =
            std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
        // Avoid a second manifest resource; icons and other Tauri resources
        // remain generated normally. The product's dependency stays identical.
        tauri_build::try_build(
            tauri_build::Attributes::new()
                .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest()),
        )
        .expect("build Windows UI resources");
    } else {
        tauri_build::build();
    }
}
