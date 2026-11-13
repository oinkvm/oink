//! This device's IPv4 addresses, refreshed on the operating system's own
//! interface/address change notifications instead of a polling timer.
//! Enumerating interfaces costs milliseconds on machines with many adapters,
//! so it happens on this watcher's thread, never on the realtime network
//! thread; the owner hands the list to the link layer
//! (`LinkLayer::set_local_addresses`).
//!
//! Desktop only. Phones get the same event from their host (the system's
//! network path monitor) through the mobile network owner.
use std::net::Ipv4Addr;
use std::sync::OnceLock;
use tokio::sync::watch;

static WATCH: OnceLock<watch::Sender<Vec<Ipv4Addr>>> = OnceLock::new();

/// The current address list, updated after every system notification that
/// changes it. One watcher thread per process serves every subscriber.
pub fn local_addresses() -> watch::Receiver<Vec<Ipv4Addr>> {
    WATCH
        .get_or_init(|| {
            let (sender, _) = watch::channel(current());
            let watcher = sender.clone();
            let started = std::thread::Builder::new()
                .name("oink-address-watch".into())
                .spawn(move || run(&watcher));
            if let Err(error) = started {
                tracing::warn!(%error, "address change watcher did not start; local addresses stay as read at start-up");
            }
            sender
        })
        .subscribe()
}

fn current() -> Vec<Ipv4Addr> {
    super::local_ipv4().into_iter().map(|(ip, _)| ip).collect()
}

fn run(sender: &watch::Sender<Vec<Ipv4Addr>>) {
    let mut notifier = match Notifier::new() {
        Ok(notifier) => notifier,
        Err(error) => {
            tracing::warn!(%error, "system address notifications unavailable; local addresses stay as read at start-up");
            return;
        }
    };
    // Read once more: a change between the first read and the subscription
    // would otherwise wait for the next notification.
    refresh(sender);
    loop {
        if let Err(error) = notifier.wait() {
            tracing::warn!(%error, "system address notifications ended; local addresses stay as last read");
            return;
        }
        refresh(sender);
    }
}

fn refresh(sender: &watch::Sender<Vec<Ipv4Addr>>) {
    let addresses = current();
    sender.send_if_modified(|old| {
        if *old == addresses {
            return false;
        }
        tracing::info!(target: "oink_link_diagnostics", kind = "local_addresses", addresses = ?addresses,
            "Local addresses changed");
        *old = addresses;
        true
    });
}

/// macOS: a routing socket reports address and interface changes as
/// RTM_NEWADDR/RTM_DELADDR/RTM_IFINFO messages (route(4)); other routing
/// messages (route add/remove, ARP) are skipped without enumerating.
#[cfg(target_os = "macos")]
struct Notifier {
    fd: std::os::fd::OwnedFd,
    buffer: Vec<u8>,
}
#[cfg(target_os = "macos")]
impl Notifier {
    fn new() -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain socket(2) call; the descriptor is owned below.
        let fd = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, libc::AF_UNSPEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: `fd` is a fresh descriptor that nothing else owns.
            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) },
            buffer: vec![0; 8192],
        })
    }
    fn wait(&mut self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: reads into a buffer this struct owns, within its length.
            let read = unsafe { libc::read(self.fd.as_raw_fd(), self.buffer.as_mut_ptr().cast(), self.buffer.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            // struct rt_msghdr starts with u_short rtm_msglen, u_char
            // rtm_version, u_char rtm_type; one read returns one message.
            if read >= 4
                && matches!(
                    i32::from(self.buffer[3]),
                    libc::RTM_NEWADDR | libc::RTM_DELADDR | libc::RTM_IFINFO
                )
            {
                return Ok(());
            }
        }
    }
}

/// Windows (NotifyIpInterfaceChange) and Linux (rtnetlink address groups).
#[cfg(any(windows, target_os = "linux"))]
struct Notifier(if_addrs::IfChangeNotifier);
#[cfg(any(windows, target_os = "linux"))]
impl Notifier {
    fn new() -> std::io::Result<Self> {
        if_addrs::IfChangeNotifier::new().map(Self)
    }
    fn wait(&mut self) -> std::io::Result<()> {
        self.0.wait(None).map(|_| ())
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
struct Notifier;
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
impl Notifier {
    fn new() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the host reports network changes on this platform",
        ))
    }
    fn wait(&mut self) -> std::io::Result<()> {
        unreachable!()
    }
}
