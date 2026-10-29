//! A callback-local bound for synchronous remote representations, not file hydration.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::OnceLock,
    time::{Duration, Instant},
};

// User decision, 2026-10-25: "先加一个 5s 的超时，超时直接让粘贴失败".
pub const NATIVE_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeReadOutcome {
    Served,
    Failed,
    TimedOut,
    Cancelled,
}

static OBSERVER: OnceLock<fn(Duration, NativeReadOutcome)> = OnceLock::new();

/// Observe returning native callbacks that read a remote representation. Called
/// on the native thread: observers must be nonblocking. No content or identity.
pub fn set_native_read_observer(observer: fn(Duration, NativeReadOutcome)) -> bool {
    OBSERVER.set(observer).is_ok()
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Read>>> = const { RefCell::new(None) };
}

pub(crate) struct Read {
    started: Instant,
    remote: Cell<bool>,
    outcome: Cell<NativeReadOutcome>,
    #[cfg(test)]
    clock: Option<std::sync::Arc<TestClock>>,
}
impl Read {
    pub(crate) fn elapsed(&self) -> Duration {
        let elapsed = self.started.elapsed();
        #[cfg(test)]
        let elapsed = elapsed
            + self.clock.as_ref().map_or(Duration::ZERO, |clock| {
                Duration::from_millis(clock.offset.load(std::sync::atomic::Ordering::Acquire))
            });
        elapsed
    }
    pub(crate) fn remaining(&self) -> Duration {
        NATIVE_READ_TIMEOUT.saturating_sub(self.elapsed())
    }
    pub(crate) fn timed_out(&self) {
        self.outcome.set(NativeReadOutcome::TimedOut);
    }
    pub(crate) fn cancelled(&self) {
        if self.outcome.get() != NativeReadOutcome::TimedOut {
            self.outcome.set(NativeReadOutcome::Cancelled);
        }
    }
    #[cfg(test)]
    pub(crate) fn wake_on_advance(&self, wake: impl Fn() + Send + Sync + 'static) {
        if let Some(clock) = &self.clock {
            *clock.wake.lock().unwrap() = Some(Box::new(wake));
        }
    }
}

pub(crate) fn timed_out() -> bool {
    CURRENT.with_borrow(|read| {
        read.as_ref()
            .is_some_and(|read| read.outcome.get() == NativeReadOutcome::TimedOut)
    })
}

pub(crate) fn remote_read() -> Option<Rc<Read>> {
    CURRENT.with_borrow(|read| {
        read.as_ref().map(|read| {
            read.remote.set(true);
            read.clone()
        })
    })
}

pub(crate) struct Callback {
    read: Rc<Read>,
    previous: Option<Rc<Read>>,
}
impl Callback {
    pub(crate) fn begin() -> Self {
        let read = Rc::new(Read {
            started: Instant::now(),
            remote: Cell::new(false),
            outcome: Cell::new(NativeReadOutcome::Failed),
            #[cfg(test)]
            clock: TEST_CLOCK.with_borrow(Clone::clone),
        });
        let previous = CURRENT.replace(Some(read.clone()));
        Self { read, previous }
    }
    pub(crate) fn served(&self) {
        self.read.outcome.set(NativeReadOutcome::Served);
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn timed_out(&self) -> bool {
        timed_out()
    }
}
impl Drop for Callback {
    fn drop(&mut self) {
        CURRENT.set(self.previous.take());
        if self.read.remote.get() {
            let elapsed = self.read.elapsed();
            let outcome = self.read.outcome.get();
            if let Some(observer) = OBSERVER.get() {
                observer(elapsed, outcome);
            }
            #[cfg(test)]
            TEST_OUTCOMES.with_borrow_mut(|outcomes| outcomes.push((elapsed, outcome)));
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CLOCK: RefCell<Option<std::sync::Arc<TestClock>>> = const { RefCell::new(None) };
    pub(crate) static TEST_OUTCOMES: RefCell<Vec<(Duration, NativeReadOutcome)>> = const { RefCell::new(Vec::new()) };
}
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestClock {
    offset: std::sync::atomic::AtomicU64,
    wake: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
#[cfg(test)]
impl TestClock {
    pub(crate) fn expire(&self) {
        self.offset.store(
            NATIVE_READ_TIMEOUT.as_millis() as u64,
            std::sync::atomic::Ordering::Release,
        );
        if let Some(wake) = self.wake.lock().unwrap().as_ref() {
            wake();
        }
    }
    pub(crate) fn scope<T>(self: &std::sync::Arc<Self>, action: impl FnOnce() -> T) -> T {
        struct Reset(Option<std::sync::Arc<TestClock>>);
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_CLOCK.set(self.0.take());
            }
        }
        let _reset = Reset(TEST_CLOCK.replace(Some(self.clone())));
        action()
    }
}

// Uses the debug-only content fixture.
#[cfg(all(test, debug_assertions))]
pub(crate) mod tests;
