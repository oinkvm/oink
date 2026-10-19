use super::{testing, *};
use oink_protocol::frame::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn tag() -> StreamTag {
    StreamTag {
        kind: StreamKind::Input,
        ordinal: 1,
    }
}

#[test]
fn reliable_bounds_order_and_abort() {
    let (tx, mut rx) = testing::reliable_pair(LinkId(1), tag(), QueueSpec { messages: 2, bytes: 5 });
    assert_eq!(tx.post(b"abc"), Ok(()));
    assert_eq!(tx.post(b"def"), Err(PostError::Full));
    assert_eq!(tx.post(b"de"), Ok(()));
    assert_eq!(tx.post(b""), Err(PostError::Full));
    assert_eq!(rx.take().unwrap().unwrap(), b"abc");
    assert_eq!(rx.take().unwrap().unwrap(), b"de");
    assert_eq!(rx.take(), Ok(None));
    tx.post(b"old").unwrap();
    drop(tx);
    assert_eq!(rx.take(), Err(Closed));
}

#[test]
fn receiver_drop_closes_writer() {
    let (tx, rx) = testing::reliable_pair(LinkId(1), tag(), QueueSpec::reliable(StreamKind::Input).0);
    drop(rx);
    assert_eq!(tx.post(b"gone"), Err(PostError::Closed));
}

#[test]
fn datagrams_drop_oldest_by_both_bounds() {
    let (tx, rx) = testing::datagram_pair(LinkId(1), tag(), QueueSpec { messages: 2, bytes: 5 });
    tx.post(b"abc").unwrap();
    tx.post(b"de").unwrap();
    tx.post(b"fgh").unwrap();
    assert_eq!(rx.take().unwrap().unwrap(), b"de");
    assert_eq!(rx.take().unwrap().unwrap(), b"fgh");
    assert_eq!(tx.post(vec![0; MAX_DATAGRAM]), Err(DatagramError::TooLarge));
    drop(rx);
    assert_eq!(tx.post(b"gone"), Err(DatagramError::Closed));
}

#[tokio::test]
async fn wake_and_async_read_do_not_lose_notifications() {
    let (tx, mut rx) = testing::reliable_pair(LinkId(1), tag(), QueueSpec::reliable(StreamKind::Input).0);
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    rx.set_wake(Arc::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    }));
    tx.post(b"first").unwrap();
    assert!(wakes.load(Ordering::Relaxed) > 0);
    assert_eq!(rx.next().await.unwrap(), b"first");
    let writer = tokio::spawn(async move {
        tokio::task::yield_now().await;
        tx.post(b"next").unwrap();
        tokio::task::yield_now().await;
        tx
    });
    assert_eq!(rx.next().await.unwrap(), b"next");
    let tx = writer.await.unwrap();
    drop(tx);
    assert_eq!(rx.next().await, Err(Closed));
}

#[test]
fn framing_boundaries_and_partial_headers() {
    let header = StreamHeader {
        route_tag: 42,
        priority: Priority::Interactive,
        tag: tag(),
        datagram_flow: 128,
    };
    let mut bytes = Vec::new();
    header.encode(&mut bytes);
    for end in 0..bytes.len() {
        assert_eq!(StreamHeader::decode(&bytes[..end]), Ok(None));
    }
    assert_eq!(StreamHeader::decode(&bytes), Ok(Some((header, bytes.len()))));
    bytes[8] = 2;
    assert_eq!(StreamHeader::decode(&bytes), Err(FrameError::Header));
}

#[test]
fn datagram_snapshot_clones_preserve_flow_until_last_handle() {
    let (tx, rx) = testing::datagram_pair(LinkId(1), tag(), QueueSpec { messages: 2, bytes: 16 });
    let published_tx = tx.clone();
    let published_rx = rx.clone();
    drop(tx);
    drop(rx);
    published_tx.post(b"live").unwrap();
    assert_eq!(published_rx.take().unwrap().unwrap(), b"live");
    let next = published_rx.clone();
    drop(published_rx);
    published_tx.post(b"next").unwrap();
    assert_eq!(next.take().unwrap().unwrap(), b"next");
    drop(next);
    assert_eq!(published_tx.post(b"closed"), Err(DatagramError::Closed));
}

#[test]
fn reliable_owner_is_woken_for_space_without_replacing_reader_wake() {
    let (tx, mut rx) = testing::reliable_pair(LinkId(1), tag(), QueueSpec { messages: 1, bytes: 4 });
    let readers = Arc::new(AtomicUsize::new(0));
    let writers = Arc::new(AtomicUsize::new(0));
    let read = readers.clone();
    rx.set_wake(Arc::new(move || {
        read.fetch_add(1, Ordering::Relaxed);
    }));
    let write = writers.clone();
    tx.set_wake(Arc::new(move || {
        write.fetch_add(1, Ordering::Relaxed);
    }));
    tx.post(b"full").unwrap();
    assert_eq!(tx.post(b"x"), Err(PostError::Full));
    let before = writers.load(Ordering::Relaxed);
    assert_eq!(rx.take().unwrap().unwrap(), b"full");
    assert!(writers.load(Ordering::Relaxed) > before);
    assert!(readers.load(Ordering::Relaxed) > 0);
    tx.post(b"next").unwrap();
    let before = writers.load(Ordering::Relaxed);
    drop(rx);
    assert!(writers.load(Ordering::Relaxed) > before);
}
