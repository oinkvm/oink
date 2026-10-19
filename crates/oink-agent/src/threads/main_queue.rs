//! Existing main-command queue with an explicit park notification. Numeric trace
//! queues share this notification without allocating an extra command to wake it.
use super::MainCommand;
use oink_foundation::wake::WakeGate;
use std::sync::{Arc, OnceLock, mpsc};

#[derive(Debug, Default)]
struct Wake {
    thread: OnceLock<std::thread::Thread>,
    gate: WakeGate,
}
impl Wake {
    fn notify(&self) {
        if self.gate.notify()
            && let Some(thread) = self.thread.get()
        {
            thread.unpark();
        }
    }
}

#[derive(Debug)]
pub struct MainSender {
    sender: Option<mpsc::Sender<MainCommand>>,
    wake: Arc<Wake>,
}
impl Clone for MainSender {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            wake: self.wake.clone(),
        }
    }
}
impl MainSender {
    #[allow(clippy::result_large_err)] // Preserve the inline command; boxing would allocate on the producer.
    pub fn send(&self, command: MainCommand) -> Result<(), mpsc::SendError<MainCommand>> {
        self.sender.as_ref().expect("live sender").send(command)?;
        self.wake.notify();
        Ok(())
    }
    /// Signal records already placed in a separate fixed-capacity trace queue.
    pub(crate) fn notifier(&self) -> Arc<dyn Fn() + Send + Sync> {
        let wake = Arc::downgrade(&self.wake);
        Arc::new(move || {
            if let Some(wake) = wake.upgrade() {
                wake.notify();
            }
        })
    }
}
impl Drop for MainSender {
    fn drop(&mut self) {
        // Publish disconnection before waking: the receiver may be parked without
        // any command when the last producer is dropped during startup failure.
        drop(self.sender.take());
        self.wake.notify();
    }
}
#[derive(Debug)]
pub struct MainReceiver {
    receiver: mpsc::Receiver<MainCommand>,
    wake: Arc<Wake>,
}
impl std::ops::Deref for MainReceiver {
    type Target = mpsc::Receiver<MainCommand>;
    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}
impl MainReceiver {
    pub(crate) fn bind(&self) {
        self.wake
            .thread
            .set(std::thread::current())
            .expect("one main-command owner");
    }
    pub(crate) fn begin_turn(&self) {
        self.wake.gate.begin_turn();
    }
    pub(crate) fn wait(&self) {
        if self.wake.gate.prepare_wait() {
            std::thread::park();
        }
    }
}
pub fn main_channel() -> (MainSender, MainReceiver) {
    let (sender, receiver) = mpsc::channel();
    let wake = Arc::new(Wake::default());
    (
        MainSender {
            sender: Some(sender),
            wake: wake.clone(),
        },
        MainReceiver { receiver, wake },
    )
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn commands_trace_only_notifications_and_last_sender_drop_wake_the_owner() {
        for mode in 0..3 {
            let (send, receive) = main_channel();
            let (ready, started) = mpsc::sync_channel(1);
            let (done, finished) = mpsc::sync_channel(1);
            let owner = std::thread::spawn(move || {
                receive.bind();
                receive.begin_turn();
                assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
                ready.send(()).unwrap();
                receive.wait();
                done.send(receive.try_recv()).unwrap();
            });
            started.recv().unwrap();
            match mode {
                0 => send.send(MainCommand::Wake).unwrap(),
                1 => send.notifier()(),
                _ => drop(send),
            }
            let result = finished.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
            match mode {
                0 => assert!(matches!(result, Ok(MainCommand::Wake))),
                1 => assert!(matches!(result, Err(mpsc::TryRecvError::Empty))),
                _ => assert!(matches!(result, Err(mpsc::TryRecvError::Disconnected))),
            }
            owner.join().unwrap();
        }
    }
}
