//! Fixed elevated Windows policy entry. This never requests or bypasses elevation.
use oink_update::policy::{Channel, Command, Mode, Preferences};
#[derive(clap::Args, Debug)]
pub struct Args {
    #[arg(long, value_enum)]
    checks: Toggle,
    #[arg(long, value_enum)]
    mode: PolicyMode,
    #[arg(long, value_enum)]
    channel: PolicyChannel,
}
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum Toggle {
    On,
    Off,
}
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum PolicyMode {
    Notify,
    Automatic,
}
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum PolicyChannel {
    Stable,
    Beta,
}
pub fn run(args: Args) -> anyhow::Result<()> {
    let preferences = Preferences {
        checks_enabled: matches!(args.checks, Toggle::On),
        mode: match args.mode {
            PolicyMode::Notify => Mode::Notify,
            PolicyMode::Automatic => Mode::Automatic,
        },
        channel: match args.channel {
            PolicyChannel::Stable => Channel::Stable,
            PolicyChannel::Beta => Channel::Beta,
        },
    };
    let reply = oink_winsvc::update::request(oink_update::ipc::Request::Command {
        command: Command::SetPreferences { preferences },
    })?;
    if let oink_update::ipc::Reply::Error { error } = reply {
        return Err(error.into());
    }
    crate::cli_output::stdout(format_args!("{}", serde_json::to_string(&reply)?))
}
