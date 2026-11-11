use super::*;
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    net::UnixStream,
};
pub type Stream = UnixStream;
pub(crate) type AsyncStream = tokio::net::UnixStream;

#[derive(Debug)]
pub(crate) struct Listener {
    listener: tokio::net::UnixListener,
    owner: OwnerLock,
    path: PathBuf,
    socket: PathBuf,
    _alias: tempfile::TempDir,
}
impl Listener {
    pub(crate) fn bind(data_dir: &Path, role: &str) -> io::Result<Self> {
        let owner = OwnerLock::acquire(data_dir, role)?;
        let directory = data_dir.join("ipc");
        match std::fs::create_dir(&directory) {
            Ok(()) => std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let meta = std::fs::symlink_metadata(&directory)?;
        // SAFETY: geteuid has no preconditions and returns the effective uid.
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC directory must be owned by this user with mode 0700",
            ));
        }
        // Darwin sun_path is only 104 bytes. The socket itself lives in the data
        // directory; a private short-lived symlink gives bind/connect a short
        // lookup path, even with a long home or OINK_HOME. No global chdir/umask.
        let alias = tempfile::Builder::new().prefix("oi-").tempdir_in("/tmp")?;
        std::os::unix::fs::symlink(std::fs::canonicalize(&directory)?, alias.path().join("d"))?;
        let name = format!("{role}-{}.sock", owner.owner.nonce);
        let path = alias.path().join("d").join(&name);
        let socket = directory.join(name);
        let listener = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            owner,
            path,
            socket,
            _alias: alias,
        })
    }
    pub(crate) fn release(&self) {
        self.owner.release();
    }
    pub(crate) fn address(&self) -> Address {
        Address::Native {
            path: self.path.to_string_lossy().into(),
            owner: self.owner.owner.clone(),
            lock: self.owner.path.clone(),
        }
    }
    pub(crate) async fn accept(&self) -> io::Result<(AsyncStream, ())> {
        let (stream, _) = self.listener.accept().await?;
        // File permissions are not a portable BSD authorization boundary.
        // Check credentials as well as the private directory and secret.
        // A credential failure belongs to this accepted connection (for
        // example a probe that closed before accept), not to the listener.
        // Reject only that stream so a disconnected probe cannot stop IPC.
        let credentials = stream.peer_cred().map_err(|error| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("cannot authenticate IPC peer: {error}"),
            )
        })?;
        // SAFETY: geteuid has no preconditions.
        if credentials.uid() != unsafe { libc::geteuid() } {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok((stream, ()))
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}
/// Whether the client of one accepted connection has closed its end.
#[derive(Debug)]
pub(crate) struct Peer(std::os::fd::OwnedFd);
pub(crate) fn peer(stream: &AsyncStream) -> io::Result<Peer> {
    use std::os::fd::AsFd;
    stream.as_fd().try_clone_to_owned().map(Peer)
}
impl Peer {
    pub(crate) fn closed(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut byte = 0u8;
        // SAFETY: live socket and a one-byte buffer; MSG_PEEK consumes nothing.
        let read = unsafe {
            libc::recv(
                self.0.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        // 0 is the peer's orderly close. Unread data or "would block" mean it
        // is still there; any other error means the connection is gone.
        read == 0
            || (read < 0
                && !matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ))
    }
}
pub(crate) fn connect(path: &str, _pid: u32, timeout: Duration) -> io::Result<Stream> {
    use std::os::fd::{AsRawFd, OwnedFd};
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&socket2::SockAddr::unix(path)?) {
        Ok(()) => {}
        Err(error) if matches!(error.raw_os_error(), Some(libc::EINPROGRESS) | Some(libc::EAGAIN)) => {
            let mut fd = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: one live socket descriptor and correctly sized pollfd.
            let ready = unsafe { libc::poll(&mut fd, 1, ms) };
            if ready == 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            if ready < 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(error) = socket.take_error()? {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    }
    socket.set_nonblocking(false)?;
    let descriptor: OwnedFd = socket.into();
    let stream = Stream::from(descriptor);
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: live connected socket and valid uid/gid outputs.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: geteuid has no preconditions.
        if uid != unsafe { libc::geteuid() } {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(stream)
}
pub(crate) fn probe(path: &str) -> io::Result<()> {
    Stream::connect(path).map(drop)
}

pub(crate) fn lock_owner(file: &File) -> io::Result<()> {
    file.try_lock_exclusive()
}
pub(crate) fn unlock_owner(file: &File) -> io::Result<()> {
    FileExt::unlock(file)
}
