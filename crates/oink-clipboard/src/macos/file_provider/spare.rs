//! One-shot identities: a taken spare cannot be put back, even on cancellation.
use super::*;
use std::collections::VecDeque;

const CAPACITY: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Status {
    pub capacity: usize,
    pub ready: usize,
    // Worker can be waiting for foreground catalog publication to finish.
    pub worker: bool,
    pub preparing: bool,
}

pub(super) struct Spare {
    pub key: String,
    pub token: String,
    pub preparation_us: u64,
}
pub(super) struct Taken<T> {
    pub spare: Spare,
    pub owner: T,
}
#[derive(Default)]
pub(super) struct Pool {
    worker: bool,
    preparing: Option<String>,
    ready: VecDeque<Spare>,
    pub elapsed_us: Option<u64>,
    pub failure: Option<FileProviderReason>,
}
impl Pool {
    pub const fn new() -> Self {
        Self {
            worker: false,
            preparing: None,
            ready: VecDeque::new(),
            elapsed_us: None,
            failure: None,
        }
    }
    pub fn start(&mut self) -> bool {
        if self.worker || self.ready.len() >= CAPACITY {
            return false;
        }
        self.worker = true;
        true
    }
    pub fn preparing(&mut self, key: String) {
        assert!(self.worker && self.preparing.is_none() && self.ready.len() < CAPACITY);
        self.preparing = Some(key);
    }
    /// Keep ownership of the single worker while filling; failure stops this
    /// attempt, preserving other ready identities. A later demand may retry.
    pub fn finish(&mut self, result: Result<Spare, FileProviderReason>, elapsed_us: u64) -> bool {
        self.preparing = None;
        self.elapsed_us = Some(elapsed_us);
        match result {
            Ok(spare) => {
                self.ready.push_back(spare);
                self.failure = None;
                self.worker = self.ready.len() < CAPACITY;
            }
            Err(reason) => {
                self.failure = Some(reason);
                self.worker = false;
            }
        }
        self.worker
    }
    pub fn protected(&self) -> impl Iterator<Item = &str> {
        self.preparing
            .iter()
            .map(String::as_str)
            .chain(self.ready.iter().map(|s| s.key.as_str()))
    }
    pub fn snapshot(&self) -> Status {
        Status {
            capacity: CAPACITY,
            ready: self.ready.len(),
            worker: self.worker,
            preparing: self.preparing.is_some(),
        }
    }
    pub fn observe(&self, timing: &mut crate::provider_preparation::PreparationTiming) {
        timing.spare_ready = !self.ready.is_empty();
        timing.spare_preparation_us = self.elapsed_us;
        timing.spare_failure = self.failure;
    }
    pub fn take<T>(
        &mut self,
        owner: T,
        timing: &mut crate::provider_preparation::PreparationTiming,
    ) -> Option<Taken<T>> {
        self.observe(timing);
        self.ready.pop_front().map(|spare| {
            timing.spare_preparation_us = Some(spare.preparation_us);
            Taken { spare, owner }
        })
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn take_binds_once_and_never_reuses_identity_across_epochs_or_cancel() {
        let id = ClipId {
            origin: oink_foundation::DeviceId([3; 32]),
            seq: 9,
        };
        let mut pool = Pool::default();
        assert!(pool.start());
        assert!(!pool.start());
        let mut timing = crate::provider_preparation::PreparationTiming::default();
        assert!(pool.take((id, 1), &mut timing).is_none());
        pool.preparing("first".into());
        assert_eq!(pool.protected().collect::<Vec<_>>(), vec!["first"]);
        assert!(pool.take((id, 1), &mut timing).is_none());
        pool.finish(
            Ok(Spare {
                key: "first".into(),
                token: "a".into(),
                preparation_us: 10,
            }),
            10,
        );
        let taken = pool.take((id, 1), &mut timing).unwrap();
        assert_eq!(taken.owner, (id, 1));
        assert!(timing.spare_ready);
        assert!(pool.take((id, 2), &mut timing).is_none());
        drop(taken); // cancelled before publication: discard, with no return API
        pool.preparing("second".into());
        pool.finish(
            Ok(Spare {
                key: "second".into(),
                token: "b".into(),
                preparation_us: 20,
            }),
            20,
        );
        assert_eq!(pool.take((id, 2), &mut timing).unwrap().spare.key, "second");
    }
    fn ready(pool: &mut Pool, key: &str) -> bool {
        pool.preparing(key.into());
        pool.finish(
            Ok(Spare {
                key: key.into(),
                token: key.into(),
                preparation_us: 10,
            }),
            10,
        )
    }

    #[test]
    fn startup_and_partial_pool_allow_immediate_take_and_protect_all_catalogs() {
        let mut pool = Pool::new();
        let id = ClipId {
            origin: oink_foundation::DeviceId([4; 32]),
            seq: 1,
        };
        let mut timing = PreparationTiming::default();
        assert_eq!(
            pool.snapshot(),
            Status {
                capacity: 5,
                ready: 0,
                worker: false,
                preparing: false
            }
        );
        assert!(pool.start());
        // A first drag during startup never waits for readiness: NewParent path.
        pool.preparing("one".into());
        assert!(pool.take((id, 1), &mut timing).is_none());
        assert!(!timing.spare_ready);
        assert!(pool.finish(
            Ok(Spare {
                key: "one".into(),
                token: "one".into(),
                preparation_us: 10
            }),
            10
        ));
        pool.preparing("two".into());
        assert_eq!(
            pool.snapshot(),
            Status {
                capacity: 5,
                ready: 1,
                worker: true,
                preparing: true
            }
        );
        assert_eq!(pool.protected().collect::<Vec<_>>(), vec!["two", "one"]);
        let taken = pool.take((id, 2), &mut timing).unwrap();
        assert_eq!(taken.owner, (id, 2));
        assert_eq!(taken.spare.key, "one");
        assert_eq!(pool.protected().collect::<Vec<_>>(), vec!["two"]);
        assert!(!pool.start()); // partial consumption cannot create another worker
        assert!(pool.finish(
            Ok(Spare {
                key: "two".into(),
                token: "two".into(),
                preparation_us: 20
            }),
            20
        ));
        for key in ["three", "four", "five"] {
            assert!(ready(&mut pool, key));
        }
        assert!(!ready(&mut pool, "six"));
        assert_eq!(
            pool.snapshot(),
            Status {
                capacity: 5,
                ready: 5,
                worker: false,
                preparing: false
            }
        );
        assert_eq!(
            pool.protected().collect::<Vec<_>>(),
            vec!["two", "three", "four", "five", "six"]
        );
        assert!(!pool.start()); // full pool is idle
        for (index, key) in ["two", "three", "four", "five", "six"].into_iter().enumerate() {
            assert_eq!(pool.take((id, 3 + index as u64), &mut timing).unwrap().spare.key, key);
            assert_eq!(pool.start(), index == 0);
        }
        assert!(pool.take((id, 9), &mut timing).is_none());
        assert!(!pool.start());
        for key in ["seven", "eight", "nine", "ten"] {
            assert!(ready(&mut pool, key));
        }
        assert!(!ready(&mut pool, "eleven"));
        assert_eq!(pool.snapshot().ready, 5);
    }

    #[test]
    fn failed_refill_preserves_ready_spare_and_retries_only_on_demand() {
        let mut pool = Pool::new();
        assert!(pool.start());
        assert!(ready(&mut pool, "one"));
        pool.preparing("failed".into());
        assert!(!pool.finish(Err(FileProviderReason::Cancelled), 20));
        assert_eq!(pool.protected().collect::<Vec<_>>(), vec!["one"]);
        let mut timing = PreparationTiming::default();
        pool.observe(&mut timing);
        assert!(timing.spare_ready);
        assert_eq!(timing.spare_failure, Some(FileProviderReason::Cancelled));
        assert!(pool.start());
        assert!(!pool.start());
        assert!(ready(&mut pool, "retry"));
        assert_eq!(pool.snapshot().ready, 2);
        assert_eq!(pool.failure, None);
    }
}
