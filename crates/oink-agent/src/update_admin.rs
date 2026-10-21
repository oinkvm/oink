//! Fixed elevated Windows policy entry. This never requests or bypasses elevation.
use oink_update::policy::{Command, Preferences};
#[derive(clap::Args, Debug)]
pub struct Args {
    #[arg(long, value_enum)]
    checks: Toggle,
}
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum Toggle {
    On,
    Off,
}
pub fn run(args: Args) -> anyhow::Result<()> {
    let preferences = Preferences {
        checks_enabled: matches!(args.checks, Toggle::On),
    };
    let reply = oink_winsvc::update::request(oink_update::ipc::Request::Command {
        command: Command::SetPreferences { preferences },
    })?;
    if let oink_update::ipc::Reply::Error { error } = reply {
        return Err(error.into());
    }
    crate::cli_output::stdout(format_args!("{}", serde_json::to_string(&reply)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)]
    struct PolicyCli {
        #[command(flatten)]
        args: Args,
    }
    #[test]
    fn policy_accepts_only_checks() {
        for checks in ["on", "off"] {
            assert!(PolicyCli::try_parse_from(["policy", "--checks", checks]).is_ok());
        }
        for channel in ["stable", "beta"] {
            assert!(PolicyCli::try_parse_from(["policy", "--checks", "on", "--channel", channel]).is_err());
        }
        for mode in ["notify", "automatic"] {
            assert!(PolicyCli::try_parse_from(["policy", "--checks", "on", "--mode", mode]).is_err());
        }
    }
}
