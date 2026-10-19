fn main() {
    #[cfg(windows)]
    if let Err(error) = oink_foundation::process::initialize_priority() {
        eprintln!("Oink process priority initialization failed: {error}");
        std::process::exit(1);
    }
    #[cfg(windows)]
    if let Err(e) = oink_winsvc::run() {
        eprintln!("Oink input service: {e}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    eprintln!("Oink input service requires Windows");
}
