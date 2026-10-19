use super::*;
use crate::record::RecordRead;
use std::sync::atomic::Ordering;

pub(super) async fn choice(choice: &RaceChoice) -> anyhow::Result<bool> {
    let mut rx = choice.subscribe();
    loop {
        if let Some(reverse) = *rx.borrow_and_update() {
            return Ok(reverse);
        }
        rx.changed().await?;
    }
}
impl Core {
    pub(super) fn start_warm(self: &Arc<Self>, pool: Arc<Pool>, auth: Authorization) {
        if !auth.current() || !pool.current() {
            return;
        }
        let generation = {
            let mut st = lock(&pool.state);
            if st.advert.is_none() || !matches!(st.warm, WarmState::Cold) {
                return;
            }
            // Both advertised listeners are unavailable, so neither physical
            // dial direction can succeed. Do not consume the initialization
            // deadline waiting for a race that cannot start (Mac/Windows).
            if st.advert.as_ref().is_some_and(|advert| advert.port.is_none()) && lock(&self.listener).is_none() {
                st.warm = WarmState::Failed("neither peer has a TCP listener".into());
                drop(st);
                let _ = self.error(&pool, "listener", "neither peer has a TCP listener");
                pool.notify.notify_waiters();
                self.status_changed();
                return;
            }
            st.warm = WarmState::Opening;
            st.warm_binding = Some(auth.grant.binding);
            st.generation
        };
        let core = self.clone();
        let task_auth = auth.clone();
        self.spawn(auth, async move {
            let started = std::time::Instant::now();
            let trace = super::super::diagnostics::Steps::start(
                Some(pool.key.peer),
                super::super::diagnostics::hex(&pool.key.connection.random),
                "direction",
            );
            let work = async {
                let purpose = loop {
                    let notify = pool.notify.notified();
                    tokio::pin!(notify);
                    notify.as_mut().enable();
                    let known = {
                        let st = lock(&pool.state);
                        if st.generation != generation {
                            return Err(NetError::Closed);
                        }
                        st.direction_known
                    };
                    if known {
                        break LowTcpPurpose::Warm;
                    }
                    if core.cfg.boot.device_id < pool.key.peer {
                        break LowTcpPurpose::Race;
                    }
                    notify.await;
                };
                let hop = core
                    .open_lease(
                        pool.clone(),
                        task_auth.clone(),
                        rand::random::<u64>().max(1),
                        Some(LowOptions {
                            send_max: 0,
                            receive_max: 0,
                            operation_max: 0,
                        }),
                        purpose,
                        Some(generation),
                        StreamKind::LowControl as u16,
                        Some(trace.0.clone()),
                    )
                    .await?;
                trace.enter("warm_end");
                core.finish_warm(hop, Some(trace.0.clone()))
                    .await
                    .map_err(|e| core.error(&pool, "warm finish", e))
            };
            let result = match tokio::time::timeout(core.cfg.dial_timeout * 4, work).await {
                Ok(result) => result,
                Err(_) => {
                    trace.end("failed", "timeout", "neither TCP direction usable");
                    Err(core.error(&pool, trace.stage(), "neither TCP direction usable"))
                }
            };
            trace.result(&result);
            {
                let mut st = lock(&pool.state);
                if st.generation != generation {
                    return;
                }
                st.warm_time = Some(started.elapsed());
                st.warm = match result {
                    Ok(()) => WarmState::Ready,
                    Err(error) => WarmState::Failed(error.to_string()),
                };
            }
            pool.notify.notify_waiters();
            core.status_changed();
        });
    }
    pub(super) async fn wait_warm(self: &Arc<Self>, pool: Arc<Pool>, auth: Authorization) -> Result<(), NetError> {
        self.ensure_listener();
        self.publish_advert(auth.grant)?;
        let (waiting, refresh) = {
            let st = lock(&pool.state);
            let counter = st.warm_waiters.clone();
            let first = counter.fetch_add(1, Ordering::AcqRel) == 0;
            let refresh = first
                && !matches!(st.warm, WarmState::Ready)
                && (matches!(st.warm, WarmState::Failed(_))
                    || (!st.direction_known && st.advert.as_ref().is_some_and(|a| a.port.is_none())));
            (Queued(counter), refresh)
        };
        if refresh {
            let slot = self.reserve_control(auth.grant).await?;
            lock(&pool.state).reset_bootstrap();
            let (reply, _) = tokio::sync::oneshot::channel();
            slot.send(ControlSend {
                grant: auth.grant,
                message: LowMsg::Refresh,
                reply,
            });
        }
        let trace = super::super::diagnostics::Steps::start(
            Some(pool.key.peer),
            super::super::diagnostics::hex(&pool.key.connection.random),
            "peer_info",
        );
        let work = async {
            loop {
                let notify = pool.notify.notified();
                tokio::pin!(notify);
                notify.as_mut().enable();
                if !auth.current() || !pool.current() {
                    return Err(NetError::Closed);
                }
                self.start_warm(pool.clone(), auth.clone());
                let (warm, stage) = {
                    let st = lock(&pool.state);
                    (
                        st.warm.clone(),
                        if st.advert.is_none() {
                            "peer_info"
                        } else if !st.direction_known {
                            "direction"
                        } else {
                            "warm_end"
                        },
                    )
                };
                if trace.stage() != stage {
                    trace.enter(stage);
                }
                match warm {
                    WarmState::Ready => return Ok(()),
                    WarmState::Failed(e) => return Err(self.error(&pool, "bootstrap", e)),
                    _ => {}
                }
                notify.await;
            }
        };
        let result = tokio::select! {biased;_=auth.cancelled()=>Err(NetError::Closed),result=tokio::time::timeout(self.cfg.dial_timeout*5,work)=>result.unwrap_or_else(|_|{trace.end("failed","timeout","warm timeout"); Err(self.error(&pool,trace.stage(),"warm timeout"))})};
        trace.result(&result);
        drop(waiting);
        result
    }
    pub(super) async fn finish_warm(
        self: &Arc<Self>,
        hop: Hop,
        diagnostic: Option<super::super::diagnostics::Steps>,
    ) -> anyhow::Result<()> {
        let owned_trace = diagnostic.is_none().then(|| {
            super::super::diagnostics::Steps::start(
                Some(hop.pool.key.peer),
                super::super::diagnostics::hex(&hop.pool.key.connection.random),
                "warm_send_end",
            )
        });
        let trace = diagnostic.unwrap_or_else(|| owned_trace.as_ref().expect("owned trace").0.clone());
        trace.operation(super::super::diagnostics::hex(&hop.request.request));
        if trace.stage() != "warm_send_end" {
            trace.enter("warm_send_end");
        }
        let result = async {
            let life = hop.life.clone();
            let mut done = life.done.subscribe();
            let (mut tx, mut rx) = hop.into_io();
            tx.finish();
            trace.enter("warm_peer_end");
            anyhow::ensure!(rx.read_record_bytes(&mut [0]).await? == 0, "DATA on warm lease");
            rx.finish_read();
            trace.enter("warm_end_ack");
            wait_closed(&mut done).await;
            anyhow::ensure!(life.gracefully_finished(), "warm aborted");
            Ok(())
        }
        .await;
        if owned_trace.is_some() || result.is_err() {
            trace.result(&result);
        }
        result
    }
    async fn select_race(&self, pool: &Pool, request: RequestId, reverse: bool) -> bool {
        let grant = {
            let st = lock(&pool.state);
            let Some(pending) = st.pending.get(&request) else {
                return false;
            };
            pending.authorization.grant
        };
        let Ok(slot) = self.reserve_control(grant).await else {
            return false;
        };
        {
            let mut st = lock(&pool.state);
            let Some(pending) = st.pending.get_mut(&request) else {
                return false;
            };
            if !pending.authorization.current() {
                return false;
            }
            let Some(choice) = &pending.race else { return false };
            if choice.borrow().is_some() {
                return false;
            }
            pending.reverse = reverse;
            choice.send_replace(Some(reverse));
            let grant = pending.authorization.grant;
            st.direction_known = true;
            st.prefer_reverse = reverse;
            let (reply, _) = tokio::sync::oneshot::channel();
            slot.send(ControlSend {
                grant,
                message: LowMsg::RaceWinner { request, reverse },
                reply,
            });
        }
        pool.notify.notify_waiters();
        true
    }
    pub(super) fn accept_race_winner(
        self: &Arc<Self>,
        pool: Arc<Pool>,
        auth: Authorization,
        request: RequestId,
        reverse: bool,
    ) {
        if pool.key.peer >= self.cfg.boot.device_id {
            return;
        }
        let mut st = lock(&pool.state);
        let Some(pending) = st.pending.get_mut(&request) else {
            return;
        };
        if pending.request.binding != auth.grant.binding || pending.outgoing {
            return;
        }
        let Some(choice) = &pending.race else { return };
        if choice.borrow().is_some() {
            return;
        }
        pending.reverse = reverse;
        choice.send_replace(Some(reverse));
        st.direction_known = true;
        st.prefer_reverse = !reverse;
        drop(st);
        pool.notify.notify_waiters();
        self.start_warm(pool, auth);
    }
    pub(super) async fn race(
        self: &Arc<Self>,
        pool: Arc<Pool>,
        auth: Authorization,
        request: &LowRequest,
        replies: &mut mpsc::Receiver<ReplyMessage>,
        selection: RaceChoice,
        trace: &super::super::diagnostics::Steps,
    ) -> Result<Hop, NetError> {
        // Retain the choice from request creation: an authenticated reverse
        // candidate may consume pending after Ack, before this future polls.
        let forward = self.dial(&pool, &auth, request, false, trace);
        tokio::pin!(forward);
        let mut failed = false;
        loop {
            tokio::select! {biased;
                message=replies.recv()=>return match message{Some(ReplyMessage::Stream(hop))=>{trace.end("failed","superseded","forward candidate superseded by reverse socket"); trace.enter("reverse_socket"); Ok(*hop)},_=>Err(self.error(&pool,"race","race cancelled"))},
                result=&mut forward,if !failed=>{
                    match result{
                        Ok((stream,socket)) => {
                            trace.enter("race_winner");
                            if self.select_race(&pool,request.request,false).await {
                                let(life,_,auth)=self.consume(&pool,request,true,false,socket)?;
                                return self.hop(pool.clone(),request,auth,stream,life,true);
                            }
                            trace.end("failed","superseded","forward candidate not selected");
                            trace.enter("reverse_socket");
                        },
                        Err(error)=>{trace.fail(&error); trace.enter("reverse_socket"); let mut st=lock(&pool.state);if st.pending.contains_key(&request.request)&&!st.direction_known{st.first_failure=Some(error.to_string());}},
                    }failed=true;
                },
                result=choice(&selection),if !failed=>{if result.unwrap_or(false){trace.end("failed","superseded","reverse direction selected"); trace.enter("reverse_socket"); failed=true;}},
            }
        }
    }
    pub(super) async fn accept_race_candidate(
        self: &Arc<Self>,
        pool: Arc<Pool>,
        request: LowRequest,
        reverse: bool,
        mut stream: io::Stream,
        socket: std::net::TcpStream,
        trace: &super::super::diagnostics::Steps,
    ) -> anyhow::Result<()> {
        let (race, authorization) = {
            let st = lock(&pool.state);
            let pending = st
                .pending
                .get(&request.request)
                .ok_or_else(|| anyhow::anyhow!("cancelled race"))?;
            anyhow::ensure!(
                pending.authorization.current() && pending.request == request && pending.outgoing == reverse,
                "race binding mismatch"
            );
            (
                pending.race.clone().ok_or_else(|| anyhow::anyhow!("not a race"))?,
                pending.authorization.clone(),
            )
        };
        let _work = authorization
            .work()
            .ok_or_else(|| anyhow::anyhow!("Low scope cancelled"))?;
        let work = async {
            trace.enter("bind_reply");
            write_setup(
                &mut io::Guarded::authorized(&mut stream, &authorization),
                request.binding,
                &LowMsg::Ready {
                    request: request.request,
                    result: Ok(()),
                },
            )
            .await?;
            trace.enter("race_winner");
            if reverse {
                if !self.select_race(&pool, request.request, true).await {
                    trace.end("failed", "superseded", "reverse race candidate not selected");
                    return Ok(());
                }
            } else if choice(&race).await? {
                trace.end("failed", "superseded", "forward race candidate not selected");
                return Ok(());
            }
            trace.enter("deliver");
            let (life, reply, auth) = self.consume(&pool, &request, reverse, reverse, socket)?;
            let hop = self.hop(pool, &request, auth, stream, life, reverse)?;
            if reverse {
                reply
                    .ok_or_else(|| anyhow::anyhow!("missing race waiter"))?
                    .send(ReplyMessage::Stream(Box::new(hop)))
                    .await
                    .map_err(|_| anyhow::anyhow!("race cancelled"))?;
            } else {
                self.deliver(hop).await?;
            }
            Ok(())
        };
        tokio::select! {biased; _ = authorization.cancelled() => Err(anyhow::anyhow!("Low scope cancelled")), result = work => result }
    }
}
