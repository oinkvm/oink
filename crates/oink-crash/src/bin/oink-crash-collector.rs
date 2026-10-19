fn main() {
    #[cfg(windows)]
    if oink_crash::collector::run().is_err() {
        std::process::exit(1);
    }
}
