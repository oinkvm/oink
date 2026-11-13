//! Event-driven OS discovery injection. TXT and addresses never authorize a peer.
use super::{Sighting, Source, advert_from_txt};
use std::{collections::BTreeMap, net::SocketAddr};
use tokio::sync::watch;

/// Full current view makes withdrawal and network changes observable without timers.
#[derive(Clone, Debug, Default)]
pub struct NativeDiscoverySnapshot {
    pub generation: u64,
    pub services: BTreeMap<String, Sighting>,
}

#[derive(Debug)]
pub struct NativeDiscovery {
    state: watch::Sender<NativeDiscoverySnapshot>,
}
impl Default for NativeDiscovery {
    fn default() -> Self {
        Self::new()
    }
}
impl NativeDiscovery {
    pub fn new() -> Self {
        Self {
            state: watch::channel(NativeDiscoverySnapshot::default()).0,
        }
    }
    pub fn snapshot(&self) -> NativeDiscoverySnapshot {
        self.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<NativeDiscoverySnapshot> {
        self.state.subscribe()
    }
    /// Call when the native browser restarts for a new network path; the network
    /// owner keeps running. Late callbacks from an older browser generation cannot
    /// repopulate the new candidate set.
    pub fn network_changed(&self, generation: u64) {
        self.state.send_if_modified(|state| {
            if generation <= state.generation {
                return false;
            }
            *state = NativeDiscoverySnapshot {
                generation,
                services: BTreeMap::new(),
            };
            true
        });
    }
    pub fn upsert(
        &self,
        generation: u64,
        service: String,
        txt: BTreeMap<String, String>,
        mut endpoints: Vec<SocketAddr>,
    ) -> bool {
        let Some(advert) = advert_from_txt(|key| txt.get(key).cloned()) else {
            return false;
        };
        if service.is_empty() || service.len() > 1024 {
            return false;
        }
        endpoints.retain(|address| {
            let ip = address.ip();
            address.port() != 0 && address.port() == advert.port && !ip.is_unspecified() && !ip.is_multicast()
        });
        endpoints.sort();
        endpoints.dedup();
        endpoints.truncate(16);
        if endpoints.is_empty() {
            return false;
        }
        self.state.send_if_modified(|state| {
            if state.generation != generation || (!state.services.contains_key(&service) && state.services.len() >= 256)
            {
                return false;
            }
            if state
                .services
                .get(&service)
                .is_some_and(|old| old.advert == advert && old.addrs == endpoints)
            {
                return false;
            }
            state.services.insert(
                service,
                Sighting {
                    advert,
                    addrs: endpoints,
                    source: Source::Mdns,
                },
            );
            true
        })
    }
    pub fn withdraw(&self, generation: u64, service: &str) -> bool {
        self.state
            .send_if_modified(|state| state.generation == generation && state.services.remove(service).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Advert;

    #[test]
    fn withdrawals_and_generation_fence_late_callbacks() {
        let discovery = NativeDiscovery::new();
        let view = discovery.subscribe();
        let advert = Advert::local(
            oink_foundation::DeviceId([7; 32]),
            "test phone".into(),
            oink_foundation::GroupId([9; 16]),
            1,
            47470,
            "ios".into(),
        );
        let txt = super::super::txt_pairs(&advert).into_iter().collect::<BTreeMap<_, _>>();
        let endpoint = "[fe80::1%4]:47470".parse().unwrap();
        assert!(discovery.upsert(0, "a".into(), txt.clone(), vec![endpoint]));
        assert_eq!(view.borrow().services["a"].addrs, vec![endpoint]);
        assert!(discovery.withdraw(0, "a"));
        discovery.network_changed(1);
        assert!(!discovery.upsert(0, "a".into(), txt.clone(), vec![endpoint]));
        assert!(discovery.upsert(1, "a".into(), txt, vec![endpoint]));
        discovery.network_changed(2);
        assert!(view.borrow().services.is_empty());
        discovery.network_changed(1);
        assert_eq!(view.borrow().generation, 2);
    }
}
