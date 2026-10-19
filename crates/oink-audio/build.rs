fn main() {
    println!("cargo:rerun-if-changed=native/realtime.c");
    println!("cargo:rerun-if-changed=native/media_pause.m");
    println!("cargo:rerun-if-changed=native/macos.m");
    println!("cargo:rerun-if-changed=native/macos_mute.m");
    println!("cargo:rerun-if-changed=native/capture_permission.m");
    println!("cargo:rerun-if-changed=native/macos_playback.m");
    println!("cargo:rerun-if-changed=native/macos_devices.m");
    println!("cargo:rerun-if-changed=native/power.m");
    println!("cargo:rerun-if-changed=native/exit_volume.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("native/realtime.c")
            .flag("-std=c11")
            .flag("-mmacosx-version-min=11.0")
            .warnings(true)
            .compile("oink_audio_realtime");
        let mut native = cc::Build::new();
        native
            .file("native/macos.m")
            .file("native/macos_mute.m")
            .file("native/media_pause.m")
            .file("native/capture_permission.m")
            .file("native/macos_playback.m")
            .file("native/macos_devices.m")
            .file("native/power.m")
            .file("native/exit_volume.m")
            .flag("-fobjc-arc")
            .flag("-std=c11")
            // Compile with weak references to process-tap entry points. The
            // bridge checks availability before calling them at runtime.
            .flag("-mmacosx-version-min=11.0")
            .warnings(true)
            .compile("oink_audio_capture");
        // Rust links with -nodefaultlibs, so Clang cannot automatically add
        // the runtime that implements Objective-C @available checks. Debug
        // builds can mask this when their deployment target folds the check.
        // Ask the selected compiler instead of hard-coding an Xcode location.
        let runtime = native
            .get_compiler()
            .to_command()
            .arg("-print-file-name=libclang_rt.osx.a")
            .output()
            .expect("query Clang's macOS availability runtime");
        assert!(runtime.status.success(), "Clang runtime query failed");
        let runtime = String::from_utf8(runtime.stdout).expect("Clang runtime path is UTF-8");
        let runtime = std::path::Path::new(runtime.trim());
        assert!(runtime.is_file(), "Clang macOS runtime archive is missing");
        println!(
            "cargo:rustc-link-search=native={}",
            runtime.parent().expect("runtime archive directory").display()
        );
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
        println!("cargo:rustc-link-lib=framework=CoreAudio");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
    }
}
