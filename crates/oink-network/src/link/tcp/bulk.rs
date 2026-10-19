//! Each logical Low record is exactly one admitted v1 frame.
use super::*;
use crate::record::{RecordRead, RecordWrite};
use oink_protocol::{
    frame::MAX_LOW_MESSAGE,
    v1::{
        Message,
        admission::{Agreement, Context, Phase},
        frame::{self, FrameHeader, OpaqueFrame},
        registry::Transport,
    },
};

pub(in crate::link) struct BulkStream {
    pub link: LinkId,
    pub diagnostic_id: [u8; 16],
    agreement: Agreement,
    pub peer: DeviceId,
    pub kind: StreamKind,
    pub path: super::super::LinkPath,
    life: Arc<io::Life>,
    tx: io::Tx,
    rx: io::Rx,
    paced: Option<crate::pace::Paced>,
    send_remaining: Option<u64>,
    recv_remaining: Option<u64>,
    prefix: [u8; 6],
    prefix_read: usize,
    body: Vec<u8>,
    body_read: usize,
    length: Option<usize>,
    eof: bool,
}
impl std::fmt::Debug for BulkStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LowStream")
            .field("link", &self.link)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
impl BulkStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: Endpoint,
        agreement: Agreement,
        path: super::super::LinkPath,
        request: &LowRequest,
        stream: io::Stream,
        life: Arc<io::Life>,
        outgoing: bool,
        paced: Option<crate::pace::Paced>,
    ) -> Self {
        let limits = request.options.filter(|o| o.operation_max < EXTRA_MAX).map(|o| {
            if outgoing {
                (o.send_max, o.receive_max)
            } else {
                (o.receive_max, o.send_max)
            }
        });
        let (tx, rx) = io::split(stream, life.clone());
        Self {
            link: endpoint.link,
            diagnostic_id: request.request,
            agreement,
            peer: endpoint.peer.device_id,
            kind: StreamKind::from_wire(u8::try_from(request.kind).expect("admitted endpoint kind"))
                .expect("admitted endpoint kind"),
            path,
            life,
            tx,
            rx,
            paced,
            send_remaining: limits.map(|l| l.0),
            recv_remaining: limits.map(|l| l.1),
            prefix: [0; 6],
            prefix_read: 0,
            body: vec![],
            body_read: 0,
            length: None,
            eof: false,
        }
    }
    pub fn pacing_observer(&self) -> Arc<dyn Fn() -> Option<crate::LowPacingStatus> + Send + Sync> {
        let paced = self.paced.clone();
        Arc::new(move || paced.as_ref().map(|p| p.pacer.observation()))
    }
    pub fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let life = self.life.clone();
        Arc::new(move || life.effect_current())
    }
    pub async fn send(&mut self, message: &Message) -> Result<(), LowError> {
        let trace = (self.kind == StreamKind::Dnd).then(|| {
            oink_foundation::drag_trace::Trace::new(
                oink_foundation::drag_trace::stage::LOW_SEND,
                oink_foundation::drag_trace::Context {
                    object: self.link.0,
                    ..Default::default()
                }
                .with_request(self.diagnostic_id),
            )
        });
        let result = async {
            if !self.life.active() {
                return Err(LowError::Interrupted);
            }
            let bytes = frame::encode(message, MAX_LOW_MESSAGE).map_err(|_| LowError::TooLarge)?;
            let frame = OpaqueFrame::parse_exact(&bytes, MAX_LOW_MESSAGE).map_err(|_| LowError::TooLarge)?;
            self.context()
                .admit(frame.header)
                .map_err(|v| LowError::Io(std::io::Error::other(format!("{:?}", v.code))))?;
            if bytes.len() > MAX_LOW_MESSAGE {
                return Err(LowError::TooLarge);
            }
            if let Some(remaining) = &mut self.send_remaining {
                let Some(next) = remaining.checked_sub(bytes.len() as u64) else {
                    self.cancel();
                    return Err(LowError::Budget);
                };
                *remaining = next;
            }
            let header = &bytes[..6];
            let body = &bytes[6..];
            let result = if let Some(paced) = &self.paced {
                async {
                    paced.record(&mut self.tx, header, body).await?;
                    self.tx.flush_message().await
                }
                .await
            } else {
                async {
                    self.tx.record(header, body).await?;
                    self.tx.flush_message().await
                }
                .await
            };
            if let Err(e) = result {
                let error = self.io_error(e);
                self.cancel();
                return Err(error);
            }
            Ok(())
        }
        .await;
        if let Some(trace) = trace {
            trace.result(if result.is_ok() { 0 } else { -1 });
        }
        result
    }
    pub async fn recv(&mut self) -> Result<Option<Message>, LowError> {
        let trace = (self.kind == StreamKind::Dnd).then(|| {
            oink_foundation::drag_trace::Trace::new(
                oink_foundation::drag_trace::stage::LOW_RECV,
                oink_foundation::drag_trace::Context {
                    object: self.link.0,
                    ..Default::default()
                }
                .with_request(self.diagnostic_id),
            )
        });
        let result = async {
            if self.eof {
                return Ok(None);
            }
            if !self.life.active() {
                return Err(LowError::Interrupted);
            }
            let result = self.receive().await;
            if result.is_err() {
                self.cancel();
            }
            result
        }
        .await;
        if let Some(trace) = trace {
            trace.result(if result.is_ok() { 0 } else { -1 });
        }
        result
    }
    async fn receive(&mut self) -> Result<Option<Message>, LowError> {
        while self.prefix_read < 6 {
            let n = self
                .rx
                .read_record_bytes(&mut self.prefix[self.prefix_read..])
                .await
                .map_err(|e| self.io_error(e))?;
            if n == 0 {
                if self.prefix_read != 0 {
                    return Err(LowError::Io(std::io::Error::other("partial Low length")));
                }
                self.rx.finish_read();
                self.eof = true;
                return Ok(None);
            }
            self.prefix_read += n;
        }
        if self.length.is_none() {
            let header = FrameHeader::parse(&self.prefix, MAX_LOW_MESSAGE)
                .map_err(|_| LowError::TooLarge)?
                .ok_or(LowError::TooLarge)?;
            self.context()
                .admit(header)
                .map_err(|v| LowError::Io(std::io::Error::other(format!("{:?}", v.code))))?;
            let length = header.total_length();
            if let Some(remaining) = &mut self.recv_remaining {
                *remaining = remaining.checked_sub(length as u64).ok_or(LowError::Budget)?;
            }
            self.body.resize(length, 0);
            self.body[..6].copy_from_slice(&self.prefix);
            self.body_read = 6;
            self.length = Some(length);
        }
        while self.body_read < self.body.len() {
            let n = self
                .rx
                .read_record_bytes(&mut self.body[self.body_read..])
                .await
                .map_err(|e| self.io_error(e))?;
            if n == 0 {
                return Err(LowError::Io(std::io::Error::other("partial Low message")));
            }
            self.body_read += n;
        }
        let frame = OpaqueFrame::parse_exact(&self.body, MAX_LOW_MESSAGE)
            .map_err(|e| LowError::Io(std::io::Error::other(e)))?;
        let message = self
            .context()
            .decode(frame)
            .map_err(|v| LowError::Io(std::io::Error::other(format!("{:?}", v.code))))?;
        self.prefix_read = 0;
        self.body_read = 0;
        self.body.clear();
        self.length = None;
        Ok(Some(message))
    }
    fn context(&self) -> Context<'_> {
        Context {
            kind: self.kind as u16,
            transport: Transport::Low,
            phase: Phase::Member,
            agreement: &self.agreement,
            direct: self.path == super::super::LinkPath::Direct,
            old_member_lane: false,
        }
    }
    fn io_error(&self, error: std::io::Error) -> LowError {
        use std::io::ErrorKind;
        // Socket closure may arrive before the N1 cancellation notification.
        // An operation that never received END was interrupted in either order.
        if !self.life.active()
            || matches!(
                error.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionReset
                    | ErrorKind::BrokenPipe
            )
        {
            LowError::Interrupted
        } else {
            LowError::Io(error)
        }
    }
    pub fn finish(&mut self) {
        self.tx.finish();
    }
    pub fn cancel(&mut self) {
        self.tx.abort();
        self.rx.stop();
    }
}
