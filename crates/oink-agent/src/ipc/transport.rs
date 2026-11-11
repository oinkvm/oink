//! Native user-local transport. Legacy TCP endpoints remain readable for upgrades.
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
#[path = "transport/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "transport/windows.rs"]
mod platform;
pub(crate) use platform::{AsyncStream, Listener, peer};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owner {
    pub pid: u32,
    pub nonce: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Address {
    Native { path: String, owner: Owner, lock: PathBuf },
    Legacy { port: u16 },
}
impl Address {
    pub(crate) fn connect(&self, timeout: Duration) -> io::Result<Stream> {
        match self {
            Self::Native { path, owner, lock } => {
                verify_owner(lock, owner)?;
                let stream = platform::connect(path, owner.pid, timeout)?;
                verify_owner(lock, owner)?;
                Ok(Stream::Native(stream))
            }
            Self::Legacy { port } => {
                TcpStream::connect_timeout(&([127, 0, 0, 1], *port).into(), timeout).map(Stream::Legacy)
            }
        }
    }
    /// Probe only: a live endpoint without an owner must never receive a command.
    pub(crate) fn probe(&self) -> io::Result<()> {
        match self {
            Self::Native { path, .. } => platform::probe(path),
            Self::Legacy { port } => {
                TcpStream::connect_timeout(&([127, 0, 0, 1], *port).into(), Duration::from_secs(3)).map(drop)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct OwnerLock {
    _file: File,
    pub owner: Owner,
    pub path: PathBuf,
}
impl OwnerLock {
    pub(crate) fn acquire(data_dir: &Path, role: &str) -> io::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join(format!("{role}.transport.lock"));
        // Never replace this inode. Windows's sharing mode also forbids deletion
        // while it is owned. A separate IPC lock covers embedded/fake hosts too.
        match oink_foundation::storage::create_private_new(&path) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(3);
        }
        let mut file = options.open(&path)?;
        platform::lock_owner(&file)?;
        let mut secret = [0u8; 16];
        getrandom::fill(&mut secret).map_err(|e| io::Error::other(e.to_string()))?;
        let owner = Owner {
            pid: std::process::id(),
            nonce: secret.iter().map(|b| format!("{b:02x}")).collect(),
        };
        file.set_len(0)?;
        file.write_all(&serde_json::to_vec(&owner)?)?;
        file.sync_all()?;
        Ok(Self {
            _file: file,
            owner,
            path,
        })
    }
}
impl OwnerLock {
    pub(crate) fn release(&self) {
        let _ = platform::unlock_owner(&self._file);
    }
}
impl Drop for OwnerLock {
    fn drop(&mut self) {
        let _ = platform::unlock_owner(&self._file);
    }
}
fn verify_owner(path: &Path, expected: &Owner) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(3);
    }
    let mut file = options.open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&file)?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC endpoint has no instance-lock owner",
            ));
        }
        Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
        Err(e) => return Err(e),
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let actual: Owner = serde_json::from_slice(&bytes)?;
    if actual != *expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC endpoint owner changed",
        ));
    }
    Ok(())
}

/// Blocking local stream, shared by the UI's reader and explicit cancellation.
#[derive(Debug)]
pub enum Stream {
    #[doc(hidden)]
    Native(platform::Stream),
    #[doc(hidden)]
    Legacy(TcpStream),
}
impl Stream {
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Native(s) => s.set_read_timeout(timeout),
            Self::Legacy(s) => s.set_read_timeout(timeout),
        }
    }
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Native(s) => s.set_write_timeout(timeout),
            Self::Legacy(s) => s.set_write_timeout(timeout),
        }
    }
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Native(s) => s.shutdown(how),
            Self::Legacy(s) => s.shutdown(how),
        }
    }
}
// macOS cannot create a socket close-on-exec atomically: a child process
// spawned between socket() and fcntl(FD_CLOEXEC) keeps a copy of the client's
// descriptor for its whole life, so closing ours would not tell the backend
// this client left, and it would still run a request nobody waits for.
// shutdown acts on the socket rather than on one descriptor, so the backend
// sees the client leave however many copies exist.
#[cfg(unix)]
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}
impl Read for &Stream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Native(s) => Read::read(&mut &*s, out),
            Stream::Legacy(s) => Read::read(&mut &*s, out),
        }
    }
}
impl Write for &Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Native(s) => Write::write(&mut &*s, bytes),
            Stream::Legacy(s) => Write::write(&mut &*s, bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Read for Stream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        Read::read(&mut &*self, out)
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Write::write(&mut &*self, bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_endpoint_checks_lock_owner_and_rejects_a_second_listener() {
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "agent").unwrap();
        let address = listener.address();
        assert!(Listener::bind(directory.path(), "agent").is_err());
        let mut stale = address.clone();
        if let Address::Native { owner, .. } = &mut stale {
            owner.pid = owner.pid.wrapping_add(1);
        }
        assert_eq!(
            stale.connect(Duration::from_secs(1)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let stream = address.connect(Duration::from_secs(1)).unwrap();
        drop(stream);
        listener.release();
        assert_eq!(
            address.connect(Duration::from_secs(1)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // Even with no owner, a live listener is NOT evidence of a successful
        // stop. Probe it without transmitting a token or command.
        assert!(address.probe().is_ok());
        drop(listener);
        tokio::task::yield_now().await;
        assert!(address.probe().is_err());
    }
    #[tokio::test]
    async fn peer_is_closed_only_once_the_client_has_left() {
        use tokio::io::AsyncReadExt;
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "peer").unwrap();
        let mut client = listener.address().connect(Duration::from_secs(1)).unwrap();
        client.write_all(b"x").unwrap();
        let (mut stream, ()) = listener.accept().await.unwrap();
        let peer = peer(&stream).unwrap();
        assert!(!peer.closed(), "a connected client with unread data");
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        assert!(!peer.closed(), "a connected client that is waiting");
        drop(client);
        assert!(peer.closed());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn a_client_that_leaves_is_seen_while_a_child_still_holds_its_descriptor() {
        use std::os::fd::AsFd;
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "inherited").unwrap();
        let client = listener.address().connect(Duration::from_secs(1)).unwrap();
        let (stream, ()) = listener.accept().await.unwrap();
        let peer = peer(&stream).unwrap();
        // A child spawned before the descriptor became close-on-exec keeps a
        // copy; a duplicate has the same effect without forking.
        let Stream::Native(native) = &client else {
            unreachable!()
        };
        let inherited = native.as_fd().try_clone_to_owned().unwrap();
        drop(client);
        assert!(peer.closed());
        drop(inherited);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_is_private_even_with_a_long_data_directory() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let long = directory.path().join("long-directory-".repeat(10));
        std::fs::create_dir_all(&long).unwrap();
        let listener = Listener::bind(&long, "agent").unwrap();
        let Address::Native { path, .. } = listener.address() else {
            unreachable!()
        };
        assert!(path.len() < 104);
        assert_eq!(
            std::fs::metadata(long.join("ipc")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        listener.address().connect(Duration::from_secs(1)).unwrap();
        drop(listener);
        assert!(!Path::new(&path).exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_shared_or_symlinked_socket_directory() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ipc");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Listener::bind(directory.path(), "agent").is_err());
        std::fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(directory.path(), &path).unwrap();
        assert!(Listener::bind(directory.path(), "agent").is_err());
    }
}
