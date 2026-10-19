//! Process, connection, path and sharing identities. Identity equality always
//! includes both process nonces; counters from different boot pairs are unordered.
use crate::DeviceId;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Boot {
    pub device_id: DeviceId,
    pub nonce: [u8; 16],
}

/// Canonical pair, ordered by public-key bytes, never by a nonce or dial direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct BootPair {
    coordinator: Boot,
    follower: Boot,
}
impl BootPair {
    pub fn new(a: Boot, b: Boot) -> Option<Self> {
        match a.device_id.cmp(&b.device_id) {
            Ordering::Less => Some(Self {
                coordinator: a,
                follower: b,
            }),
            Ordering::Greater => Some(Self {
                coordinator: b,
                follower: a,
            }),
            Ordering::Equal => None,
        }
    }
    pub fn coordinator(self) -> Boot {
        self.coordinator
    }
    pub fn follower(self) -> Boot {
        self.follower
    }
    pub fn contains(self, boot: Boot) -> bool {
        self.coordinator == boot || self.follower == boot
    }
    pub fn other(self, boot: Boot) -> Option<Boot> {
        if self.coordinator == boot {
            Some(self.follower)
        } else if self.follower == boot {
            Some(self.coordinator)
        } else {
            None
        }
    }
}
impl<'de> Deserialize<'de> for BootPair {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Pair {
            coordinator: Boot,
            follower: Boot,
        }
        let p = Pair::deserialize(d)?;
        if p.coordinator.device_id >= p.follower.device_id {
            return Err(serde::de::Error::custom("noncanonical boot pair"));
        }
        Ok(Self {
            coordinator: p.coordinator,
            follower: p.follower,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId {
    pub initiator_boot: Boot,
    pub random: [u8; 16],
}

macro_rules! coordinated_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name {
            pub boot_pair: BootPair,
            /// Allocated only by the coordinator. Zero is reserved; never wraps.
            pub coordinator_counter: u64,
        }
        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                (self.boot_pair == other.boot_pair).then(|| self.coordinator_counter.cmp(&other.coordinator_counter))
            }
        }
        impl $name {
            pub fn valid(self) -> bool {
                self.coordinator_counter != 0
            }
        }
    };
}
coordinated_id!(PathId);

/// Compare revisions only after checking the complete PathId. A migration
/// increments this value while retaining PathId; failure allocates a new PathId.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RouteRevision(pub u64);
impl RouteRevision {
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// One PAKE transaction; a repeated request refers to the same transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PairId {
    pub initiator_boot: Boot,
    pub random: [u8; 16],
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counters_do_not_order_different_incarnations() {
        let a = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        };
        let b = Boot {
            device_id: DeviceId([2; 32]),
            nonce: [2; 16],
        };
        let pair = BootPair::new(b, a).unwrap();
        assert_eq!(pair.coordinator(), a);
        assert_eq!(BootPair::new(a, a), None);
        let old = PathId {
            boot_pair: pair,
            coordinator_counter: 9,
        };
        let next = PathId {
            coordinator_counter: 10,
            ..old
        };
        assert!(old < next);
        let reboot = PathId {
            boot_pair: BootPair::new(a, Boot { nonce: [3; 16], ..b }).unwrap(),
            ..next
        };
        assert_eq!(old.partial_cmp(&reboot), None);
        assert_eq!(RouteRevision(u64::MAX).next(), None);
    }
}
