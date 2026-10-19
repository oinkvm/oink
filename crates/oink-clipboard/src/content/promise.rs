//! The crossing deadline bounds names only; file receipt uses transfer authority.
//! This gate owns publication authority, never an external writer's lifetime.
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

pub(crate) const LIMIT: Duration = Duration::from_secs(3);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Prepared,
    Receiving,
    Finalizing,
    Ready,
    Abandoned,
}
#[derive(Default)]
struct Receiver {
    names: Option<Vec<String>>,
    completed: Vec<Option<String>>,
}
pub(crate) struct Gate {
    pub phase: Phase,
    deadline: Option<Instant>,
    receivers: Vec<Receiver>,
}
impl Gate {
    pub fn new(receivers: usize) -> Self {
        Self {
            phase: Phase::Prepared,
            deadline: None,
            receivers: (0..receivers).map(|_| Receiver::default()).collect(),
        }
    }
    #[cfg(all(test, debug_assertions))]
    pub fn begin(&mut self, now: Instant) -> bool {
        if self.phase != Phase::Prepared || self.receivers.is_empty() {
            return false;
        }
        self.begin_until(Some(now + LIMIT))
    }
    /// Border export supplies crossing+3s; a Drop proxy uses the transfer's
    /// existing no-progress/cancel authority, not an unrelated export timer.
    pub fn begin_until(&mut self, deadline: Option<Instant>) -> bool {
        if self.phase != Phase::Prepared || self.receivers.is_empty() {
            return false;
        }
        self.deadline = deadline;
        self.phase = Phase::Receiving;
        true
    }
    /// Filenames are enough to publish unknown-size virtual descriptors. Once
    /// declared, content receipt must not inherit the short crossing deadline.
    #[cfg(target_os = "macos")]
    pub fn names_ready(&mut self, now: Instant) -> bool {
        if self.phase != Phase::Receiving
            || !self.live(now)
            || self.receivers.iter().any(|receiver| receiver.names.is_none())
        {
            return false;
        }
        self.deadline = None;
        true
    }
    pub fn live(&self, now: Instant) -> bool {
        matches!(self.phase, Phase::Receiving | Phase::Finalizing)
            && self.deadline.is_none_or(|deadline| now < deadline)
    }
    pub fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
    pub fn abandon(&mut self) -> bool {
        if matches!(self.phase, Phase::Ready | Phase::Abandoned) {
            return false;
        }
        self.phase = Phase::Abandoned;
        true
    }
    /// A known real-file item does not participate in the Windows exception.
    #[cfg(target_os = "macos")]
    pub fn skip(&mut self, receiver: usize) {
        self.receivers[receiver].names = Some(Vec::new());
    }
    pub fn names(&mut self, receiver: usize, names: Vec<String>) {
        let valid = !names.is_empty() && names.iter().all(|name| valid_name(name));
        self.receivers[receiver].names = valid.then_some(names);
        if !valid {
            self.abandon();
        }
        // Separate native items may legitimately export the same leaf name.
        // Each receiver owns its own managed container; duplicates WITHIN one
        // receiver are ambiguous and must fail the whole receipt.
        if self.receivers.iter().filter_map(|r| r.names.as_ref()).any(|names| {
            let mut unique = HashSet::new();
            names.iter().any(|name| !unique.insert(name))
        }) {
            self.abandon();
        }
    }
    pub fn completed(&mut self, receiver: usize, name: Option<String>) {
        if name.is_none() {
            self.abandon();
        }
        self.receivers[receiver].completed.push(name);
    }
    pub fn drained(&self) -> bool {
        self.receivers
            .iter()
            .all(|r| r.names.as_ref().is_some_and(|names| names.len() == r.completed.len()))
    }
    pub fn finalize(&mut self, now: Instant) -> bool {
        if self.phase != Phase::Receiving {
            return false;
        }
        if !self.live(now) {
            self.abandon();
            return false;
        }
        if !self.drained() {
            return false;
        }
        if self.receivers.iter().any(|r| {
            let expected: HashSet<_> = r.names.as_ref().unwrap().iter().collect();
            let actual: HashSet<_> = r.completed.iter().filter_map(Option::as_ref).collect();
            expected != actual || actual.len() != r.completed.len()
        }) {
            self.abandon();
            return false;
        }
        self.phase = Phase::Finalizing;
        true
    }
    pub fn commit(&mut self, now: Instant) -> bool {
        if self.phase != Phase::Finalizing {
            return false;
        }
        if !self.live(now) {
            self.abandon();
            return false;
        }
        self.phase = Phase::Ready;
        true
    }
    #[cfg(all(test, debug_assertions))]
    pub fn ordered_names(&self) -> Vec<String> {
        self.receivers
            .iter()
            .filter_map(|r| r.names.as_ref())
            .flatten()
            .cloned()
            .collect()
    }
    #[cfg(target_os = "macos")]
    pub fn receiver_names(&self) -> Vec<Vec<String>> {
        self.receivers
            .iter()
            .map(|r| r.names.clone().unwrap_or_default())
            .collect()
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "macos")]
    fn known_names_end_the_crossing_deadline_before_content_is_ready() {
        let now = Instant::now();
        let mut gate = Gate::new(2);
        assert!(gate.begin_until(Some(now + LIMIT)));
        gate.names(0, vec!["first.txt".into()]);
        assert!(!gate.names_ready(now));
        gate.names(1, vec!["second.txt".into()]);
        assert!(gate.names_ready(now + LIMIT - Duration::from_nanos(1)));
        assert!(!gate.drained());
        let after_write = now + LIMIT * 10;
        gate.completed(0, Some("first.txt".into()));
        gate.completed(1, Some("second.txt".into()));
        assert!(gate.finalize(after_write));
        assert!(gate.commit(after_write));
        let mut late = Gate::new(1);
        late.begin_until(Some(now + LIMIT));
        late.names(0, vec!["late.txt".into()]);
        assert!(!late.names_ready(now + LIMIT));
        assert!(!late.finalize(now + LIMIT));
    }
    #[test]
    fn drop_proxy_has_no_boundary_deadline_and_equal_names_in_separate_items_are_valid() {
        let now = Instant::now();
        let mut gate = Gate::new(2);
        assert!(gate.begin_until(None));
        for receiver in 0..2 {
            gate.names(receiver, vec!["same.jpg".into()]);
            gate.completed(receiver, Some("same.jpg".into()));
        }
        assert!(gate.finalize(now + Duration::from_secs(30)));
        assert!(gate.commit(now + Duration::from_secs(31)));
        let mut boundary = Gate::new(1);
        assert!(boundary.begin_until(Some(now + LIMIT)));
        boundary.names(0, vec!["late.jpg".into()]);
        boundary.completed(0, Some("late.jpg".into()));
        assert!(!boundary.finalize(now + LIMIT));
    }
    #[test]
    fn entire_batch_must_finish_and_commit_strictly_before_one_deadline() {
        let now = Instant::now();
        for at in [LIMIT - Duration::from_nanos(1), LIMIT, LIMIT + Duration::from_nanos(1)] {
            let mut gate = Gate::new(2);
            assert!(gate.begin(now));
            gate.names(0, vec!["one".into()]);
            gate.names(1, vec!["two".into()]);
            gate.completed(0, Some("one".into()));
            assert!(!gate.finalize(now + Duration::from_secs(2)));
            gate.completed(1, Some("two".into()));
            assert!(gate.finalize(now + Duration::from_secs(2)));
            assert_eq!(gate.commit(now + at), at < LIMIT);
            assert!(!gate.commit(now));
            assert_eq!(gate.expired(now + at), at >= LIMIT);
        }
    }
    #[test]
    fn early_callbacks_do_not_guess_item_count_and_late_callbacks_only_drain() {
        let now = Instant::now();
        let mut gate = Gate::new(1);
        assert!(gate.begin(now));
        gate.completed(0, Some("one".into()));
        assert!(!gate.drained());
        assert!(!gate.finalize(now));
        gate.names(0, vec!["one".into(), "two".into()]);
        assert!(gate.abandon());
        gate.completed(0, Some("two".into()));
        assert!(gate.drained());
        assert!(!gate.finalize(now));
        assert!(!gate.commit(now));
        assert_eq!(gate.ordered_names(), ["one", "two"]);
    }
    #[test]
    fn unknown_count_errors_duplicate_names_and_wrong_callbacks_never_publish() {
        let now = Instant::now();
        for names in [vec![], vec!["../escape".into()], vec!["same".into(), "same".into()]] {
            let mut gate = Gate::new(1);
            gate.begin(now);
            gate.names(0, names);
            assert_eq!(gate.phase, Phase::Abandoned);
            assert!(!gate.finalize(now));
        }
        for callback in [None, Some("wrong".into())] {
            let mut gate = Gate::new(1);
            gate.begin(now);
            gate.names(0, vec!["expected".into()]);
            gate.completed(0, callback);
            assert!(gate.drained());
            assert!(!gate.finalize(now));
            assert_eq!(gate.phase, Phase::Abandoned);
        }
    }
}
