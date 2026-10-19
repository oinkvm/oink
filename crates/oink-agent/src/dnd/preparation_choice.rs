//! The first ready publication wins; Enter work never blocks ordinary completion.
use std::sync::{Condvar, Mutex};

pub(super) struct Choice<T> {
    result: Mutex<Option<T>>,
    changed: Condvar,
    selecting: Mutex<()>,
}
impl<T: Clone> Choice<T> {
    pub(super) fn new() -> Self {
        Self {
            result: Mutex::new(None),
            changed: Condvar::new(),
            selecting: Mutex::new(()),
        }
    }
    /// Losing namespaces drop outside the completion lock without revoking the winner.
    pub(super) fn complete(&self, result: T) -> bool {
        let mut ready = self.result.lock().unwrap();
        if ready.is_some() {
            return false;
        }
        *ready = Some(result);
        self.changed.notify_all();
        true
    }
    pub(super) fn pending(&self) -> bool {
        self.result.lock().unwrap().is_none()
    }
    pub(super) fn with_result(&self, visit: impl FnOnce(&T)) {
        if let Some(result) = &*self.result.lock().unwrap() {
            visit(result);
        }
    }
    /// `start_enter` only schedules work. Both workers publish through complete.
    pub(super) fn select(&self, start_enter: impl FnOnce()) -> T {
        let _selecting = self.selecting.lock().unwrap();
        if let Some(result) = self.result.lock().unwrap().clone() {
            return result;
        }
        start_enter();
        let mut ready = self.result.lock().unwrap();
        while ready.is_none() {
            ready = self.changed.wait(ready).unwrap();
        }
        ready.clone().unwrap()
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn completed_ordinary_or_failure_never_spends_stock() {
        for result in [Ok(1), Err(2)] {
            let choice = Choice::new();
            assert!(choice.complete(result));
            assert_eq!(choice.select(|| panic!("completed ordinary must win")), result);
            choice.with_result(|ready| assert_eq!(*ready, result));
        }
    }

    #[test]
    fn pending_ordinary_selects_enter_without_waiting_for_completion() {
        let choice = Choice::new();
        assert_eq!(
            choice.select(|| {
                assert!(choice.complete(2));
            }),
            2
        );
        assert!(!choice.complete(1));
        assert_eq!(choice.select(|| panic!("winner must be retained")), 2);
    }

    #[test]
    fn off_or_empty_keeps_the_ordinary_wait_path() {
        let choice = Arc::new(Choice::new());
        let worker = choice.clone();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || tx.send(worker.select(|| {})).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        choice.complete(1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        thread.join().unwrap();
    }

    #[test]
    fn ordinary_ready_during_enter_publication_is_observable_immediately() {
        let choice = Arc::new(Choice::new());
        let worker = choice.clone();
        let enter_choice = choice.clone();
        let (entered, enter) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let (loser_done, loser_result) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            done.send(worker.select(|| {
                std::thread::spawn(move || {
                    entered.send(()).unwrap();
                    resumed.recv().unwrap();
                    loser_done.send(enter_choice.complete(2)).unwrap();
                });
            }))
            .unwrap();
        });
        enter.recv_timeout(Duration::from_secs(1)).unwrap();
        choice.complete(1);
        let immediate = result.recv_timeout(Duration::from_millis(50));
        resume.send(()).unwrap();
        thread.join().unwrap();
        assert_eq!(immediate, Ok(1), "an in-flight Enter must not hide ready ordinary URLs");
        assert_eq!(loser_result.recv_timeout(Duration::from_secs(1)), Ok(false));
        assert_eq!(choice.select(|| panic!("late Enter replaced winner")), 1);
    }

    #[test]
    fn overlapping_enters_launch_only_one_publication() {
        let choice = Arc::new(Choice::new());
        let first = choice.clone();
        let (started, start) = mpsc::channel();
        let thread = std::thread::spawn(move || first.select(|| started.send(()).unwrap()));
        start.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = choice.clone();
        let second = std::thread::spawn(move || second.select(|| panic!("second Enter spent stock")));
        choice.complete(2);
        assert_eq!(thread.join().unwrap(), 2);
        assert_eq!(second.join().unwrap(), 2);
        assert!(!choice.pending());
    }

    #[test]
    fn losing_namespace_is_dropped_without_touching_selected_delivery() {
        let choice = Choice::new();
        let ordinary = Arc::new(1);
        let delivery = choice.select(|| {
            choice.complete(ordinary.clone());
        });
        for losing in [2, 3] {
            let unused = Arc::new(losing);
            let weak = Arc::downgrade(&unused);
            assert!(!choice.complete(unused));
            assert!(weak.upgrade().is_none());
        }
        drop(choice);
        assert!(Arc::ptr_eq(&ordinary, &delivery));
    }

    #[test]
    fn enter_return_reannounce_and_following_drags_only_take_at_enter() {
        let stock = Mutex::new(vec![2, 1]);
        let first = Choice::new();
        let other_member = Choice::new();
        other_member.complete(10);
        assert_eq!(stock.lock().unwrap().len(), 2);
        assert_eq!(
            first.select(|| {
                first.complete(stock.lock().unwrap().pop().unwrap());
            }),
            1
        );
        {
            let reannounced = Choice::new();
            reannounced.complete(11);
        }
        assert_eq!(*stock.lock().unwrap(), [2]);
        let second = Choice::new();
        assert_eq!(
            second.select(|| {
                second.complete(stock.lock().unwrap().pop().unwrap());
            }),
            2
        );
        stock.lock().unwrap().push(3);
        let third = Choice::new();
        assert_eq!(
            third.select(|| {
                third.complete(stock.lock().unwrap().pop().unwrap());
            }),
            3
        );
        assert!(!first.complete(12));
        assert_eq!(second.select(|| panic!("second stock already selected")), 2);
    }
}
