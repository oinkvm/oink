//! Windows send decision, also exercised with fake sends on every platform.
use quinn::udp::Transmit;
use std::io;

pub(super) fn send(
    transmit: &Transmit<'_>,
    mut socket_send: impl FnMut(&Transmit<'_>) -> io::Result<()>,
    on_resend: impl FnOnce(),
) -> io::Result<()> {
    match socket_send(transmit) {
        Err(error)
            if transmit.src_ip.is_some()
                && (error.kind() == io::ErrorKind::InvalidInput
                    || error.raw_os_error() == Some(10022)
                    || error.raw_os_error() == Some(10065) && transmit.segment_size.is_some()) =>
        {
            // Windows can find a source-constrained path without a next hop and
            // reject valid IP_PKTINFO with WSAEINVAL, while an unhinted send works.
            // Native tests also reproduced WSAEHOSTUNREACH
            // (10065) with source hint + UDP segmentation; unhinted segmented sends
            // arrived with the correct source. Only that segmented 10065 case is
            // retried; segmentation is preserved. Its kernel branch is not proven
            // to be the same as the 10022 NULL-next-hop fault.
            // Single-homed paths select the same source; a multi-homed reply may
            // use another address and fail this handshake, which already failed.
            // Keep hints normally for multi-homing/migration; remove this workaround
            // when Windows or quinn-udp fixes it. Retry once, with all other fields intact.
            let unhinted = Transmit {
                src_ip: None,
                ..transmit.clone()
            };
            on_resend();
            socket_send(&unhinted)
        }
        result => result,
    }
}

#[cfg(any(windows, test))]
mod diagnostics {
    use std::{
        net::IpAddr,
        time::{Duration, Instant},
    };

    pub(super) const INTERVAL: Duration = Duration::from_secs(60);

    pub(super) fn destination_class(ip: IpAddr) -> usize {
        match ip {
            IpAddr::V4(ip) if ip.is_loopback() => 0,
            IpAddr::V4(ip) if ip.is_link_local() => 1,
            IpAddr::V4(ip) if ip.is_private() => 2,
            IpAddr::V4(_) => 3,
            IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some() => destination_class(ip.to_ipv4_mapped().unwrap().into()),
            IpAddr::V6(ip) if ip.is_loopback() => 4,
            IpAddr::V6(ip) if ip.is_unicast_link_local() => 5,
            IpAddr::V6(ip) if ip.is_unique_local() => 6,
            IpAddr::V6(_) => 7,
        }
    }

    #[derive(Debug, Default)]
    pub(super) struct Reports {
        seen: u16,
        last: Option<Instant>,
    }
    impl Reports {
        pub(super) fn due(&mut self, class: usize, now: Instant) -> bool {
            let first = self.seen & (1 << class) == 0;
            self.seen |= 1 << class;
            if first
                || self
                    .last
                    .is_none_or(|last| now.saturating_duration_since(last) >= INTERVAL)
            {
                self.last = Some(now);
                true
            } else {
                false
            }
        }
    }

    #[cfg(windows)]
    static RESENDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    #[cfg(windows)]
    static SEGMENTED_RESENDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    #[cfg(windows)]
    static REPORTS: std::sync::Mutex<Reports> = std::sync::Mutex::new(Reports { seen: 0, last: None });

    #[cfg(windows)]
    pub fn resends() -> u64 {
        RESENDS.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(windows)]
    pub fn segmented_resends() -> u64 {
        SEGMENTED_RESENDS.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(windows)]
    pub fn record(destination: std::net::SocketAddr, segmented_host_unreachable: bool) {
        if segmented_host_unreachable {
            SEGMENTED_RESENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        RESENDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let class = destination_class(destination.ip());
        if REPORTS
            .lock()
            .expect("source hint reports")
            .due(class + if segmented_host_unreachable { 8 } else { 0 }, Instant::now())
        {
            let destination_class = [
                "ipv4_loopback",
                "ipv4_link_local",
                "ipv4_private",
                "ipv4_global",
                "ipv6_loopback",
                "ipv6_link_local",
                "ipv6_private",
                "ipv6_global",
            ][class];
            tracing::info!(
                destination_class,
                windows_source_hint_resends = resends(),
                windows_source_hint_segmented_resends = segmented_resends(),
                "Windows rejected UDP source hint; resending without hint"
            );
        }
    }
}
#[cfg(windows)]
pub(super) use diagnostics::{record, resends, segmented_resends};

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, time::Instant};

    fn transmit() -> Transmit<'static> {
        Transmit {
            destination: "127.0.0.1:12345".parse().unwrap(),
            ecn: Some(quinn::udp::EcnCodepoint::Ect0),
            contents: b"onetwo",
            segment_size: Some(3),
            src_ip: Some("127.0.0.1".parse().unwrap()),
        }
    }

    #[test]
    fn error_and_hint_matrix() {
        for hint in [false, true] {
            for kind in [
                io::ErrorKind::InvalidInput,
                io::ErrorKind::WouldBlock,
                io::ErrorKind::Interrupted,
                io::ErrorKind::PermissionDenied,
                io::ErrorKind::AddrNotAvailable,
                io::ErrorKind::Other,
            ] {
                let mut tx = transmit();
                if !hint {
                    tx.src_ip = None;
                }
                let mut calls = 0;
                let retries = Cell::new(0);
                let result = send(
                    &tx,
                    |sent| {
                        calls += 1;
                        assert_eq!(sent.src_ip, if calls == 1 { tx.src_ip } else { None });
                        Err(io::Error::new(kind, "fake send"))
                    },
                    || retries.set(retries.get() + 1),
                );
                let retry = hint && kind == io::ErrorKind::InvalidInput;
                assert_eq!(calls, if retry { 2 } else { 1 });
                assert_eq!(retries.get(), usize::from(retry));
                assert_eq!(result.unwrap_err().kind(), kind);
            }
        }
    }

    #[test]
    fn one_resend_preserves_payload_and_propagates_its_result() {
        for second_error in [None, Some(10022), Some(10065), Some(10049), Some(10035)] {
            let tx = transmit();
            let mut calls = 0;
            let result = send(
                &tx,
                |sent| {
                    calls += 1;
                    assert_eq!(sent.destination, tx.destination);
                    assert_eq!(sent.ecn, tx.ecn);
                    assert!(std::ptr::eq(sent.contents, tx.contents));
                    assert_eq!(sent.segment_size, tx.segment_size);
                    if calls == 1 {
                        return Err(io::Error::from_raw_os_error(10022));
                    }
                    assert_eq!(sent.src_ip, None);
                    second_error.map_or(Ok(()), |code| Err(io::Error::from_raw_os_error(code)))
                },
                || {},
            );
            assert_eq!(calls, 2);
            assert_eq!(result.err().and_then(|error| error.raw_os_error()), second_error);
        }
    }

    #[test]
    fn segmented_host_unreachable_resends_only_with_hint_and_preserves_segmentation() {
        for code in [10065, 10051, 10049, 10013] {
            for hint in [false, true] {
                for segmented in [false, true] {
                    let mut tx = transmit();
                    if !hint {
                        tx.src_ip = None;
                    }
                    if !segmented {
                        tx.segment_size = None;
                    }
                    let mut calls = 0;
                    let retries = Cell::new(0);
                    let result = send(
                        &tx,
                        |sent| {
                            calls += 1;
                            assert_eq!(sent.segment_size, tx.segment_size);
                            assert_eq!(sent.destination, tx.destination);
                            assert_eq!(sent.ecn, tx.ecn);
                            assert!(std::ptr::eq(sent.contents, tx.contents));
                            if calls == 1 {
                                return Err(io::Error::from_raw_os_error(code));
                            }
                            assert_eq!(sent.src_ip, None);
                            Ok(())
                        },
                        || retries.set(retries.get() + 1),
                    );
                    let retry = code == 10065 && hint && segmented;
                    assert_eq!(calls, if retry { 2 } else { 1 });
                    assert_eq!(retries.get(), usize::from(retry));
                    assert_eq!(result.is_ok(), retry);
                }
            }
        }
    }

    #[test]
    fn successful_send_is_not_repeated() {
        let mut calls = 0;
        send(
            &transmit(),
            |_| {
                calls += 1;
                Ok(())
            },
            || panic!("unexpected resend"),
        )
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn first_per_class_then_one_report_per_minute() {
        use diagnostics::*;
        let now = Instant::now();
        let mut reports = Reports::default();
        for (ip, class) in [
            ("127.0.0.1", 0),
            ("169.254.1.1", 1),
            ("10.0.0.1", 2),
            ("203.0.113.1", 3),
            ("::1", 4),
            ("fe80::1", 5),
            ("fd00::1", 6),
            ("2001:db8::1", 7),
        ] {
            assert_eq!(destination_class(ip.parse().unwrap()), class);
            assert!(reports.due(class, now));
            assert!(!reports.due(class, now));
        }
        assert!(!reports.due(0, now + INTERVAL - std::time::Duration::from_millis(1)));
        assert!(reports.due(0, now + INTERVAL));
        assert!(!reports.due(1, now + INTERVAL));
    }
}
