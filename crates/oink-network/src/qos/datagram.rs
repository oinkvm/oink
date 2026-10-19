//! Darwin datagram errors must not retire a QUIC connection prematurely.
use quinn::udp::Transmit;
use std::io;

pub(super) fn send_result(result: io::Result<()>, transmit: &Transmit<'_>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::HostUnreachable => {
            // On macOS 26.6.2, two captured starts after app replacement rejected
            // local-subnet sends while nehelper refreshed the already-allowed
            // app's network-privacy UUID cache.
            // Returning Err exits Quinn's driver as LocallyClosed, removing the
            // reachable candidate while another address waits out its deadline.
            // Treat this send as UDP packet loss, as Quinn's default socket does:
            // its PTO retries on the same socket within the existing dial budget.
            // Keep the actual error visible; do not change routes, source hints,
            // interfaces or deadlines. Other errors retain their existing handling.
            tracing::warn!(target: "oink_link_diagnostics", kind = "udp_send",
                stage = "kernel", result = "dropped", reason = "host_unreachable",
                destination = %transmit.destination, src_ip = ?transmit.src_ip,
                os_error = error.raw_os_error().unwrap_or_default(),
                os_error_known = error.raw_os_error().is_some(), %error,
                "UDP host unreachable; QUIC loss recovery remains active");
            Ok(())
        }
        result => result,
    }
}

#[cfg(test)]
mod tests;
