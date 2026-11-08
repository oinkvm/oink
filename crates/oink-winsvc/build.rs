fn main() {
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-changed=../oink-credential-provider/ipc.hpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new()
            .cpp(true)
            // Match the UI package-drop receiver: mixed /MD and /MT objects
            // fail LNK2038 when the two native libraries share an executable.
            // Static CRT also avoids adding an MSVCP140 runtime prerequisite.
            .static_crt(true)
            .std("c++17")
            .file("native/unlock.cpp")
            .include("../oink-credential-provider")
            .flag_if_supported("/EHsc")
            .compile("oink_auto_unlock");
        for lib in [
            "advapi32", "wtsapi32", "crypt32", "user32", "shell32", "ole32", "userenv",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}
