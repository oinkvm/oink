fn main() {
    for key in [
        "OINK_REPORT_DSN",
        "OINK_REPORT_REGION",
        "OINK_SOURCE_COMMIT",
        "OINK_RELEASE_VERSION",
        "OINK_RELEASE_BUILD",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
}
