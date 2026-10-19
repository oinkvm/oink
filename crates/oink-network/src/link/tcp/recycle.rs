use super::*;
struct Recycled {
    stream: io::Stream,
    socket: std::net::TcpStream,
    generation: u64,
}
impl Core {
    pub(super) fn recycle(
        self: &Arc<Self>,
        pool: Arc<Pool>,
        request: LowRequest,
        auth: Authorization,
        outgoing: bool,
        dialer: DeviceId,
        life: &Arc<io::Life>,
    ) {
        let core = Arc::downgrade(self);
        let generation = life.generation;
        life.recycle(Box::new(move |stream, socket| {
            if let Some(core) = core.upgrade() {
                core.park(
                    pool,
                    request,
                    auth,
                    outgoing,
                    dialer,
                    Recycled {
                        stream,
                        socket,
                        generation,
                    },
                );
            }
        }));
    }
    fn park(
        self: &Arc<Self>,
        pool: Arc<Pool>,
        request: LowRequest,
        auth: Authorization,
        outgoing: bool,
        dialer: DeviceId,
        recycled: Recycled,
    ) {
        let Recycled {
            stream,
            socket,
            generation,
        } = recycled;
        let life = {
            let mut st = lock(&pool.state);
            if !pool.current() || !auth.current() || st.generation != generation {
                return;
            }
            let life = io::Life::new(
                socket,
                self.runtime.clone(),
                self.cfg.dial_timeout,
                &request,
                &auth,
                None,
                generation,
            );
            if auth.bind(life.clone()).is_err() {
                return;
            }
            let weak = Arc::downgrade(self);
            let grant = auth.grant;
            let id = request.request;
            life.cancel_via(move || {
                if let Some(core) = weak.upgrade() {
                    let _ = core.cancel_hint(grant, id);
                }
            });
            st.active.insert(
                request.request,
                Active {
                    life: Arc::downgrade(&life),
                    binding: request.binding,
                    purpose: request.purpose,
                    connection: request.connection,
                    outgoing,
                    extra: request.extra,
                    idle: true,
                    keep_alive: request.keep_alive,
                },
            );
            life
        };
        if outgoing {
            lock(&pool.state).idle.push(Idle {
                stream,
                life: life.clone(),
                request: request.clone(),
                dialer,
                authorization: auth.clone(),
            });
            pool.notify.notify_waiters();
            let weak = Arc::downgrade(&life);
            let mut done = life.done.subscribe();
            self.spawn(auth,async move{
                tokio::select!{_=wait_closed(&mut done)=>{},_=tokio::time::sleep(IDLE),if !request.keep_alive=>{if let Some(life)=weak.upgrade(){life.close();}}}
                lock(&pool.state).idle.retain(|i|i.life.active());
            });
        } else {
            let core = self.clone();
            let task_auth = auth.clone();
            self.spawn(auth, async move {
                core.accept_reuse(pool, request, task_auth, dialer, stream, life).await;
            });
        }
    }
    async fn accept_reuse(
        self: Arc<Self>,
        pool: Arc<Pool>,
        previous: LowRequest,
        auth: Authorization,
        dialer: DeviceId,
        mut stream: io::Stream,
        idle: Arc<io::Life>,
    ) {
        let mut done = idle.done.subscribe();
        let read = async {
            if previous.keep_alive {
                read_setup(&mut io::Guarded::new(&mut stream, &idle)).await.ok()
            } else {
                tokio::time::timeout(IDLE, read_setup(&mut io::Guarded::new(&mut stream, &idle)))
                    .await
                    .ok()
                    .and_then(Result::ok)
            }
        };
        let binding = tokio::select! {biased;_=wait_closed(&mut done)=>None,result=read=>result};
        let Some((
            binding,
            LowMsg::Bind {
                request,
                reverse: false,
            },
        )) = binding
        else {
            idle.close();
            return;
        };
        if request.binding != binding
            || request.connection != previous.connection
            || request.extra != previous.extra
            || request.request == previous.request
            || !auth.current()
        {
            idle.close();
            return;
        }
        let Some(socket) = auth.with_current(|| idle.take_socket()) else {
            return;
        };
        let Ok((life, _, next)) = self.consume(&pool, &request, false, false, socket) else {
            return;
        };
        lock(&pool.state).dialer = Some(dialer);
        self.recycle(pool.clone(), request.clone(), next.clone(), false, dialer, &life);
        // Ownership has crossed into the newly authorized lease. Its work must
        // no longer be cancelled by the old idle scope's task wrapper.
        let core = self.clone();
        self.spawn(next.clone(), async move {
            let work = async {
                write_setup(
                    &mut io::Guarded::new(&mut stream, &life),
                    binding,
                    &LowMsg::Ready {
                        request: request.request,
                        result: Ok(()),
                    },
                )
                .await?;
                let hop = core.hop(pool, &request, next, stream, life.clone(), false)?;
                core.deliver(hop).await
            };
            if !matches!(tokio::time::timeout(core.cfg.dial_timeout, work).await, Ok(Ok(()))) {
                life.close();
            }
        });
    }
}
