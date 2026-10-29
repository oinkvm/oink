//! Optional numeric stage observations. No identities, content, paths, allocation,
//! serialization, or transport belongs in this callback interface.
use crate::model::Unavailable;
use std::{sync::OnceLock, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed(Unavailable),
    Cancelled,
}
#[derive(Clone, Copy, Debug)]
pub enum Measurement {
    FileFirstByte { size: u64 },
    FileComplete { size: u64, outcome: Outcome },
    Delivery { drag: bool, outcome: Outcome },
    DropAccepted { accepted: bool },
}
#[derive(Debug)]
pub struct Observer {
    pub enabled: fn() -> bool,
    pub record: fn(Measurement, Instant, Instant),
}
static OBSERVER: OnceLock<Observer> = OnceLock::new();
pub fn set_observer(observer: Observer) {
    let _ = OBSERVER.set(observer);
}
pub fn start() -> Option<Instant> {
    OBSERVER.get().filter(|o| (o.enabled)()).map(|_| Instant::now())
}
pub fn record(start: Option<Instant>, measurement: Measurement) {
    if let (Some(start), Some(observer)) = (start, OBSERVER.get()) {
        (observer.record)(measurement, start, Instant::now());
    }
}

/// One full-file reader only (offset zero). Reconnects stay inside this sample;
/// seek/range readers are deliberately excluded from whole-file throughput.
#[derive(Debug)]
pub(crate) struct FileRead {
    start: Option<Instant>,
    size: u64,
    first: bool,
}
impl FileRead {
    pub(crate) fn new(start: Option<Instant>, size: u64) -> Self {
        Self {
            start,
            size,
            first: false,
        }
    }
    pub(crate) fn first_byte(&mut self) {
        if !self.first {
            self.first = true;
            record(self.start, Measurement::FileFirstByte { size: self.size });
        }
    }
    pub(crate) fn finish(&mut self, outcome: Outcome) {
        record(
            self.start.take(),
            Measurement::FileComplete {
                size: self.size,
                outcome,
            },
        );
    }
}
impl Drop for FileRead {
    fn drop(&mut self) {
        self.finish(Outcome::Cancelled);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    thread_local! {
        static CAPTURE: RefCell<Option<Vec<Measurement>>> = const { RefCell::new(None) };
    }
    pub(crate) fn capture(work: impl FnOnce()) -> Vec<Measurement> {
        set_observer(Observer {
            enabled: || CAPTURE.with(|v| v.borrow().is_some()),
            record: |m, _, _| {
                CAPTURE.with(|v| {
                    if let Some(events) = v.borrow_mut().as_mut() {
                        events.push(m);
                    }
                })
            },
        });
        CAPTURE.with(|v| *v.borrow_mut() = Some(Vec::new()));
        work();
        CAPTURE.with(|v| v.borrow_mut().take().unwrap())
    }
    #[test]
    fn file_boundaries_are_once_only_and_incomplete_read_is_cancelled() {
        let events = capture(|| {
            let mut file = FileRead::new(start(), 123);
            file.first_byte();
            file.first_byte();
            file.finish(Outcome::Completed);
            file.finish(Outcome::Completed);
            drop(file);
            drop(FileRead::new(start(), 456));
        });
        assert!(matches!(
            events.as_slice(),
            [
                Measurement::FileFirstByte { size: 123 },
                Measurement::FileComplete {
                    size: 123,
                    outcome: Outcome::Completed
                },
                Measurement::FileComplete {
                    size: 456,
                    outcome: Outcome::Cancelled
                }
            ]
        ));
        assert!(start().is_none());
    }
}
