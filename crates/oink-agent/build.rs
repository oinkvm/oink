fn main() {
    sparkle();
    println!("cargo:rerun-if-changed=native/permissions.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("native/permissions.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .flag("-mmacosx-version-min=14.2")
            .warnings(true)
            .compile("oink_permissions");
        println!("cargo:rustc-link-lib=framework=Network");
        println!("cargo:rustc-link-lib=framework=Foundation");
    }
    println!("cargo:rerun-if-env-changed=OINK_RELEASE_BUILD");
    println!("cargo:rerun-if-env-changed=RELEASE_BUILD");
    // Release controller exports RELEASE_BUILD for every build/package job.
    // The explicit prefixed variable is also supported for isolated build proofs.
    let build = std::env::var("OINK_RELEASE_BUILD")
        .or_else(|_| std::env::var("RELEASE_BUILD"))
        .unwrap_or_else(|_| "0".into());
    let numeric: u64 = build.parse().expect("OINK_RELEASE_BUILD must be a numeric build");
    assert!(numeric < (1u64 << 31), "release build exceeds MSI range");
    println!("cargo:rustc-env=OINK_RELEASE_BUILD={numeric}");
    println!("cargo:rerun-if-env-changed=OINK_RELEASE_VERSION");
    let version = std::env::var("OINK_RELEASE_VERSION")
        .unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").expect("package version"));
    println!("cargo:rustc-env=OINK_RELEASE_VERSION={version}");
    println!("cargo:rerun-if-env-changed=OINK_SOURCE_TREE_SHA256");
    let tree = std::env::var("OINK_SOURCE_TREE_SHA256").unwrap_or_else(|_| "unrecorded".into());
    assert!(
        tree == "unrecorded"
            || (tree.len() == 64 && tree.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        "OINK_SOURCE_TREE_SHA256 must be 64 lowercase hexadecimal characters"
    );
    println!("cargo:rustc-env=OINK_DIAGNOSTIC_SOURCE_TREE_SHA256={tree}");
}

fn sparkle() {
    println!("cargo:rerun-if-changed=native/update.m");
    println!("cargo:rerun-if-env-changed=SPARKLE_FRAMEWORK_DIR");
    if std::env::var_os("CARGO_FEATURE_SPARKLE").is_none()
        || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }
    let directory = std::env::var("SPARKLE_FRAMEWORK_DIR")
        .expect("Sparkle feature requires the verified Sparkle 2.9.6 distribution directory");
    cc::Build::new()
        .file("native/update.m")
        .flag("-fobjc-arc")
        .flag("-fblocks")
        .flag("-Werror")
        .flag("-mmacosx-version-min=14.2")
        .flag(format!("-F{directory}"))
        .compile("oink_update_macos");
    println!("cargo:rustc-link-search=framework={directory}");
    println!("cargo:rustc-link-lib=framework=Sparkle");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
}
