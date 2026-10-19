//! The audio data plane depends only on packet admission and timestamped reads.
//! Tests substitute an in-memory endpoint without exporting network fake handles.
use oink_network::link::{DatagramError, DatagramRecv, DatagramSend, StreamId};
use std::{fmt::Debug, time::Instant};

pub trait MediaSender: Debug + Send + Sync {
    fn post(&self, frame: &[u8]) -> Result<(), DatagramError>;
}
pub trait MediaReceiver: Debug + Send + Sync {
    fn follows(&self) -> StreamId;
    fn dropped(&self) -> u64;
    fn take_timed_into(&self, output: &mut [u8]) -> Result<Option<(usize, Instant)>, DatagramError>;
}
impl MediaSender for DatagramSend {
    fn post(&self, frame: &[u8]) -> Result<(), DatagramError> {
        DatagramSend::post(self, frame)
    }
}
impl MediaReceiver for DatagramRecv {
    fn dropped(&self) -> u64 {
        DatagramRecv::dropped(self)
    }
    fn follows(&self) -> StreamId {
        DatagramRecv::follows(self)
    }
    fn take_timed_into(&self, output: &mut [u8]) -> Result<Option<(usize, Instant)>, DatagramError> {
        DatagramRecv::take_timed_into(self, output)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use oink_network::link::{LinkId, QueueSpec, StreamDir};
    use oink_protocol::frame::StreamTag;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Debug)]
    struct Frame {
        bytes: [u8; oink_audio::media::FRAME_CAPACITY],
        len: usize,
        arrived: Instant,
    }
    #[derive(Debug)]
    struct Queue {
        frames: Mutex<VecDeque<Frame>>,
        capacity: usize,
        dropped: std::sync::atomic::AtomicU64,
        follows: StreamId,
    }
    impl MediaSender for Queue {
        fn post(&self, frame: &[u8]) -> Result<(), DatagramError> {
            if frame.len() > oink_audio::media::FRAME_CAPACITY {
                return Err(DatagramError::TooLarge);
            }
            let mut slot = Frame {
                bytes: [0; oink_audio::media::FRAME_CAPACITY],
                len: frame.len(),
                arrived: Instant::now(),
            };
            slot.bytes[..frame.len()].copy_from_slice(frame);
            let mut frames = self.frames.lock().unwrap();
            if frames.len() == self.capacity {
                frames.pop_front();
                self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            frames.push_back(slot);
            Ok(())
        }
    }
    impl MediaReceiver for Queue {
        fn dropped(&self) -> u64 {
            self.dropped.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn follows(&self) -> StreamId {
            self.follows
        }
        fn take_timed_into(&self, output: &mut [u8]) -> Result<Option<(usize, Instant)>, DatagramError> {
            let Some(frame) = self.frames.lock().unwrap().pop_front() else {
                return Ok(None);
            };
            if frame.len > output.len() {
                return Err(DatagramError::TooLarge);
            }
            output[..frame.len].copy_from_slice(&frame.bytes[..frame.len]);
            Ok(Some((frame.len, frame.arrived)))
        }
    }
    pub(crate) fn datagram_pair(
        link: LinkId,
        tag: StreamTag,
        queue: QueueSpec,
    ) -> (Arc<dyn MediaSender>, Arc<dyn MediaReceiver>) {
        let queue = Arc::new(Queue {
            frames: Mutex::new(VecDeque::with_capacity(usize::from(queue.messages))),
            capacity: usize::from(queue.messages),
            dropped: Default::default(),
            follows: StreamId {
                link,
                tag,
                dir: StreamDir::In,
            },
        });
        (queue.clone(), queue)
    }
}
