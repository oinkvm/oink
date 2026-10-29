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

#[cfg(all(test, debug_assertions))]
mod split_tests {
    use super::*;
    use oink_audio::media::Kind;
    use oink_network::link::{LinkId, QueueSpec};
    use oink_protocol::frame::{StreamKind, StreamTag};
    use oink_protocol::v1::hot::{self, Value};

    #[test]
    fn overflow_is_bounded_and_accounted_only_to_the_delayed_lane() {
        let (send, [speaker, microphone]) = oink_network::link::testing::partitioned_datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec {
                messages: 16,
                bytes: 16 * 1100,
            },
            [[0x0604, 0x0606], [0x0618, 0x0619]],
        );
        let before = Instant::now();
        for sequence in 1..=13 {
            let kind = if sequence == 13 {
                Kind::Microphone
            } else {
                Kind::Speaker
            };
            let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
            let len = hot::encode_into(
                kind.sleep_id(),
                &[(1, Value::Uint(1)), (2, Value::Uint(1)), (3, Value::Uint(sequence))],
                &mut bytes,
                oink_audio::media::FRAME_CAPACITY,
            )
            .unwrap();
            send.post(&bytes[..len]).unwrap();
        }
        let after = Instant::now();
        let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
        assert!(microphone.take_timed_into(&mut bytes).unwrap().is_some());
        assert_eq!(microphone.dropped(), 0);
        assert_eq!(speaker.dropped(), 4);
        for sequence in 1..=8 {
            let (len, arrived) = speaker.take_timed_into(&mut bytes).unwrap().unwrap();
            assert!(arrived >= before && arrived <= after, "queueing retains ingress time");
            let message =
                oink_protocol::v1::frame::OpaqueFrame::parse_exact(&bytes[..len], oink_audio::media::FRAME_CAPACITY)
                    .unwrap()
                    .decode()
                    .unwrap();
            let oink_protocol::v1::Message::AudioSleep(sleep) = message else {
                panic!("wrong lane")
            };
            assert_eq!(sleep.sequence(), Ok(sequence));
        }
        assert!(speaker.take_timed_into(&mut bytes).unwrap().is_none());
    }
    #[test]
    fn either_consumer_reads_its_kind_without_stealing_the_other_stream() {
        let (send, [speaker, microphone]) = oink_network::link::testing::partitioned_datagram_pair(
            LinkId(1),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec {
                messages: 16,
                bytes: 16 * 1100,
            },
            [[0x0604, 0x0606], [0x0618, 0x0619]],
        );
        for kind in [Kind::Microphone, Kind::Speaker, Kind::Microphone, Kind::Speaker] {
            let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
            let len = hot::encode_into(
                kind.sleep_id(),
                &[(1, Value::Uint(1)), (2, Value::Uint(1)), (3, Value::Uint(1))],
                &mut bytes,
                oink_audio::media::FRAME_CAPACITY,
            )
            .unwrap();
            send.post(&bytes[..len]).unwrap();
        }
        let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
        for (lane, expected) in [
            (speaker.clone(), Kind::Speaker),
            (speaker, Kind::Speaker),
            (microphone.clone(), Kind::Microphone),
            (microphone, Kind::Microphone),
        ] {
            let (len, _) = lane.take_timed_into(&mut bytes).unwrap().unwrap();
            assert_eq!(Kind::from_frame(&bytes[..len]).unwrap(), expected);
        }
    }
}
