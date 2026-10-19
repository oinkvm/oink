//! macOS bundle entry, separate from the IPC-only UI executable.
fn main() -> std::process::ExitCode {
    oink_agent::telemetry::fatal_boundary(run)
}

fn run() -> std::process::ExitCode {
    if let Err(error) = oink_foundation::process::initialize_priority() {
        eprintln!("Oink process priority initialization failed: {error}");
        return std::process::ExitCode::FAILURE;
    }
    #[cfg(target_os = "macos")]
    if std::env::args().any(|arg| arg == "--launch-smoke") {
        return std::process::ExitCode::from(oink_agent::backend::run_launch_smoke());
    }
    let launch = if std::env::args().any(|arg| arg == oink_agent::backend::REOPEN_FLAG) {
        oink_agent::backend::Launch::Reopen
    } else {
        oink_agent::backend::Launch::User
    };
    std::process::ExitCode::from(oink_agent::backend::run(launch))
}
