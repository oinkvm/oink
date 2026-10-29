use super::*;
use crate::{content::*, model::*, sync::files::*};
use oink_foundation::DeviceId;
use oink_foundation::content::ClipId;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

struct Link {
    answer: AtomicBool,
    bytes: Vec<u8>,
    started: mpsc::Sender<()>,
    cancelled: mpsc::Sender<()>,
    partial: bool,
}
struct Channel {
    link: Arc<Link>,
    replies: VecDeque<ContentMsg>,
    stalled: bool,
    finished: bool,
}
impl Drop for Channel {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.link.cancelled.send(());
        }
    }
}
// Keep the Arc in every channel, just as the real stream owns its cancellation lifetime.
struct Transport(Arc<Link>);
impl BulkLink for Transport {
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn open(&self, _: DeviceId) -> BoxFuture<'_, std::io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async move {
            Ok(Box::new(Channel {
                link: self.0.clone(),
                replies: VecDeque::new(),
                stalled: false,
                finished: false,
            }) as Box<dyn BulkChannel>)
        })
    }
}
impl BulkChannel for Channel {
    fn send<'a>(&'a mut self, request: &'a ContentMsg) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            let ContentMsg::ReadRepresentation {
                offer, representation, ..
            } = *request
            else {
                panic!("representation only")
            };
            self.stalled = !self.link.answer.load(Ordering::Acquire);
            if !self.stalled || self.link.partial {
                self.replies.push_back(ContentMsg::RepresentationBytes {
                    offer,
                    representation,
                    offset: 0,
                    bytes: self.link.bytes.clone(),
                });
            }
            if !self.stalled {
                self.replies.push_back(ContentMsg::RepresentationEnd {
                    offer,
                    representation,
                    size: self.link.bytes.len() as u64,
                });
            }
            Ok(())
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, std::io::Result<Option<ContentMsg>>> {
        Box::pin(async move {
            if let Some(reply) = self.replies.pop_front() {
                return Ok(Some(reply));
            }
            if self.stalled {
                self.link.started.send(()).unwrap();
                std::future::pending().await
            } else {
                Ok(None)
            }
        })
    }
    fn finish(&mut self) {
        self.finished = true;
    }
}

pub(crate) struct Harness {
    // Drop the remote before its runtime.
    pub(crate) remote: Remote,
    pub(crate) bytes: Vec<u8>,
    link: Arc<Link>,
    started: Option<mpsc::Receiver<()>>,
    cancelled: mpsc::Receiver<()>,
    _runtime: tokio::runtime::Runtime,
}
impl Harness {
    pub(crate) fn new(platform: ClipPlatform, partial: bool) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut inventory = crate::content::tests::fixture().inventory();
        inventory.platform = platform;
        inventory.items[0]
            .representations
            .retain(|rep| rep.id == RepresentationId(1));
        let rep = &mut inventory.items[0].representations[0];
        let bytes = if platform == ClipPlatform::Windows {
            rep.format = NativeFormat::WindowsPredefined(13);
            rep.medium = Medium::HGlobal;
            "retry\0".encode_utf16().flat_map(u16::to_le_bytes).collect()
        } else {
            b"retry".to_vec()
        };
        let (started_tx, started) = mpsc::channel();
        let (cancelled_tx, cancelled) = mpsc::channel();
        let link = Arc::new(Link {
            answer: AtomicBool::new(false),
            bytes: bytes.clone(),
            started: started_tx,
            cancelled: cancelled_tx,
            partial,
        });
        let remote = Remote::on_runtime(
            std::path::Path::new(""),
            BatchId([41; 16]),
            inventory,
            platform,
            Arc::new(Transport(link.clone())),
            Demand::Clipboard,
            runtime.handle().clone(),
        )
        .unwrap();
        Self {
            remote,
            bytes,
            link,
            started: Some(started),
            cancelled,
            _runtime: runtime,
        }
    }
    pub(crate) fn expire_when_waiting(&mut self, clock: Arc<TestClock>) -> std::thread::JoinHandle<()> {
        let started = self.started.take().unwrap();
        std::thread::spawn(move || {
            started
                .recv_timeout(Duration::from_secs(2))
                .expect("remote request reached unanswered source");
            clock.expire();
        })
    }
    pub(crate) fn allow_retry(&self) {
        self.cancelled
            .recv_timeout(Duration::from_secs(2))
            .expect("timed-out request channel dropped");
        self.link.answer.store(true, Ordering::Release);
    }
    pub(crate) fn spec(&self) -> crate::NativeSpec {
        crate::NativeSpec {
            inventory: self.remote.inventory(),
            contents: Arc::new(self.remote.clone()),
            files: vec![],
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        }
    }
}

#[test]
fn native_deadline_cancels_partial_data_and_retries_same_offer() {
    for partial in [false, true] {
        let mut harness = Harness::new(ClipPlatform::Mac, partial);
        let clock = Arc::new(TestClock::default());
        let expiry = harness.expire_when_waiting(clock.clone());
        let started = Instant::now();
        clock.scope(|| {
            let _callback = Callback::begin();
            let error = harness.remote.open(RepresentationId(1)).err().unwrap();
            assert_eq!(reason(&error), Unavailable::NoProgress);
            assert!(timed_out());
        });
        expiry.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "manual clock must not sleep 5 s"
        );
        TEST_OUTCOMES.with_borrow(|outcomes| {
            assert_eq!(outcomes.last().unwrap().1, NativeReadOutcome::TimedOut);
            assert!(outcomes.last().unwrap().0 >= NATIVE_READ_TIMEOUT);
        });
        harness.allow_retry();
        let callback = Callback::begin();
        let bytes = read_all(&mut harness.remote.open(RepresentationId(1)).unwrap()).unwrap();
        assert_eq!(bytes, harness.bytes);
        callback.served();
        drop(callback);
        TEST_OUTCOMES.with_borrow(|outcomes| assert_eq!(outcomes.last().unwrap().1, NativeReadOutcome::Served));
    }
}

#[test]
fn callback_reports_failure_and_cancellation_without_ids() {
    let harness = Harness::new(ClipPlatform::Mac, false);
    {
        let _callback = Callback::begin();
        assert!(harness.remote.open(RepresentationId(99)).is_err());
    }
    TEST_OUTCOMES.with_borrow(|outcomes| assert_eq!(outcomes.last().unwrap().1, NativeReadOutcome::Failed));
    harness.remote.cancel();
    {
        let _callback = Callback::begin();
        assert!(harness.remote.open(RepresentationId(1)).is_err());
    }
    TEST_OUTCOMES.with_borrow(|outcomes| assert_eq!(outcomes.last().unwrap().1, NativeReadOutcome::Cancelled));
}

#[test]
fn sibling_reads_share_the_callback_deadline() {
    let harness = Harness::new(ClipPlatform::Mac, false);
    harness.link.answer.store(true, Ordering::Release);
    let clock = Arc::new(TestClock::default());
    clock.scope(|| {
        let _callback = Callback::begin();
        assert!(harness.remote.open(RepresentationId(1)).is_ok());
        clock.expire();
        // A converter asking for another representation cannot get a new 5 s.
        assert!(harness.remote.open(RepresentationId(1)).is_err());
        assert!(timed_out());
    });
    let callback = Callback::begin();
    assert!(harness.remote.open(RepresentationId(1)).is_ok());
    callback.served();
}
