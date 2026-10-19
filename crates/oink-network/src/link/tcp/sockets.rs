use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufStream},
    net::{TcpListener, TcpStream},
};
const SETUP_LIMIT: usize = 4096;
type Consumed = (Arc<io::Life>, Option<mpsc::Sender<ReplyMessage>>, Authorization);

pub(super) struct Hop {
    pub(super) stream: Option<io::Stream>,
    pub(super) life: Arc<io::Life>,
    pub(super) request: LowRequest,
    pub(super) authorization: Authorization,
    pub(super) pool: Arc<Pool>,
    pub(super) paced: Option<crate::pace::Paced>,
    pub(super) outgoing: bool,
    pub(super) owner: Weak<Core>,
    pub(super) cancel_on_drop: bool,
}
impl Hop {
    pub(super) fn stream(&mut self) -> &mut io::Stream {
        self.stream.as_mut().expect("unsplit hop")
    }
    pub(super) fn into_io(mut self) -> (io::Tx, io::Rx) {
        io::split(self.stream.take().expect("unsplit hop"), self.life.clone())
    }
}
impl Drop for Hop {
    fn drop(&mut self) {
        if self.stream.is_some() {
            self.life.close();
            if self.cancel_on_drop
                && let Some(owner) = self.owner.upgrade()
            {
                let _ = owner.cancel_hint(self.authorization.grant, self.request.request);
            }
        }
    }
}
impl Core {
    pub(super) fn ensure_listener(&self) {
        if *self.stopped.borrow() {
            return;
        }
        let mut listener = lock(&self.listener);
        if listener.is_some() {
            return;
        }
        let create = || -> std::io::Result<TcpListener> {
            let addr = SocketAddr::new(self.cfg.bind_ip, self.cfg.port);
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(addr),
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;
            #[cfg(not(windows))]
            socket.set_reuse_address(true)?;
            socket.set_nonblocking(true)?;
            socket.bind(&addr.into())?;
            socket.listen(128)?;
            TcpListener::from_std(socket.into())
        };
        match create() {
            Ok(socket) => {
                *listener = socket.local_addr().ok();
                *lock(&self.listener_failure) = None;
                let owner = self.arc();
                self.spawn_background(async move {
                    let permits = Arc::new(Semaphore::new(32));
                    let mut stopped = owner.stopped.subscribe();
                    loop {
                        let accepted =
                            tokio::select! {biased; _=wait_closed(&mut stopped)=>break, result=socket.accept()=>result};
                        let Ok((socket, _)) = accepted else { break };
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            continue;
                        };
                        let core = owner.clone();
                        owner.spawn_background(async move {
                            let _permit = permit;
                            let trace = super::super::diagnostics::Steps::start(None, super::super::diagnostics::hex(&rand::random::<[u8;16]>()), "tcp_accept");
                            tracing::info!(target: "oink_link_diagnostics", kind = "low_step", stage = "tcp_accept", result = "completed", candidate = ?socket.peer_addr(),  "TCP accepted");
                            match tokio::time::timeout(core.cfg.dial_timeout, core.accept(socket, &trace)).await {
                                Ok(result) => trace.result(&result),
                                Err(_) => trace.end("failed", "timeout", "TCP accept deadline"),
                            }
                        });
                    }
                });
            }
            Err(error) => *lock(&self.listener_failure) = Some(error.to_string()),
        }
    }
    pub(super) fn advert(&self) -> LowMsg {
        LowMsg::Info {
            paced: self.cfg.pacing,
            port: lock(&self.listener).map(|a| a.port()),
            failure: lock(&self.listener_failure).clone(),
        }
    }
    pub(super) async fn dial(
        &self,
        pool: &Arc<Pool>,
        auth: &Authorization,
        request: &LowRequest,
        reverse: bool,
        trace: &super::super::diagnostics::Steps,
    ) -> Result<(io::Stream, std::net::TcpStream), NetError> {
        if !auth.current() || !self.network.validate_low(auth.grant).await {
            return Err(NetError::Closed);
        }
        let mut addr = auth.grant.next_hop_address;
        let advert = lock(&pool.state).advert.clone();
        let port = advert.as_ref().and_then(|a| a.port).ok_or_else(|| {
            self.error(
                pool,
                "listener",
                advert
                    .and_then(|a| a.failure)
                    .unwrap_or_else(|| "no TCP listener".into()),
            )
        })?;
        addr.set_port(port); // Preserve IPv6 link-local scope from the authenticated neighbor address.
        let dialer = if addr.is_ipv6() {
            tokio::net::TcpSocket::new_v6()?
        } else {
            tokio::net::TcpSocket::new_v4()?
        };
        trace.enter("tcp_connect");
        tracing::info!(target: "oink_link_diagnostics", kind = "low_step", stage = "tcp_connect", result = "entered", candidate = %addr, reverse, "TCP dial destination");
        let socket = dialer.connect(addr).await?;
        trace.enter("tls");
        let (io, socket) = buffered(socket)?;
        let cfg = self
            .cfg
            .creds
            .bulk_client_config(pool.key.peer)
            .map_err(|e| self.error(pool, "TLS config", e))?;
        let mut stream = tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(
                rustls::pki_types::ServerName::try_from(tls::SERVER_NAME).expect("static TLS name"),
                io,
            )
            .await?;
        stream.get_mut().1.set_buffer_limit(Some(io::BUFFER));
        let mut stream = io::Stream::Client(stream);
        check_tls(&stream, pool.key.peer).map_err(|e| self.error(pool, "TLS identity", e))?;
        trace.enter("bind_request");
        write_setup(
            &mut io::Guarded::authorized(&mut stream, auth),
            request.binding,
            &LowMsg::Bind {
                request: request.clone(),
                reverse,
            },
        )
        .await?;
        trace.enter("bind_reply");
        match read_setup(&mut io::Guarded::authorized(&mut stream, auth)).await? {
            (
                binding,
                LowMsg::Ready {
                    request: id,
                    result: Ok(()),
                },
            ) if binding == request.binding && id == request.request => Ok((stream, socket)),
            other => Err(self.error(pool, "bind", format!("unexpected TCP response: {other:?}"))),
        }
    }
    pub(super) async fn accept(
        self: Arc<Self>,
        socket: TcpStream,
        trace: &super::super::diagnostics::Steps,
    ) -> anyhow::Result<()> {
        trace.enter("tls");
        let (io, socket) = buffered(socket)?;
        let mut stream = tokio_rustls::TlsAcceptor::from(Arc::new(self.cfg.creds.bulk_server_config()?))
            .accept(io)
            .await?;
        stream.get_mut().1.set_buffer_limit(Some(io::BUFFER));
        let peer = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .and_then(|c| tls::device_id_from_cert(c))
            .ok_or_else(|| anyhow::anyhow!("missing TLS identity"))?;
        trace.peer(peer);
        let mut stream = io::Stream::Server(stream);
        check_tls(&stream, peer).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            lock(&self.pools)
                .iter()
                .any(|(k, p)| k.peer == peer && p.current() && !lock(&p.grants).is_empty()),
            "TCP identity has no authorized pool"
        );
        trace.enter("bind_request");
        let (binding, message) = read_setup(&mut stream).await?;
        let LowMsg::Bind { request, reverse } = message else {
            anyhow::bail!("missing Low Bind")
        };
        anyhow::ensure!(
            binding == request.binding && request.connection == request.request,
            "invalid new socket binding"
        );
        trace.connection(super::super::diagnostics::hex(&binding.hop.connection_id.random));
        trace.operation(super::super::diagnostics::hex(&request.request));
        let key = PoolKey {
            peer,
            connection: binding.hop.connection_id,
        };
        let pool = lock(&self.pools)
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown TCP pool"))?;
        if request.purpose == LowTcpPurpose::Race {
            return self
                .accept_race_candidate(pool, request, reverse, stream, socket, trace)
                .await;
        }
        let (life, reply, auth) = self.consume(&pool, &request, reverse, reverse, socket)?;
        let _work = auth.work().ok_or_else(|| anyhow::anyhow!("Low scope cancelled"))?;
        trace.enter("bind_reply");
        write_setup(
            &mut io::Guarded::new(&mut stream, &life),
            binding,
            &LowMsg::Ready {
                request: request.request,
                result: Ok(()),
            },
        )
        .await?;
        trace.enter("deliver");
        let hop = self.hop(pool, &request, auth, stream, life, reverse)?;
        if reverse {
            reply
                .ok_or_else(|| anyhow::anyhow!("reverse waiter absent"))?
                .send(ReplyMessage::Stream(Box::new(hop)))
                .await
                .map_err(|_| anyhow::anyhow!("reverse waiter cancelled"))?;
        } else {
            self.deliver(hop).await?;
        }
        Ok(())
    }
    pub(super) fn consume(
        self: &Arc<Self>,
        pool: &Arc<Pool>,
        request: &LowRequest,
        outgoing: bool,
        reverse: bool,
        socket: std::net::TcpStream,
    ) -> Result<Consumed, NetError> {
        let mut st = lock(&pool.state);
        let pending = st.pending.get(&request.request).ok_or(NetError::Closed)?;
        if !pool.current()
            || !pending.authorization.current()
            || pending.request != *request
            || pending.outgoing != outgoing
            || pending.reverse != reverse
        {
            return Err(NetError::Closed);
        }
        let pending = st.pending.remove(&request.request).expect("validated request");
        let auth = pending.authorization;
        let life = io::Life::new(
            socket,
            self.runtime.clone(),
            self.cfg.dial_timeout,
            request,
            &auth,
            Some(pending.permit),
            st.generation,
        );
        auth.bind(life.clone())?;
        let weak = Arc::downgrade(self);
        let grant = auth.grant;
        let id = request.request;
        life.cancel_via(move || {
            if let Some(core) = weak.upgrade() {
                let _ = core.cancel_hint(grant, id);
            }
        });
        let dialer = if outgoing != reverse {
            self.cfg.boot.device_id
        } else {
            pool.key.peer
        };
        // Migration can preserve the remote pool while recreating this one.
        // Its authenticated fresh Warm socket proves the usable direction even
        // when the remote retained its choice and therefore sent no new Race.
        let learned =
            !st.direction_known && request.purpose == LowTcpPurpose::Warm && request.request == request.connection;
        if learned {
            st.direction_known = true;
            st.prefer_reverse = dialer != self.cfg.boot.device_id;
        }
        self.recycle(pool.clone(), request.clone(), auth.clone(), outgoing, dialer, &life);
        st.active.retain(|_, a| a.upgrade().is_some_and(|l| l.active()));
        st.active.insert(
            request.request,
            Active {
                life: Arc::downgrade(&life),
                binding: request.binding,
                purpose: request.purpose,
                connection: request.connection,
                outgoing,
                extra: request.extra,
                idle: false,
                keep_alive: request.keep_alive,
            },
        );
        if outgoing {
            if request.request == request.connection {
                st.opened += 1;
            } else {
                st.reused += 1;
            }
        }
        st.dialer = Some(dialer);
        let first_inbound = request.connection == request.request
            && outgoing == reverse
            && lock(&self.inbound).insert(pool.key.connection);
        let recovered = lock(&pool.failure).take().is_some();
        // Status publication reads this pool's state again; leave the lease
        // mutation's critical section before taking that snapshot.
        drop(st);
        if first_inbound || recovered {
            self.status_changed();
        }
        if learned {
            pool.notify.notify_waiters();
            self.start_warm(pool.clone(), auth.clone());
        }
        Ok((life, pending.reply, auth))
    }
    pub(super) fn hop(
        self: &Arc<Self>,
        pool: Arc<Pool>,
        request: &LowRequest,
        authorization: Authorization,
        stream: io::Stream,
        life: Arc<io::Life>,
        outgoing: bool,
    ) -> Result<Hop, NetError> {
        if !life.active() || !authorization.current() {
            return Err(NetError::Closed);
        }
        let paced = (request.paced && !request.options.is_some_and(|o| o.operation_max <= 256 * 1024)).then(|| {
            crate::pace::Paced {
                pacer: pool.pacer.clone(),
            }
        });
        Ok(Hop {
            stream: Some(stream),
            life,
            request: request.clone(),
            authorization,
            pool,
            paced,
            outgoing,
            owner: Arc::downgrade(self),
            cancel_on_drop: true,
        })
    }
    pub(super) fn error(&self, pool: &Pool, stage: &str, error: impl std::fmt::Display) -> NetError {
        let failure = crate::LowUnavailable {
            from: self.cfg.boot.device_id,
            to: pool.key.peer,
            reason: crate::LowFailureReason::Tcp {
                stage: stage.into(),
                detail: error.to_string(),
            },
        };
        let changed = {
            let mut previous = lock(&pool.failure);
            let changed = previous.as_ref() != Some(&failure);
            *previous = Some(failure.clone());
            changed
        };
        if changed {
            self.status_changed();
        }
        NetError::LowUnavailable(failure)
    }
}
fn buffered(socket: TcpStream) -> std::io::Result<(BufStream<TcpStream>, std::net::TcpStream)> {
    socket.set_nodelay(true)?;
    let socket = socket.into_std()?;
    let control = socket.try_clone()?;
    Ok((
        BufStream::with_capacity(io::BUFFER, io::BUFFER, TcpStream::from_std(socket)?),
        control,
    ))
}
fn check_tls(stream: &io::Stream, peer: DeviceId) -> Result<(), String> {
    let (_, tls) = stream.get_ref();
    if tls.alpn_protocol() != Some(oink_protocol::v1::LOW_ALPN)
        || tls.protocol_version() != Some(rustls::ProtocolVersion::TLSv1_3)
        || tls.negotiated_cipher_suite().map(|s| s.suite()) != Some(rustls::CipherSuite::TLS13_AES_128_GCM_SHA256)
        || tls
            .peer_certificates()
            .and_then(|c| c.first())
            .and_then(|c| tls::device_id_from_cert(c))
            != Some(peer)
    {
        return Err("TLS identity, ALPN, version or cipher mismatch".into());
    }
    Ok(())
}
pub(super) async fn write_setup<W: tokio::io::AsyncWrite + Unpin>(
    stream: &mut W,
    binding: LowBinding,
    message: &LowMsg,
) -> std::io::Result<()> {
    let bytes = crate::link::control_wire::encode(&crate::link::control::LinkMsg::Low {
        binding,
        message: Box::new(message.clone()),
    })
    .map_err(std::io::Error::other)?;
    if bytes.len() > SETUP_LIMIT {
        return Err(std::io::Error::other("TCP setup too large"));
    }
    stream.write_all(&bytes).await?;
    stream.flush().await
}
pub(super) fn control_header(bytes: &[u8], maximum: usize) -> std::io::Result<oink_protocol::v1::frame::FrameHeader> {
    use oink_protocol::v1::{
        frame::FrameHeader,
        registry::{self, Transport},
    };
    let header = FrameHeader::parse(bytes, maximum)
        .map_err(std::io::Error::other)?
        .ok_or_else(|| std::io::Error::other("partial TCP header"))?;
    if !registry::message(header.type_id).is_some_and(|entry| {
        entry
            .routes
            .iter()
            .any(|r| r.kind == 1 && r.transport == Transport::Tcp)
    }) {
        return Err(std::io::Error::other("invalid TCP control channel"));
    }
    Ok(header)
}
pub(super) async fn read_control<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
    maximum: usize,
) -> std::io::Result<Vec<u8>> {
    let mut prefix = [0; 6];
    stream.read_exact(&mut prefix).await?;
    let header = control_header(&prefix, maximum)?;
    let mut bytes = vec![0; header.total_length()];
    bytes[..6].copy_from_slice(&prefix);
    stream.read_exact(&mut bytes[6..]).await?;
    Ok(bytes)
}
pub(super) async fn read_setup<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
) -> std::io::Result<(LowBinding, LowMsg)> {
    let bytes = read_control(stream, SETUP_LIMIT).await?;
    match crate::link::control_wire::decode(&bytes).map_err(std::io::Error::other)? {
        crate::link::control::LinkMsg::Low { binding, message } => Ok((binding, *message)),
        _ => Err(std::io::Error::other("not a Low setup message")),
    }
}
