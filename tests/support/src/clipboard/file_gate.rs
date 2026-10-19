//! One-shot native file read boundary for real transport interruption fixtures.
use std::{
    io::{self, Read},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

#[derive(Debug)]
pub(super) struct State {
    after: u64,
    claimed: AtomicBool,
    released: Mutex<bool>,
    ready: Condvar,
    entered: Mutex<Option<mpsc::Sender<u64>>>,
    offsets: Mutex<Vec<u64>>,
}
#[derive(Debug)]
pub struct FileReadGate {
    pub(super) state: Arc<State>,
}
impl FileReadGate {
    pub fn release(&self) {
        *self.state.released.lock().unwrap() = true;
        self.state.ready.notify_all();
    }
    pub fn opened_offsets(&self) -> Vec<u64> {
        self.state.offsets.lock().unwrap().clone()
    }
}
impl Drop for FileReadGate {
    fn drop(&mut self) {
        self.release();
    }
}
pub(super) fn new(after: u64) -> (FileReadGate, mpsc::Receiver<u64>) {
    let (send, receive) = mpsc::channel();
    (
        FileReadGate {
            state: Arc::new(State {
                after,
                claimed: AtomicBool::new(false),
                released: Mutex::new(false),
                ready: Condvar::new(),
                entered: Mutex::new(Some(send)),
                offsets: Mutex::new(Vec::new()),
            }),
        },
        receive,
    )
}
struct Gated {
    inner: oink_clipboard::FileReader,
    gate: Arc<State>,
    position: u64,
}
impl Read for Gated {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.position >= self.gate.after && !self.gate.claimed.swap(true, Ordering::AcqRel) {
            if let Some(entered) = self.gate.entered.lock().unwrap().take() {
                let _ = entered.send(self.position);
            }
            let mut released = self.gate.released.lock().unwrap();
            while !*released {
                released = self.gate.ready.wait(released).unwrap();
            }
        }
        let max = if self.position < self.gate.after {
            bytes
                .len()
                .min(usize::try_from(self.gate.after - self.position).unwrap_or(usize::MAX))
        } else {
            bytes.len()
        };
        let count = self.inner.read(&mut bytes[..max])?;
        self.position += count as u64;
        Ok(count)
    }
}
pub(super) fn wrap(reader: oink_clipboard::FileReader, state: Arc<State>, offset: u64) -> oink_clipboard::FileReader {
    {
        let mut offsets = state.offsets.lock().unwrap();
        if offsets.len() < 128 {
            offsets.push(offset);
        }
    }
    Box::new(Gated {
        inner: reader,
        gate: state,
        position: offset,
    })
}
