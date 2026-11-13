fn main() {
    println!("cargo:rerun-if-changed=src/logging/os_log.c");
    // os_log is a C macro API (the format string must live in the binary's
    // os_log section), so iOS logs go through a two-function C shim.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        cc::Build::new().file("src/logging/os_log.c").compile("oink_os_log");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }
}
