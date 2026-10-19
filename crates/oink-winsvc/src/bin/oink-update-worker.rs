fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    match oink_winsvc::update::run_worker() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
    #[cfg(not(windows))]
    std::process::ExitCode::FAILURE
}
