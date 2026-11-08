fn main() {
    println!("cargo:rerun-if-changed=native/objc-exceptions.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("native/objc-exceptions.m")
            .flag("-fno-objc-arc")
            .flag("-fobjc-exceptions")
            .flag("-fexceptions")
            .flag("-Wall")
            .flag("-Wextra")
            .flag("-Werror")
            .compile("oink_objc_exceptions");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=c++");
    }
}
