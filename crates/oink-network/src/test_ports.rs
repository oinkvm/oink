//! Socket reservations for isolated fixtures; compiled only for tests.
//!
//! A number and a file lock are not a socket reservation. Other binaries can
//! have different temporary directories, and ordinary bind(:0) does not consult
//! file locks at all. Keep the actual sockets until their owners take them.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

#[derive(Default)]
struct Reservations {
    udp: HashMap<SocketAddr, UdpSocket>,
    tcp: HashMap<SocketAddr, TcpListener>,
    leases: Vec<std::fs::File>,
}
fn reservations() -> &'static Mutex<Reservations> {
    static RESERVED: OnceLock<Mutex<Reservations>> = OnceLock::new();
    RESERVED.get_or_init(Default::default)
}

/// Reserve QUIC UDP, adjacent discovery UDP and same-number Low TCP sockets.
/// Keep the range below the default macOS/Linux/Windows ephemeral ranges;
/// Windows can exclude a UDP-assigned ephemeral port from TCP. Every block
/// also leaves room for the agent's IPC listener at base + 2.
pub fn reserve(bind_ip: IpAddr) -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    allocate(bind_ip, std::process::id().wrapping_mul(97), &NEXT)
}
fn allocate(bind_ip: IpAddr, seed: u32, next: &AtomicU32) -> u16 {
    let directory = std::env::temp_dir().join("oink-test-port-leases");
    std::fs::create_dir_all(&directory).expect("create test port lease directory");
    for _ in 0..5_000 {
        let offset = next.fetch_add(1, Ordering::Relaxed);
        let port = 10_000 + ((seed.wrapping_add(offset) % 5_000) * 4) as u16;
        // Retain the existing per-pair file leases through process exit. They
        // prevent reuse after shutdown, including old binaries using stride 2.
        // Never unlink: another process must lock the same inode.
        let mut leases = Vec::new();
        for base in [port, port + 2] {
            let lease = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join(base.to_string()))
                .expect("open test port lease");
            match fs2::FileExt::try_lock_exclusive(&lease) {
                Ok(()) => leases.push(lease),
                Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => break,
                Err(error) => panic!("lock test port lease: {error}"),
            }
        }
        if leases.len() != 2 {
            continue;
        }
        let quic_addr = SocketAddr::new(bind_ip, port);
        let discovery_addr = SocketAddr::new(bind_ip, port + 1);
        if let Ok(quic) = UdpSocket::bind(quic_addr)
            && let Ok(discovery) = UdpSocket::bind(discovery_addr)
            && let Ok(low) = TcpListener::bind(quic_addr)
            && let Ok(_ipc) = TcpListener::bind((Ipv4Addr::LOCALHOST, port + 2))
        {
            let mut reserved = reservations().lock().unwrap();
            assert!(reserved.udp.insert(quic_addr, quic).is_none());
            assert!(reserved.udp.insert(discovery_addr, discovery).is_none());
            assert!(reserved.tcp.insert(quic_addr, low).is_none());
            reserved.leases.extend(leases);
            return port;
        }
    }
    panic!("no isolated test QUIC/discovery/Low port block available");
}

pub(crate) fn take_udp(address: SocketAddr) -> Option<UdpSocket> {
    reservations().lock().unwrap().udp.remove(&address)
}
/// Take the Low reservation, also used by tests deliberately blocking Low
/// before LinkLayer startup. Dropping it releases the real socket.
pub fn take_tcp(address: SocketAddr) -> Option<TcpListener> {
    reservations().lock().unwrap().tcp.remove(&address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockets_remain_exclusive_through_handoff_and_leases_prevent_reuse() {
        let ip = Ipv4Addr::LOCALHOST.into();
        let next = AtomicU32::new(0);
        let port = allocate(ip, 0, &next);
        let quic = SocketAddr::new(ip, port);
        let discovery = SocketAddr::new(ip, port + 1);
        assert!(UdpSocket::bind(quic).is_err());
        assert!(UdpSocket::bind(discovery).is_err());
        assert!(TcpListener::bind(quic).is_err());
        let sockets = (
            take_udp(quic).unwrap(),
            take_udp(discovery).unwrap(),
            take_tcp(quic).unwrap(),
        );
        assert!(UdpSocket::bind(quic).is_err());
        assert!(UdpSocket::bind(discovery).is_err());
        assert!(TcpListener::bind(quic).is_err());
        drop(sockets);
        let _quic = UdpSocket::bind(quic).unwrap();
        let _discovery = UdpSocket::bind(discovery).unwrap();
        let _low = TcpListener::bind(quic).unwrap();
        drop((_quic, _discovery, _low));
        let seed = u32::from((port - 10_000) / 4);
        assert_ne!(allocate(ip, seed, &AtomicU32::new(0)), port);
    }

    #[test]
    fn concurrent_allocators_cannot_take_another_threads_pending_sockets() {
        let ports = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| allocate(Ipv4Addr::LOCALHOST.into(), 0, &AtomicU32::new(0))))
                .collect();
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .collect::<std::collections::HashSet<_>>()
        });
        assert_eq!(ports.len(), 16);
        for port in &ports {
            assert!(
                !ports.contains(&(port + 2)),
                "a neighbor must not occupy IPC at base + 2"
            );
        }
    }

    #[test]
    fn another_process_with_a_different_temp_dir_cannot_steal_pending_sockets() {
        const CHILD: &str = "OINK_TEST_RESERVED_PORT";
        let ip = Ipv4Addr::LOCALHOST.into();
        if let Ok(parent) = std::env::var(CHILD) {
            let parent: u16 = parent.parse().unwrap();
            assert_ne!(
                std::env::temp_dir(),
                std::path::PathBuf::from(std::env::var_os("OINK_TEST_PARENT_TMP").unwrap())
            );
            assert!(UdpSocket::bind((ip, parent)).is_err());
            assert!(UdpSocket::bind((ip, parent + 1)).is_err());
            assert!(TcpListener::bind((ip, parent)).is_err());
            let seed = u32::from((parent - 10_000) / 4);
            assert_ne!(allocate(ip, seed, &AtomicU32::new(0)), parent);
        } else {
            let port = allocate(ip, 0, &AtomicU32::new(0));
            let dir = tempfile::tempdir().unwrap();
            // Use the actual module path: a hard-coded support-crate path used
            // to silently select zero tests when included in oink-network.
            let name = format!(
                "{}::another_process_with_a_different_temp_dir_cannot_steal_pending_sockets",
                module_path!()
            );
            let name = name.split_once("::").unwrap().1;
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env(CHILD, port.to_string())
                .env("OINK_TEST_PARENT_TMP", std::env::temp_dir())
                .env("TMPDIR", dir.path())
                .env("TMP", dir.path())
                .env("TEMP", dir.path())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                stdout.contains("1 passed"),
                "child must execute the regression: {stdout}"
            );
        }
    }
}
