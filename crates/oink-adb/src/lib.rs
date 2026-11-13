//! Wireless ADB client with an explicit Oink key set and owned sockets.
//! It never reads user Android SDK state and never starts an adb server.
mod key;
mod pairing;
mod spake;
mod tls;
mod transport;

pub use key::AdbKey;
pub use pairing::{PairingPeer, pair};
pub use transport::{Client, ShellOutput};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ADB transport failed")]
    Io(#[from] std::io::Error),
    #[error("invalid ADB protocol message")]
    Protocol,
    #[error("ADB authentication failed")]
    Authentication,
    #[error("invalid ADB credential")]
    Key,
    #[error("ADB shell exited with code {0}")]
    Shell(u8),
    #[error("ADB payload exceeds the supported limit")]
    Limit,
}
pub type Result<T> = std::result::Result<T, Error>;

pub async fn start_helper(address: std::net::SocketAddr, keys: &[AdbKey], apk_path: &str) -> Result<()> {
    let command = helper_command(apk_path)?;
    let mut client = Client::connect(address, keys).await?;
    let result = client.shell(&command).await?;
    if result.exit_code != 0 {
        return Err(Error::Shell(result.exit_code));
    }
    Ok(())
}

pub fn helper_command(apk_path: &str) -> Result<String> {
    if !apk_path.starts_with('/') || apk_path.contains(['\'', '\n', '\r', '\0']) {
        return Err(Error::Protocol);
    }
    Ok(format!(
        "CLASSPATH='{apk_path}' app_process / com.hachiwii.oink.privilege.HelperMain </dev/null >/dev/null 2>&1 &"
    ))
}
