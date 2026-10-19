//! A relay carries stream bytes without decoding the records or their modules.
use super::{wire, *};
use oink_foundation::session::PathId;
use oink_protocol::v1::frame::StreamHeader;
use std::task::{Context, Poll};

pub(super) struct Pipe {
    pub path: PathId,
    pub source: usize,
    pub target: usize,
    pub header: StreamHeader,
    pub original_flow: u32,
    pub reader: quinn::RecvStream,
    writer: Option<quinn::SendStream>,
    opening: wire::Opening,
    stopping: wire::Stopping,
    pending: Vec<u8>,
    offset: usize,
    length: usize,
    done: bool,
    pub datagrams: Arc<queue::DatagramQueue>,
}
impl Pipe {
    pub fn new(
        path: PathId,
        source: usize,
        target: usize,
        reader: quinn::RecvStream,
        mut header: StreamHeader,
        flow: u32,
    ) -> Self {
        let original_flow = header.datagram_flow;
        header.datagram_flow = flow;
        let mut pending = Vec::with_capacity(16 * 1024);
        pending.extend_from_slice(&header.encode().expect("validated relay header"));
        let length = pending.len();
        pending.resize(16 * 1024, 0);
        Self {
            path,
            source,
            target,
            header: header.clone(),
            original_flow,
            reader,
            writer: None,
            opening: Default::default(),
            stopping: Default::default(),
            pending,
            offset: 0,
            length,
            done: false,
            datagrams: queue::DatagramQueue::new(QueueSpec {
                messages: 64,
                bytes: 64 * 1100,
            }),
        }
    }
    pub fn stop(&mut self, code: quinn::VarInt) {
        let _ = self.reader.stop(code);
        if let Some(writer) = &mut self.writer {
            let _ = writer.reset(code);
        }
        self.done = true;
        self.datagrams.close();
    }
    fn check_reset(&mut self, cx: &mut Context<'_>) -> bool {
        // As for endpoint readers, use a non-consuming read probe: mixing Quinn's
        // reset-only waiter with a buffered FIN leaves a stale blocked reader.
        if let Poll::Ready(Err(error)) = wire::poll(self.reader.read_chunk(0, true), cx) {
            self.stop(match error {
                quinn::ReadError::Reset(code) => code,
                _ => 0u32.into(),
            });
            return true;
        }
        false
    }
    fn check_stopped(&mut self, cx: &mut Context<'_>) -> bool {
        let writer = self.writer.as_mut().expect("writer");
        match self.stopping.poll(cx, || Box::pin(writer.stopped())) {
            Poll::Ready(Ok(Some(code))) => {
                self.stop(code);
                return true;
            }
            Poll::Ready(Err(_)) => {
                self.stop(0u32.into());
                return true;
            }
            _ => {}
        }
        false
    }
    pub fn step(&mut self, conn: &quinn::Connection, cx: &mut Context<'_>) -> bool {
        if self.done {
            return false;
        }
        if self.length != 0 && self.check_reset(cx) {
            return true;
        }
        if self.writer.is_none() {
            match self.opening.poll(cx, || {
                let conn = conn.clone();
                Box::pin(async move { conn.open_uni().await })
            }) {
                Poll::Ready(Ok(writer)) => {
                    let _ = writer.set_priority(if self.header.priority == 0 { 10 } else { 0 });
                    self.writer = Some(writer);
                }
                Poll::Pending => return false,
                Poll::Ready(Err(_)) => {
                    self.stop(0u32.into());
                    return true;
                }
            }
        }
        if self.length == 0 {
            match self.reader.poll_read(cx, &mut self.pending) {
                Poll::Ready(Ok(0)) => {
                    if self.check_stopped(cx) {
                        return true;
                    }
                    let _ = self.writer.as_mut().expect("writer").finish();
                    self.done = true;
                    self.datagrams.close();
                    return true;
                }
                Poll::Ready(Ok(n)) => {
                    self.length = n;
                }
                Poll::Ready(Err(quinn::ReadError::Reset(code))) => {
                    self.stop(code);
                    return true;
                }
                Poll::Ready(Err(_)) => {
                    self.stop(0u32.into());
                    return true;
                }
                Poll::Pending => {
                    return self.check_stopped(cx);
                }
            }
        }
        let writer = self.writer.as_mut().expect("writer");
        match wire::poll(writer.write(&self.pending[self.offset..self.length]), cx) {
            Poll::Ready(Ok(n)) => {
                self.offset += n;
                if self.offset == self.length {
                    self.length = 0;
                    self.offset = 0;
                }
                true
            }
            Poll::Ready(Err(quinn::WriteError::Stopped(code))) => {
                self.stop(code);
                true
            }
            Poll::Ready(Err(_)) => {
                self.stop(0u32.into());
                true
            }
            Poll::Pending => false,
        }
    }
    pub fn done(&self) -> bool {
        self.done
    }
}
impl Drop for Pipe {
    fn drop(&mut self) {
        if !self.done {
            self.stop(0u32.into());
        }
    }
}
