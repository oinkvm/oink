//! Shared port allocation for isolated network fixtures. Uses no product configuration.
/// Keep isolated fixture ports below the OS ephemeral range. On Windows, two
/// consecutive `bind(:0)` calls can allocate N and N+1 before discovery binds
/// N+1, making one node permanently undiscoverable. Each fixture gets a distinct
/// pair. Low transport listens on TCP at the QUIC port too; probe that socket
/// as well so another fixture's TCP listener/TIME_WAIT cannot break Low startup.
/// Windows can also assign UDP `:0` a port excluded only for TCP, causing Low
/// bootstrap to fail with WSAEACCES on otherwise healthy loopback peers. The
/// shared, jointly probed range prevents that fixture assumption (Windows 11);
/// product binding and unavailable-listener behavior are unchanged.
pub fn test_port_pair() -> u16 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    allocate_port_pair(std::process::id().wrapping_mul(97), &NEXT)
}

fn allocate_port_pair(seed: u32, next: &std::sync::atomic::AtomicU32) -> u16 {
    use std::{
        net::Ipv4Addr,
        sync::{Mutex, atomic::Ordering},
    };
    static LEASES: Mutex<Vec<std::fs::File>> = Mutex::new(Vec::new());
    let directory = std::env::temp_dir().join("oink-test-port-leases");
    std::fs::create_dir_all(&directory).expect("create test port lease directory");
    for _ in 0..10_000 {
        let offset = next.fetch_add(1, Ordering::Relaxed);
        let port = 10_000 + ((seed.wrapping_add(offset) % 10_000) * 2) as u16;
        // Probing sockets alone leaves a gap before the network thread binds.
        // Parallel test binaries can choose the same pair in that gap. Keep an
        // OS lease until process exit, including after a node stops/restarts.
        // Never unlink the file: a waiter must lock the same inode as its owner.
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(port.to_string()))
            .expect("open test port lease");
        match fs2::FileExt::try_lock_exclusive(&lease) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => continue,
            Err(error) => panic!("lock test port lease: {error}"),
        }
        if let Ok(_quic) = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port))
            && let Ok(_discovery) = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port + 1))
            && let Ok(_low) = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        {
            LEASES.lock().unwrap().push(lease);
            return port;
        }
    }
    panic!("no isolated test QUIC/discovery port pair available");
}

#[cfg(test)]
mod tests {
    #[test]
    fn reserved_pair_cannot_be_reallocated_before_network_bind() {
        const CHILD: &str = "OINK_TEST_RESERVED_PORT";
        let next = std::sync::atomic::AtomicU32::new(0);
        // Both processes start at the same candidate, with the parent's real
        // sockets deliberately not bound until the child's allocation finishes.
        let port = super::allocate_port_pair(0, &next);
        if let Ok(parent) = std::env::var(CHILD) {
            assert_ne!(port, parent.parse::<u16>().unwrap());
        } else {
            assert!(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "ports::tests::reserved_pair_cannot_be_reallocated_before_network_bind"
                    ])
                    .env(CHILD, port.to_string())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let _quic = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        let _discovery = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port + 1)).unwrap();
        let _low = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    }
}
