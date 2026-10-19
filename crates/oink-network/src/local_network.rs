//! Permission evidence from this process's real Bonjour operations; no TCC probe.
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LocalNetworkAccess {
    #[default]
    NotDetermined,
    Granted,
    Denied,
}

static ACCESS: AtomicU8 = AtomicU8::new(0);

pub fn local_network_access() -> LocalNetworkAccess {
    match ACCESS.load(Ordering::Acquire) {
        1 => LocalNetworkAccess::Granted,
        2 => LocalNetworkAccess::Denied,
        _ => LocalNetworkAccess::NotDetermined,
    }
}

pub fn observe_local_network_access(state: LocalNetworkAccess) {
    if ACCESS.swap(state as u8, Ordering::AcqRel) != state as u8 {
        tracing::info!(?state, "local network access observation changed");
    }
}

pub(crate) fn dns_sd_access(error: i32) -> Option<LocalNetworkAccess> {
    match error {
        0 => Some(LocalNetworkAccess::Granted),
        // PolicyDenied and NoAuth are policy evidence. Network failures, empty
        // browse results and starting/retrying an operation say nothing about
        // the user's choice and must not overwrite a known decision.
        -65570 | -65555 => Some(LocalNetworkAccess::Denied),
        _ => None,
    }
}

pub(crate) fn observe_dns_sd(error: i32) {
    if let Some(state) = dns_sd_access(error) {
        observe_local_network_access(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_completed_operations_or_policy_errors_change_the_decision() {
        let mut state = LocalNetworkAccess::NotDetermined;
        for (error, expected) in [
            (-65563, LocalNetworkAccess::NotDetermined),
            (-65570, LocalNetworkAccess::Denied),
            (-65537, LocalNetworkAccess::Denied),
            (0, LocalNetworkAccess::Granted),
            (-65555, LocalNetworkAccess::Denied),
            (0, LocalNetworkAccess::Granted),
        ] {
            if let Some(next) = dns_sd_access(error) {
                state = next;
            }
            assert_eq!(state, expected);
        }
    }
}
