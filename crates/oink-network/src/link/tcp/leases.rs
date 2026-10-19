use super::*;

struct RequestGuard {
    core: Weak<Core>,
    pool: Arc<Pool>,
    grant: Grant,
    id: RequestId,
    complete: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.complete
            && let Some(core) = self.core.upgrade()
        {
            core.cancel_request(&self.pool, self.id);
            let _ = core.cancel_hint(self.grant, self.id);
        }
    }
}
impl Core {
    pub(super) async fn receive_authorized(self: &Arc<Self>, pool: Arc<Pool>, auth: Authorization, message: LowMsg) {
        let grant = auth.grant;
        let stage = match &message {
            LowMsg::Info { .. } => Some("peer_info"),
            LowMsg::Refresh => Some("refresh"),
            LowMsg::Open(_) => Some("open_request"),
            LowMsg::Ready { .. } => Some("open_reply"),
            LowMsg::Reverse { .. } => Some("reverse_request"),
            LowMsg::RaceWinner { .. } => Some("race_winner"),
            LowMsg::Cancel { .. } => Some("cancel"),
            _ => None,
        };
        if let Some(stage) = stage {
            let operation = match &message {
                LowMsg::Open(r) => super::super::diagnostics::hex(&r.request),
                LowMsg::Ready { request, .. }
                | LowMsg::Reverse { request }
                | LowMsg::RaceWinner { request, .. }
                | LowMsg::Cancel { request } => super::super::diagnostics::hex(request),
                _ => super::super::diagnostics::hex(&pool.key.connection.random),
            };
            tracing::info!(target: "oink_link_diagnostics", kind = "low_control", peer_id = pool.key.peer.to_hex().as_str(), operation, stage, result = "received", "Low control received");
        }
        match message {
            LowMsg::Info { paced, port, failure } => {
                let changed = lock(&pool.state).advert.as_ref().is_some_and(|a| a.port != port);
                let slot = if changed {
                    match self.reserve_control(grant).await {
                        Ok(slot) => Some(slot),
                        Err(_) => return,
                    }
                } else {
                    None
                };
                let first = {
                    let mut st = lock(&pool.state);
                    let first = st.advert.is_none();
                    if changed {
                        st.reset_bootstrap();
                    }
                    st.advert = Some(Advert { paced, port, failure });
                    first
                };
                if let Some(slot) = slot {
                    // Both peers forget the learned direction after listener changes.
                    // Reserve before resetting: full N1 must not leave local state
                    // ahead of a mandatory Refresh that has not been admitted.
                    let (reply, _) = tokio::sync::oneshot::channel();
                    slot.send(ControlSend {
                        grant,
                        message: LowMsg::Refresh,
                        reply,
                    });
                    self.status_changed();
                }
                if first {
                    // A transit hop has no local prepare_scope callback. Reply
                    // before waiting for the smaller peer's race, otherwise it
                    // cannot learn our listener/pacing policy and start racing.
                    self.ensure_listener();
                    let _ = self.publish_advert(grant);
                }
                pool.notify.notify_waiters();
                self.start_warm(pool, auth);
            }
            LowMsg::Refresh => {
                self.ensure_listener();
                lock(&pool.state).reset_bootstrap();
                self.status_changed();
                let _ = self.publish_advert(grant);
                pool.notify.notify_waiters();
                self.start_warm(pool, auth);
            }
            LowMsg::Open(request) => {
                if request.binding != grant.binding {
                    return;
                }
                self.accept_request(pool, auth, request).await;
            }
            LowMsg::Ready { request, result } => {
                let reply = lock(&pool.state)
                    .pending
                    .get(&request)
                    .filter(|p| p.request.binding == grant.binding)
                    .and_then(|p| p.reply.clone());
                if let Some(reply) = reply {
                    let _ = reply.try_send(ReplyMessage::Ack(result));
                }
            }
            LowMsg::RaceWinner { request, reverse } => self.accept_race_winner(pool, auth, request, reverse),
            LowMsg::Reverse { request } => {
                let request = {
                    let mut st = lock(&pool.state);
                    st.pending.get_mut(&request).and_then(|p| {
                        if p.outgoing || p.reverse || p.request.binding != grant.binding {
                            None
                        } else {
                            p.reverse = true;
                            Some(p.request.clone())
                        }
                    })
                };
                if let Some(request) = request {
                    let core = self.clone();
                    let task_auth = auth.clone();
                    self.spawn(auth, async move {
                        core.reverse(pool, task_auth, request).await;
                    });
                }
            }
            LowMsg::Cancel { request } => {
                let matches = {
                    let st = lock(&pool.state);
                    st.pending
                        .get(&request)
                        .is_some_and(|p| p.request.binding == grant.binding)
                        || st.active.get(&request).is_some_and(|a| a.binding == grant.binding)
                };
                if matches {
                    self.cancel_request(&pool, request);
                }
            }
            // TCP-only frames never acquire a control-channel interpretation.
            LowMsg::Bind { .. }
            | LowMsg::Record { .. }
            | LowMsg::End { .. }
            | LowMsg::EndAck { .. }
            | LowMsg::RelayUnavailable { .. } => {}
        }
    }
    fn valid_request(&self, pool: &Pool, auth: &Authorization, request: &LowRequest, outgoing: bool) -> bool {
        auth.current()
            && pool.current()
            && request.binding == auth.grant.binding
            && request.route_tag != 0
            && request.options.is_none_or(|o| o.valid())
            && !(request.extra && (!short(request.options) || request.keep_alive))
            && (request.purpose == LowTcpPurpose::Transfer
                || (!request.extra
                    && request.options
                        == Some(LowOptions {
                            send_max: 0,
                            receive_max: 0,
                            operation_max: 0,
                        })))
            && (request.purpose != LowTcpPurpose::Race
                || if outgoing {
                    self.cfg.boot.device_id < pool.key.peer
                } else {
                    pool.key.peer < self.cfg.boot.device_id
                })
            && lock(&pool.state)
                .advert
                .as_ref()
                .is_some_and(|a| request.paced == (self.cfg.pacing || a.paced))
    }
    pub(super) async fn accept_request(self: &Arc<Self>, pool: Arc<Pool>, auth: Authorization, request: LowRequest) {
        self.ensure_listener();
        if self.publish_advert(auth.grant).is_err() {
            return;
        }
        let Ok(slot) = self.reserve_control(auth.grant).await else {
            return;
        };
        if request.purpose == LowTcpPurpose::Race && pool.key.peer < self.cfg.boot.device_id {
            lock(&pool.state).incoming_race = Some(request.request);
        }
        let valid = self.valid_request(&pool, &auth, &request, false);
        let result = {
            let mut st = lock(&pool.state);
            if !valid || !auth.current() || !pool.current() {
                Err("stale Low grant/policy".into())
            } else if request.purpose == LowTcpPurpose::Race && st.incoming_race != Some(request.request) {
                Err("superseded TCP bootstrap".into())
            } else if st.pending.contains_key(&request.request)
                || st.active.get(&request.request).and_then(Active::upgrade).is_some()
            {
                Err("duplicate Low request".into())
            } else {
                if request.purpose == LowTcpPurpose::Race {
                    st.reset_bootstrap();
                    st.incoming_race = Some(request.request);
                }
                if !st.incoming_room(&request) {
                    Err("TCP pool capacity exceeded".into())
                } else if let Ok(permit) = st.incoming.clone().try_acquire_owned() {
                    st.pending.insert(
                        request.request,
                        Pending {
                            request: request.clone(),
                            authorization: auth.clone(),
                            outgoing: false,
                            reverse: false,
                            race: (request.purpose == LowTcpPurpose::Race).then(|| Arc::new(watch::channel(None).0)),
                            permit,
                            reply: None,
                        },
                    );
                    Ok(())
                } else {
                    Err("incoming Low request limit".into())
                }
            }
        };
        tracing::info!(target: "oink_link_diagnostics", kind = "low_control", peer_id = pool.key.peer.to_hex().as_str(), operation = super::super::diagnostics::hex(&request.request).as_str(), stage = "open_reply", result = if result.is_ok() { "completed" } else { "failed" }, reason = if result.is_ok() { "accepted" } else { "rejected" }, error = ?result, "Low request admission");
        let (reply, _) = tokio::sync::oneshot::channel();
        slot.send(ControlSend {
            grant: auth.grant,
            message: LowMsg::Ready {
                request: request.request,
                result: result.clone(),
            },
            reply,
        });
        if result.is_ok() {
            if request.purpose == LowTcpPurpose::Race {
                pool.notify.notify_waiters();
                let core = self.clone();
                let p = pool.clone();
                let a = auth.clone();
                let r = request.clone();
                self.spawn(auth.clone(), async move {
                    core.reverse(p, a, r).await;
                });
            }
            let core = self.clone();
            let ttl = self.cfg.dial_timeout * 2 + Duration::from_secs(1);
            self.spawn(auth, async move {
                tokio::time::sleep(ttl).await;
                if lock(&pool.state).pending.remove(&request.request).is_some() {
                    tracing::info!(target: "oink_link_diagnostics", kind = "low_control", peer_id = pool.key.peer.to_hex().as_str(), operation = super::super::diagnostics::hex(&request.request).as_str(), stage = "bind_request", result = "failed", reason = "timeout", elapsed_ms = ttl.as_secs_f64() * 1000., "Accepted Low request expired before TCP binding");
                }
                drop(core);
            });
        }
    }
    pub(super) fn cancel_request(&self, pool: &Pool, request: RequestId) {
        let mut st = lock(&pool.state);
        if st.incoming_race == Some(request) {
            st.incoming_race = None;
        }
        st.pending.remove(&request);
        if let Some(life) = st.active.remove(&request).and_then(|a| a.upgrade()) {
            life.close();
        }
        pool.notify.notify_waiters();
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn open_lease(
        self: &Arc<Self>,
        pool: Arc<Pool>,
        auth: Authorization,
        route_tag: u64,
        options: Option<LowOptions>,
        purpose: LowTcpPurpose,
        generation: Option<u64>,
        kind: u16,
        diagnostic: Option<super::super::diagnostics::Steps>,
    ) -> Result<Hop, NetError> {
        let owned_trace = diagnostic.is_none().then(|| {
            super::super::diagnostics::Steps::start(
                Some(pool.key.peer),
                super::super::diagnostics::hex(&pool.key.connection.random),
                "peer_info",
            )
        });
        let trace = diagnostic.unwrap_or_else(|| owned_trace.as_ref().expect("owned trace").0.clone());
        let result = async {
        let _work = auth.work().ok_or(NetError::Closed)?;
        let paced = self.wait_policy(&pool, &auth, &trace).await?;
        let small = purpose == LowTcpPurpose::Transfer && short(options);
        let (regular, extra_slots, queue) = {
            let st = lock(&pool.state);
            (st.outgoing.clone(), st.extra_slots.clone(), st.queued.clone())
        };
        trace.enter("pool_slot");
        let queued = Queued::new(queue);
        let acquire = async {
            if small {
                let either = async {
                    tokio::select! {biased;
                        result=regular.acquire_owned()=>result.map(|p|(p,false)),result=extra_slots.acquire_owned()=>result.map(|p|(p,true)),
                    }
                };
                tokio::time::timeout(self.cfg.dial_timeout, either)
                    .await
                    .map_err(|_| { trace.end("failed", "timeout", "small slot wait timed out"); self.error(&pool, "admission", "small slot wait timed out") })?
                    .map_err(|_| NetError::Closed)
            } else {
                regular
                    .acquire_owned()
                    .await
                    .map(|p| (p, false))
                    .map_err(|_| NetError::Closed)
            }
        };
        let (permit, extra) =
            tokio::select! {biased;_=auth.cancelled()=>return Err(NetError::Closed),result=acquire=>result?};
        drop(queued);
        self.ensure_listener();
        self.publish_advert(auth.grant)?;
        let id = rand::random();
        let (reply, mut replies) = mpsc::channel(4);
        let control_slot = self.reserve_control(auth.grant).await?;
        let (request, idle, race, accepted) = {
            let mut st = lock(&pool.state);
            if !auth.current() || !pool.current() || generation.is_some_and(|g| g != st.generation) {
                return Err(NetError::Closed);
            }
            let mut addr = auth.grant.next_hop_address;
            if let Some(port) = st.advert.as_ref().and_then(|a| a.port) {
                addr.set_port(port);
            }
            if st.direction_addr.is_some_and(|old| old != addr) {
                st.reset_bootstrap();
                return Err(NetError::Closed);
            }
            st.direction_addr = Some(addr);
            let idle = st.borrow_idle(extra);
            let keep_alive = !extra
                && !st
                    .active
                    .values()
                    .any(|a| a.outgoing && a.keep_alive && a.upgrade().is_some_and(|l| l.active()))
                && !st.pending.values().any(|p| p.outgoing && p.request.keep_alive);
            let request = LowRequest {
                request: id,
                connection: idle.as_ref().map_or(id, |i| i.request.connection),
                options,
                extra,
                keep_alive: idle.as_ref().is_some_and(|i| i.request.keep_alive) || keep_alive,
                purpose,
                paced,
                route_tag,
                binding: auth.grant.binding,
                kind,
            };
            let race = (purpose == LowTcpPurpose::Race).then(|| Arc::new(watch::channel(None).0));
            st.pending.insert(
                id,
                Pending {
                    request: request.clone(),
                    authorization: auth.clone(),
                    outgoing: true,
                    reverse: false,
                    race: race.clone(),
                    permit,
                    reply: Some(reply),
                },
            );
            // Queue under the same pool lock as registration. A delayed old
            // Open cannot be published after the replacement race transaction.
            let (reply, accepted) = tokio::sync::oneshot::channel();
            control_slot.send(ControlSend {
                grant: auth.grant,
                message: LowMsg::Open(request.clone()),
                reply,
            });
            (request, idle, race, accepted)
        };
        let mut guard = RequestGuard {
            core: Arc::downgrade(self),
            pool: pool.clone(),
            grant: auth.grant,
            id,
            complete: false,
        };
        trace.operation(super::super::diagnostics::hex(&id));
        trace.enter("open_request");
        let accepted = tokio::select! { biased;
            _ = auth.cancelled() => return Err(NetError::Closed),
            result = accepted => result,
        };
        accepted.unwrap_or(Err(NetError::Closed))?;
        let work = async {
            trace.enter("open_reply");
            match replies.recv().await {
                Some(ReplyMessage::Ack(Ok(()))) => {}
                // The selected, authenticated reverse candidate can arrive on
                // TCP before QUIC Ready. It already proves request admission.
                Some(ReplyMessage::Stream(hop)) if purpose == LowTcpPurpose::Race => return Ok(*hop),
                other => return Err(self.error(&pool, "request", reply_error(other))),
            }
            if let Some(mut idle) = idle {
                trace.enter("bind_request");
                write_setup(
                    &mut io::Guarded::authorized(&mut idle.stream, &auth),
                    request.binding,
                    &LowMsg::Bind {
                        request: request.clone(),
                        reverse: false,
                    },
                )
                .await?;
                trace.enter("bind_reply");
                match read_setup(&mut io::Guarded::authorized(&mut idle.stream, &auth)).await? {
                    (
                        binding,
                        LowMsg::Ready {
                            request: got,
                            result: Ok(()),
                        },
                    ) if binding == request.binding && got == id => {}
                    other => return Err(self.error(&pool, "reuse binding", format!("{other:?}"))),
                }
                let socket = idle
                    .authorization
                    .with_current(|| idle.life.take_socket())
                    .ok_or(NetError::Closed)?;
                let (life, _, auth) = self.consume(&pool, &request, true, false, socket)?;
                // Reuse exchanges Bind in requester order; it does not redial.
                lock(&pool.state).dialer = Some(idle.dialer);
                self.recycle(pool.clone(), request.clone(), auth.clone(), true, idle.dialer, &life);
                return self.hop(pool.clone(), &request, auth, idle.stream, life, true);
            }
            if let Some(selection) = race {
                return self
                    .race(pool.clone(), auth.clone(), &request, &mut replies, selection, &trace)
                    .await;
            }
            let direction = {
                let st = lock(&pool.state);
                st.direction_known.then_some(st.prefer_reverse)
            };
            let reverse = direction.ok_or_else(|| self.error(&pool, "direction", "bootstrap incomplete"))?;
            if !reverse {
                let (stream, socket) = self.dial(&pool, &auth, &request, false, &trace).await?;
                let (life, _, auth) = self.consume(&pool, &request, true, false, socket)?;
                return self.hop(pool.clone(), &request, auth, stream, life, true);
            }
            trace.enter("reverse_request");
            let slot = self.reserve_control(auth.grant).await?;
            {
                let mut st = lock(&pool.state);
                let pending = st.pending.get_mut(&id).ok_or(NetError::Closed)?;
                pending.reverse = true;
            }
            let (reply, accepted) = tokio::sync::oneshot::channel();
            slot.send(ControlSend {
                grant: auth.grant,
                message: LowMsg::Reverse { request: id },
                reply,
            });
            tokio::select! {biased;_=auth.cancelled()=>return Err(NetError::Closed),result=accepted=>result.unwrap_or(Err(NetError::Closed))?}
            trace.enter("reverse_socket");
            match tokio::time::timeout(self.cfg.dial_timeout, replies.recv()).await {
                Ok(Some(ReplyMessage::Stream(hop))) => Ok(*hop),
                Ok(other) => Err(self.error(&pool, "reverse", reply_error(other))),
                Err(_) => {
                    trace.end("failed", "timeout", "TCP/TLS reverse dial timed out");
                    Err(self.error(&pool, "reverse", "TCP/TLS reverse dial timed out"))
                }
            }
        };
        let result = tokio::select! {biased;
            _=auth.cancelled()=>Err(NetError::Closed),
            result=tokio::time::timeout(self.cfg.dial_timeout*2+Duration::from_millis(500),work)=>result.unwrap_or_else(|_|{trace.end("failed","timeout","Low setup timed out"); Err(self.error(&pool,trace.stage(),"Low setup timed out"))}),
        };
        guard.complete = result.is_ok();
        result
        }.await;
        if owned_trace.is_some() || result.is_err() {
            trace.result(&result);
        }
        result
    }
    async fn wait_policy(
        &self,
        pool: &Pool,
        auth: &Authorization,
        trace: &super::super::diagnostics::Steps,
    ) -> Result<bool, NetError> {
        let deadline = tokio::time::Instant::now() + self.cfg.dial_timeout;
        loop {
            let notify = pool.notify.notified();
            tokio::pin!(notify);
            notify.as_mut().enable();
            if !auth.current() || !pool.current() {
                return Err(NetError::Closed);
            }
            if let Some(advert) = lock(&pool.state).advert.clone() {
                return Ok(self.cfg.pacing || advert.paced);
            }
            tokio::select! {biased;_=auth.cancelled()=>return Err(NetError::Closed), result=tokio::time::timeout_at(deadline,notify)=>if result.is_err(){trace.end("failed","timeout","peer did not advertise TCP policy"); return Err(self.error(pool,"policy","peer did not advertise TCP policy"));}}
        }
    }
    pub(super) async fn reverse(self: Arc<Self>, pool: Arc<Pool>, auth: Authorization, request: LowRequest) {
        let race = lock(&pool.state)
            .pending
            .get(&request.request)
            .and_then(|p| p.race.clone());
        let trace = super::super::diagnostics::Steps::start(
            Some(pool.key.peer),
            super::super::diagnostics::hex(&pool.key.connection.random),
            "reverse_socket",
        );
        trace.operation(super::super::diagnostics::hex(&request.request));
        let work = async {
            let (stream, socket) = self.dial(&pool, &auth, &request, true, &trace).await?;
            trace.enter("race_winner");
            if let Some(race) = &race
                && !choice(race).await?
            {
                return Ok(());
            }
            let (life, _, auth) = self.consume(&pool, &request, false, true, socket)?;
            let hop = self.hop(pool.clone(), &request, auth, stream, life, false)?;
            self.deliver(hop).await
        };
        let loser = async {
            if let Some(race) = &race
                && !choice(race).await.unwrap_or(true)
            {
                return;
            }
            std::future::pending::<()>().await;
        };
        let result = tokio::select! {biased;_=auth.cancelled()=>return,_=loser=>return,
            result=tokio::time::timeout(self.cfg.dial_timeout,work)=>result.unwrap_or_else(|_|{trace.end("failed","timeout","TCP/TLS reverse timeout"); Err(anyhow::anyhow!("TCP/TLS reverse timeout"))}),
        };
        trace.result(&result);
        if let Err(error) = result {
            if race.is_some() {
                return;
            }
            self.cancel_request(&pool, request.request);
            let _ = self
                .queue_control(
                    auth.grant,
                    LowMsg::Ready {
                        request: request.request,
                        result: Err(error.to_string()),
                    },
                )
                .await;
        }
    }
}
fn reply_error(reply: Option<ReplyMessage>) -> String {
    match reply {
        Some(ReplyMessage::Ack(Err(e))) => e,
        Some(ReplyMessage::Ack(Ok(()))) => "duplicate acknowledgement".into(),
        Some(ReplyMessage::Stream(_)) => "unexpected stream".into(),
        None => "request cancelled".into(),
    }
}
