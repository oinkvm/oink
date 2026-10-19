//! Read-only authenticated Windows input service health.
use serde_json::json;
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Read-only authenticated service handshake (fails if unavailable).
    Status,
}
pub fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Status => status(),
    }
}

fn status() -> anyhow::Result<()> {
    let s = oink_input::windows_service::probe().map_err(anyhow::Error::msg)?;
    crate::cli_output::stdout(format_args!(
        "{}",
        json!({"service": true, "helper_pid": s.pid, "session": s.session, "desktop": s.desktop, "caps_lock": s.caps_lock, "helper_priority": s.helper_priority})
    ))
}
