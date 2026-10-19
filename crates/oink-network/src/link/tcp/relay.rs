//! At most one relay. Only the network-installed forward grant chooses hop two;
//! the pool forwards bounded record fragments without decoding application data.
use super::*;
use crate::record::{RecordRead, RecordWrite};

impl Core {
    pub(super) async fn endpoint_after_relay(
        self: &Arc<Self>,
        mut hop: Hop,
        peer: DeviceId,
    ) -> Result<bulk::BulkStream, NetError> {
        if hop.pool.key.peer != peer {
            let life = hop.life.clone();
            let response = {
                let mut stream = io::Guarded::new(hop.stream(), &life);
                tokio::time::timeout(
                    self.cfg.dial_timeout * 8 + Duration::from_secs(2),
                    read_setup(&mut stream),
                )
                .await
            };
            let reply = response.map_err(|_| self.error(&hop.pool, "relay setup", "second hop timeout"))??;
            match relay_result(&hop.request, hop.pool.key.peer, peer, reply) {
                Ok(()) => {
                    self.network.hub.failures.rcu(|old| {
                        let mut next = (**old).clone();
                        next.remove(&hop.authorization.grant.binding);
                        next
                    });
                    self.network.hub.wake.notify_one();
                }
                Err(NetError::LowUnavailable(failure)) => {
                    // A second-hop refusal belongs only to this logical path.
                    // Other endpoints sharing our physical first hop remain usable.
                    if hop.authorization.current() {
                        self.network.hub.failures.rcu(|old| {
                            let mut next = (**old).clone();
                            next.insert(hop.authorization.grant.binding, failure.clone());
                            next
                        });
                        self.network.hub.wake.notify_one();
                    }
                    return Err(NetError::LowUnavailable(failure));
                }
                Err(error) => return Err(self.error(&hop.pool, "relay setup", error)),
            }
        }
        self.endpoint(hop, peer)
    }
    fn endpoint(&self, mut hop: Hop, peer: DeviceId) -> Result<bulk::BulkStream, NetError> {
        if !hop.authorization.current() || !hop.life.active() {
            return Err(NetError::Closed);
        }
        let scope = hop.authorization.grant.endpoint.ok_or(NetError::Closed)?;
        if scope.peer.device_id != peer {
            return Err(NetError::Closed);
        }
        Ok(bulk::BulkStream::new(
            scope,
            self.network.agreement(hop.request.binding).ok_or(NetError::NotReady)?,
            if hop.pool.key.peer == peer {
                super::super::LinkPath::Direct
            } else {
                super::super::LinkPath::Relayed { via: hop.pool.key.peer }
            },
            &hop.request,
            hop.stream.take().expect("unsplit hop"),
            hop.life.clone(),
            hop.outgoing,
            hop.paced.clone(),
        ))
    }
    pub(super) fn deliver(self: &Arc<Self>, hop: Hop) -> Reply<'static, anyhow::Result<()>> {
        let core = self.clone();
        Box::pin(async move {
            if hop.request.purpose != LowTcpPurpose::Transfer {
                let auth = hop.authorization.clone();
                let worker = core.clone();
                core.spawn(auth, async move {
                    let _ = worker.finish_warm(hop, None).await;
                });
                return Ok(());
            }
            if let Some(forward) = hop.authorization.forward {
                let worker = core.clone();
                let auth = hop.authorization.clone();
                core.spawn(auth, async move {
                    worker.serve_relay(hop, forward).await;
                });
                return Ok(());
            }
            anyhow::ensure!(
                hop.request.binding.target.device_id == core.cfg.boot.device_id,
                "Low grant does not terminate here"
            );
            let auth = hop.authorization.clone();
            let peer = hop.request.binding.source.device_id;
            let mut stream = core.endpoint(hop, peer)?;
            let worker = core.clone();
            // TLS admission is complete. Waiting for the operation's first
            // message belongs to its grant, not to the listener's handshake timeout.
            core.spawn(auth, async move {
                if let Ok(Some(first)) = stream.recv().await {
                    let dispatch = (stream.kind == StreamKind::Dnd).then(|| {
                        oink_foundation::drag_trace::Trace::new(
                            oink_foundation::drag_trace::stage::LOW_DISPATCH,
                            oink_foundation::drag_trace::Context {
                                object: stream.link.0,
                                ..Default::default()
                            }
                            .with_request(stream.diagnostic_id),
                        )
                    });
                    let inbound = super::super::InboundLow {
                        stream: LowStream::new(stream),
                        first,
                    };
                    let _ = worker
                        .network
                        .commands
                        .send(super::super::handle::Command::LowInbound(Box::new(inbound)));
                    worker.network.hub.wake.notify_one();
                    if let Some(trace) = dispatch {
                        trace.result(0);
                    }
                }
            });
            Ok(())
        })
    }
    async fn serve_relay(self: Arc<Self>, mut a: Hop, forward: Grant) {
        let second = async {
            let (pool, auth) = self.admit(forward, None).await?;
            self.wait_warm(pool.clone(), auth.clone()).await?;
            self.open_lease(
                pool,
                auth,
                a.request.route_tag,
                a.request.options,
                LowTcpPurpose::Transfer,
                None,
                a.request.kind,
                None,
            )
            .await
        };
        let mut done = a.life.done.subscribe();
        let second = tokio::select! {biased;_=wait_closed(&mut done)=>return,result=second=>result};
        let b = match second {
            Ok(hop) => hop,
            Err(error) => {
                a.cancel_on_drop = false;
                let binding = a.request.binding;
                let id = a.request.request;
                let failure = match error {
                    NetError::LowUnavailable(failure) => failure,
                    error => crate::LowUnavailable {
                        from: self.cfg.boot.device_id,
                        to: forward.neighbor.device_id,
                        reason: crate::LowFailureReason::Tcp {
                            stage: "relay setup".into(),
                            detail: error.to_string(),
                        },
                    },
                };
                let life = a.life.clone();
                let _ = tokio::time::timeout(
                    self.cfg.dial_timeout,
                    write_setup(
                        &mut io::Guarded::new(a.stream(), &life),
                        binding,
                        &LowMsg::RelayUnavailable { request: id, failure },
                    ),
                )
                .await;
                // The typed TCP reply must not be overtaken by a QUIC Cancel.
                // Closing this failed setup locally supplies EOF after its reply.
                life.close_without_control();
                return;
            }
        };
        let binding = a.request.binding;
        let id = a.request.request;
        let life = a.life.clone();
        if !matches!(
            tokio::time::timeout(
                self.cfg.dial_timeout,
                write_setup(
                    &mut io::Guarded::new(a.stream(), &life),
                    binding,
                    &LowMsg::Ready {
                        request: id,
                        result: Ok(())
                    }
                )
            )
            .await,
            Ok(Ok(()))
        ) {
            return;
        }
        let options = a.request.options.filter(|o| o.operation_max < EXTRA_MAX);
        let life_a = a.life.clone();
        let life_b = b.life.clone();
        life_a.hold_reuse();
        life_b.hold_reuse();
        let paced_a = a.paced.clone();
        let paced_b = b.paced.clone();
        let (mut tx_a, mut rx_a) = a.into_io();
        let (mut tx_b, mut rx_b) = b.into_io();
        let aborted = async {
            let mut done_b = life_b.done.subscribe();
            tokio::select! {_=async{wait_closed(&mut done).await;if life_a.gracefully_finished(){std::future::pending::<()>().await;}}=>{},
            _=async{wait_closed(&mut done_b).await;if life_b.gracefully_finished(){std::future::pending::<()>().await;}}=>{}}
        };
        let forwarding = async {
            tokio::try_join!(
                forward_low(&mut rx_a, &mut tx_b, paced_b.as_ref(), options.map(|o| o.send_max)),
                forward_low(&mut rx_b, &mut tx_a, paced_a.as_ref(), options.map(|o| o.receive_max))
            )
        };
        let success = tokio::select! {_=aborted=>false,result=forwarding=>result.is_ok()};
        if success {
            life_a.release_reuse();
            life_b.release_reuse();
        } else {
            life_a.close();
            life_b.close();
        }
    }
}
pub(super) fn relay_result(
    expected: &LowRequest,
    relay: DeviceId,
    target: DeviceId,
    reply: (LowBinding, LowMsg),
) -> Result<(), NetError> {
    let (binding, message) = reply;
    if binding != expected.binding {
        return Err(NetError::Invalid("relay reply binding mismatch".into()));
    }
    match message {
        LowMsg::Ready {
            request,
            result: Ok(()),
        } if request == expected.request => Ok(()),
        LowMsg::RelayUnavailable { request, failure }
            if request == expected.request && failure.from == relay && failure.to == target =>
        {
            Err(NetError::LowUnavailable(failure))
        }
        _ => Err(NetError::Invalid("relay reply request or failed hop mismatch".into())),
    }
}
async fn fill<R: RecordRead>(stream: &mut R, bytes: &mut [u8]) -> std::io::Result<bool> {
    let mut read = 0;
    while read < bytes.len() {
        let n = stream.read_record_bytes(&mut bytes[read..]).await?;
        if n == 0 {
            return if read == 0 {
                Ok(false)
            } else {
                Err(std::io::Error::other("partial relay record"))
            };
        }
        read += n;
    }
    Ok(true)
}
pub(super) async fn forward_low<R: RecordRead, W: RecordWrite>(
    input: &mut R,
    output: &mut W,
    paced: Option<&crate::pace::Paced>,
    mut remaining: Option<u64>,
) -> std::io::Result<()> {
    let mut body = [0u8; 16 * 1024];
    loop {
        let mut prefix = [0; 6];
        if !fill(input, &mut prefix).await? {
            input.finish_read();
            output.finish_records().await?;
            return Ok(());
        }
        let header = oink_protocol::v1::frame::FrameHeader::parse(&prefix, oink_protocol::frame::MAX_LOW_MESSAGE)
            .map_err(std::io::Error::other)?
            .ok_or_else(|| std::io::Error::other("partial Low header"))?;
        let length = header.total_length() - 6;
        if let Some(remaining) = &mut remaining {
            *remaining = remaining
                .checked_sub(length as u64 + 6)
                .ok_or_else(|| std::io::Error::other("Low byte budget exceeded"))?;
        }
        if let Some(paced) = paced {
            paced.write_all(output, &prefix).await?;
        } else {
            output.record(&prefix, &[]).await?;
        }
        let mut rest = length;
        while rest > 0 {
            let n = rest.min(body.len());
            if !fill(input, &mut body[..n]).await? {
                return Err(std::io::Error::other("partial Low relay body"));
            }
            if let Some(paced) = paced {
                paced.write_all(output, &body[..n]).await?;
            } else {
                output.record(&[], &body[..n]).await?;
            }
            rest -= n;
        }
        output.flush_message().await?;
    }
}
