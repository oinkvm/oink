//! Default-output observation. Native callbacks mark the reused volume endpoint
//! dirty and wake the audio owner; deadlines only flush the latest observation.
use oink_audio::volume::{Endpoint, Fault, Reading};
use std::time::{Duration, Instant};

const PUBLISH_INTERVAL: Duration = Duration::from_millis(200);
#[derive(Default)]
pub(super) struct Observer {
    output: Option<String>,
    endpoint: Option<Box<dyn Endpoint>>,
    latest: Option<Reading>,
    published: Option<Reading>,
    last_publish: Option<Instant>,
    deadline: Option<Instant>,
}
impl Observer {
    pub(super) fn output(&self) -> Option<&str> {
        self.output.as_deref()
    }
    pub(super) fn readable_endpoint(&self) -> bool {
        self.endpoint.is_some()
    }
    pub(super) fn value(&self) -> Option<Reading> {
        self.published
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub(super) fn bind(&mut self, output: Option<String>, endpoint: Result<Box<dyn Endpoint>, Fault>, now: Instant) {
        self.output = output;
        self.endpoint = endpoint.ok();
        let value = self.endpoint.as_mut().and_then(|endpoint| endpoint.read().ok());
        self.observe(value, now);
    }
    pub(super) fn retire(&mut self, now: Instant) {
        self.bind(None, Err(Fault::unavailable()), now);
    }
    fn observe(&mut self, value: Option<Reading>, now: Instant) {
        self.latest = value.filter(|reading| reading.percent <= 100);
        self.deadline = (self.latest != self.published)
            .then(|| self.last_publish.map_or(now, |last| (last + PUBLISH_INTERVAL).max(now)));
    }
    pub(super) fn tick(&mut self, now: Instant) {
        if let Some(endpoint) = &mut self.endpoint
            && endpoint.changed()
        {
            let value = endpoint.read().ok();
            self.observe(value, now);
        }
        if self.deadline.is_some_and(|deadline| deadline <= now) {
            self.published = self.latest;
            self.last_publish = Some(now);
            self.deadline = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    struct Fake {
        dirty: bool,
        value: Option<Reading>,
        reads: usize,
    }
    struct FakeEndpoint(Rc<RefCell<Fake>>);
    impl Endpoint for FakeEndpoint {
        fn changed(&self) -> bool {
            self.0.borrow().dirty
        }
        fn read(&mut self) -> Result<Reading, Fault> {
            let mut state = self.0.borrow_mut();
            state.dirty = false;
            state.reads += 1;
            state.value.ok_or_else(Fault::unavailable)
        }
        fn write(&mut self, _: Reading) -> Result<(), Fault> {
            panic!("read-only observer")
        }
    }
    #[test]
    fn callback_bursts_publish_latest_at_bounded_rate_without_polling() {
        let now = Instant::now();
        let fake = Rc::new(RefCell::new(Fake {
            dirty: false,
            value: Some(Reading {
                percent: 10,
                muted: false,
            }),
            reads: 0,
        }));
        let mut observer = Observer::default();
        observer.bind(Some("default-a".into()), Ok(Box::new(FakeEndpoint(fake.clone()))), now);
        observer.tick(now);
        assert_eq!(observer.value().unwrap().percent, 10);
        for ms in 1..200 {
            observer.tick(now + Duration::from_millis(ms));
        }
        assert_eq!(fake.borrow().reads, 1, "timer ticks never query the OS");
        for ms in 1..200 {
            let percent = (ms % 100) as u8;
            fake.borrow_mut().value = Some(Reading {
                percent,
                muted: ms == 199,
            });
            fake.borrow_mut().dirty = true;
            observer.tick(now + Duration::from_millis(ms));
            assert_eq!(observer.value().unwrap().percent, 10);
            assert_eq!(
                observer.deadline(),
                (percent != 10 || ms == 199).then_some(now + PUBLISH_INTERVAL)
            );
        }
        observer.tick(now + PUBLISH_INTERVAL);
        assert_eq!(
            observer.value(),
            Some(Reading {
                percent: 99,
                muted: true
            })
        );
        assert!(observer.deadline().is_none());
        let reads = fake.borrow().reads;
        observer.tick(now + Duration::from_secs(10));
        assert_eq!(fake.borrow().reads, reads);
        fake.borrow_mut().value = None;
        fake.borrow_mut().dirty = true;
        observer.tick(now + Duration::from_secs(10));
        assert_eq!(observer.value(), None);
        observer.retire(now + Duration::from_secs(11));
        assert!(observer.output().is_none());
        assert!(!observer.readable_endpoint());
    }
}
