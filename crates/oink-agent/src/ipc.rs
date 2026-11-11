//! Local loopback API used by the UI through [`crate::remote::RemoteApi`] and
//! by command-line clients through [`Client`].
//!
//! - Windows uses user-SID-only named pipes; Unix uses a private data-directory
//!   socket. `ipc.json` publishes the native address, lock owner and one-use secret.
//!   Legacy loopback endpoint documents remain readable during upgrades.
//! - 格式：一行一个 JSON。请求 `{"token": "...", ...}`，再带下面其中一项：
//!   - `"call": Request`（见 [`crate::api::Request`]）→ `{"ok": Reply}` 或 `{"err": ApiError}`；
//!   - `"hello": true` → [`ServiceInfo`]（版本、进程号）；
//!   - `"stop": true` → 应答后服务进程优雅退出；
//!   - `"subscribe": true` → 先应答 [`ServiceInfo`]，然后这条连接只推送事件：`{"event": ApiEvent}`；服务进程
//!     即将退出时推送 `{"closing": "stopping"}`；界面处理不过来（积压超过 [`EVENT_BACKLOG`] 条）时推送
//!     `{"closing": "lagged"}` 后断开，界面重新订阅并重新拉取全部状态。
//! - 同一连接上可以依次发多个请求（界面复用连接）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use tokio::sync::watch;

use crate::api::{self, ApiError, ApiEvent, Request};
use crate::node::Node;

mod frames;
pub(crate) use frames::Frames;
mod publication;
pub(crate) mod transport;
pub(crate) use transport::Listener;
pub use transport::Stream as LocalStream;
mod ui;
pub use ui::UiControl;

pub const IPC_FILE: &str = "ipc.json";

/// A missing owner cannot mean successful stop while its endpoint is live.
/// Do not send commands to an unowned endpoint: its recorded port may have been
/// reused. Only an absent file or a missing/refused native endpoint proves it idle;
/// unreadable endpoints and other socket errors leave shutdown unverified.
pub(crate) fn verify_no_listener(data_dir: &Path, file: &str) -> anyhow::Result<()> {
    let text = match std::fs::read(data_dir.join(file)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => anyhow::bail!("cannot verify Oink has stopped: cannot read {file}: {error}"),
    };
    let endpoint: Endpoint = serde_json::from_slice(&text)
        .map_err(|error| anyhow::anyhow!("cannot verify Oink has stopped: invalid {file}: {error}"))?;
    match endpoint.address.probe() {
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(())
        }
        Err(error) => anyhow::bail!("cannot verify Oink has stopped: cannot query {file}: {error}"),
        Ok(_) => anyhow::bail!(
            "Oink process ownership unavailable: {file} is still reachable; cannot verify Oink has stopped"
        ),
    }
}

/// 一条事件订阅连接最多积压多少条事件（界面卡住时不无限占用内存）。
pub const EVENT_BACKLOG: usize = 4096;

/// `closing` 的取值：服务进程正在按请求退出。
pub const CLOSING_STOPPING: &str = "stopping";
/// `closing` 的取值：这条订阅积压太多，服务进程断开它（服务仍在运行）。
pub const CLOSING_LAGGED: &str = "lagged";
pub const CLOSING_BUILD_MISMATCH: &str = "build_mismatch";

/// Local IPC is private to one bundle build, including its embedded frontend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub version: String,
    pub build: String,
    pub source: String,
}

impl BuildIdentity {
    pub fn current() -> Self {
        Self {
            version: env!("OINK_RELEASE_VERSION").into(),
            build: env!("OINK_RELEASE_BUILD").into(),
            source: env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256").into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    #[serde(flatten)]
    pub address: transport::Address,
    pub token: String,
}

impl Endpoint {
    /// 读取数据目录里的 `ipc.json`。
    pub(crate) fn read(data_dir: &Path) -> anyhow::Result<Self> {
        Self::read_file(data_dir, IPC_FILE)
    }

    fn read_file(data_dir: &Path, name: &str) -> anyhow::Result<Self> {
        let path = data_dir.join(name);
        let text = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::Error::new(error).context(format!("服务进程没有在运行（读不到 {}）", path.display()))
        })?;
        Ok(serde_json::from_str(&text)?)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<[u8; 16]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<Request>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stop: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hello: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub subscribe: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ui_ready: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_ui: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_views: Option<api::LiveViews>,
}

/// `hello` / `subscribe` 的应答：正在运行的服务进程。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// 服务进程的版本（`OINK_RELEASE_VERSION`）。
    pub version: String,
    pub pid: u32,
    pub build: BuildIdentity,
}

impl ServiceInfo {
    pub fn current() -> Self {
        Self {
            version: env!("OINK_RELEASE_VERSION").to_owned(),
            pid: std::process::id(),
            build: BuildIdentity::current(),
        }
    }
}

/// 运行中的 IPC 服务。
#[derive(Debug)]
pub struct Server {
    task: tokio::task::JoinHandle<()>,
    publication: publication::Publication,
    file: PathBuf,
    _listener: Arc<Listener>,
    closing: watch::Sender<bool>,
    conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Server {
    /// Bind a native, user-private endpoint independently of device ports.
    pub async fn start(node: Node) -> anyhow::Result<Self> {
        let listener = Listener::bind(node.data_dir(), "agent")?;
        Self::start_listening(node, listener).await
    }

    pub(crate) async fn start_listening(node: Node, listener: Listener) -> anyhow::Result<Self> {
        let listener = Arc::new(listener);
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("IPC token entropy: {e}"))?;
        let token: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
        let file = node.data_dir().join(IPC_FILE);
        let document = serde_json::to_string(&Endpoint {
            address: listener.address(),
            token: token.clone(),
        })?;
        let publish_path = file.clone();
        let publication = publication::Publication::start(move || write_private(&publish_path, &document))?;
        tracing::info!(address = ?listener.address(), "native local IPC listening");
        let (closing, _) = watch::channel(false);
        let conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let task = {
            let closing = closing.clone();
            let conns = conns.clone();
            let listener = listener.clone();
            tokio::spawn(async move {
                loop {
                    let stream = match listener.accept().await {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                            tracing::warn!(%error, "native IPC rejected peer credentials");
                            continue;
                        }
                        Err(error) => {
                            tracing::error!(%error, os_error = ?error.raw_os_error(), "native IPC listener failed");
                            break;
                        }
                    };
                    let node = node.clone();
                    let token = token.clone();
                    let closing = closing.subscribe();
                    let handle = tokio::spawn(async move {
                        if let Err(e) = serve(stream, node, token, closing).await {
                            tracing::debug!(%e, "IPC connection ended");
                        }
                    });
                    let mut conns = conns.lock().unwrap_or_else(|e| e.into_inner());
                    conns.retain(|h| !h.is_finished());
                    conns.push(handle);
                }
            })
        };
        Ok(Self {
            task,
            publication,
            file,
            _listener: listener,
            closing,
            conns,
        })
    }

    #[cfg(test)]
    pub(crate) fn address(&self) -> transport::Address {
        self._listener.address()
    }

    /// 优雅关闭：告诉事件订阅者服务即将退出（`{"closing": "stopping"}`），等各连接结束（最多 1 秒），
    /// 然后停止监听、删除 `ipc.json`。
    pub async fn close(self) {
        self.disconnect_clients().await;
        self._listener.release();
        let _ = std::fs::remove_file(&self.file);
    }

    /// Announce/close subscriptions first; keep the endpoint file until host cleanup ends.
    pub(crate) async fn disconnect_clients(&self) {
        self.task.abort();
        self.closing.send_replace(true);
        self.publication.stop_async().await;
        let conns: Vec<_> = std::mem::take(&mut *self.conns.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        for mut h in conns {
            if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
                h.abort();
            }
        }
    }

    /// 立即停止（不等订阅者）。
    pub fn stop(self) {
        self.task.abort();
        self._listener.release();
        self.publication.stop();
        self.closing.send_replace(true);
        for h in self.conns.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            h.abort();
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Publish an owner-only, Medium-minimum file even from an elevated UI.
    // Native IPC also admits this user across UIAccess/Medium/High integrity.
    oink_foundation::storage::atomic_write_quiet(path, text.as_bytes())
}

async fn write_line(w: &mut tokio::io::WriteHalf<transport::AsyncStream>, v: &serde_json::Value) -> anyhow::Result<()> {
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    Ok(())
}

async fn serve(
    stream: transport::AsyncStream,
    node: Node,
    token: String,
    closing: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let connection_client = api::request_context::new_client();
    let peer = transport::peer(&stream)?;
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = Frames::new(r);
    let mut next = lines.first_line().await?;
    while let Some(line) = next.take() {
        // A client that timed out or exited closes its end. What it asked for
        // must not take effect later, when nobody is waiting for it any more:
        // on 2026-11-10 a backlog of 31 abandoned requests ran 15 minutes late
        // and changed the audio route. A close just after this check is the
        // ordinary race of a client leaving mid-request.
        if peer.closed() {
            tracing::debug!("local IPC request dropped: the client already closed its connection");
            return Ok(());
        }
        let mut line = zeroize::Zeroizing::new(line);
        let request = serde_json::from_str::<Envelope>(&line);
        zeroize::Zeroize::zeroize(&mut *line);
        let mut stop = false;
        let reply = match request {
            Err(_) => {
                serde_json::json!({ "err": ApiError::InvalidArgument { message: "invalid local API request".into() } })
            }
            Ok(env) if env.token != token => {
                serde_json::json!({ "err": ApiError::InvalidArgument { message: "令牌不对".into() } })
            }
            Ok(env) if env.stop => {
                stop = true;
                serde_json::json!({ "ok": null })
            }
            Ok(env) if env.build.as_ref() != Some(&BuildIdentity::current()) => {
                let backend = BuildIdentity::current();
                tracing::warn!(client = ?env.build, ?backend, "local IPC build mismatch; refusing client; exit and reopen Oink from the current bundle");
                write_line(
                    &mut w,
                    &serde_json::json!({
                        "closing": CLOSING_BUILD_MISMATCH,
                        "backend": backend,
                        "err": ApiError::Unavailable { message: "local IPC build mismatch".into() }
                    }),
                )
                .await?;
                finish_response(&mut lines).await;
                return Ok(());
            }
            Ok(env) if env.ui_ready => {
                crate::backend::ui_ready(&node);
                serde_json::json!({ "ok": null })
            }
            Ok(env) if env.hello => serde_json::json!({ "ok": ServiceInfo::current() }),
            Ok(env) if env.subscribe => {
                return stream_events(lines, w, node, closing, env.live_views.unwrap_or(api::LiveViews::ALL)).await;
            }
            Ok(env) => {
                let node = node.clone();
                tokio::task::spawn_blocking(move || match env.call {
                    Some(req) => {
                        match api::request_context::with_client(env.client_id.unwrap_or(connection_client), || {
                            api::dispatch(&node, req)
                        }) {
                            Ok(r) => serde_json::json!({ "ok": r }),
                            Err(e) => serde_json::json!({ "err": e }),
                        }
                    }
                    None => serde_json::json!({ "err": ApiError::InvalidArgument { message: "缺少 call".into() } }),
                })
                .await?
            }
        };
        write_line(&mut w, &reply).await?;
        if stop {
            tracing::info!("stop requested over local IPC");
            node.request_stop();
            finish_response(&mut lines).await;
            return Ok(());
        }
        next = lines.next_line().await?;
    }
    Ok(())
}

/// DisconnectNamedPipe discards unread server data. Wait for the client's EOF
/// after its final response instead of blocking a runtime worker in
/// FlushFileBuffers (which a stalled client could keep blocked indefinitely).
/// This is bounded teardown, never a request/port retry.
async fn finish_response(lines: &mut Frames<tokio::io::ReadHalf<transport::AsyncStream>>) {
    #[cfg(windows)]
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        // The UI can still have view updates queued after subscribing. A
        // complete control frame is not EOF: disconnecting then discards the
        // unread final response on Windows. Keep the same total drain budget.
        while let Ok(Some(_)) = lines.next_line().await {}
    })
    .await;
    #[cfg(not(windows))]
    let _ = lines;
}

/// 事件订阅：把服务的事件逐行推给这条连接，直到对方断开、服务退出或积压过多。
async fn stream_events(
    mut lines: Frames<tokio::io::ReadHalf<transport::AsyncStream>>,
    mut w: tokio::io::WriteHalf<transport::AsyncStream>,
    node: Node,
    mut closing: watch::Receiver<bool>,
    views: api::LiveViews,
) -> anyhow::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiEvent>(EVENT_BACKLOG);
    let lagged = Arc::new(tokio::sync::Notify::new());
    let sink = {
        let lagged = lagged.clone();
        Arc::new(move |ev: &ApiEvent| {
            if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) = tx.try_send(ev.clone()) {
                lagged.notify_one();
            }
        })
    };
    // 先订阅再应答：界面收到应答后才重新拉取状态，之后的变化一定会作为事件到达。
    let id = node.subscribe_views(sink, views);
    tracing::info!("UI subscribed to service events over local IPC");
    let result = async {
        write_line(&mut w, &serde_json::json!({ "ok": ServiceInfo::current() })).await?;
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let Some(ev) = ev else { return Ok(()) };
                    write_line(&mut w, &serde_json::json!({ "event": ev })).await?;
                }
                () = lagged.notified() => {
                    tracing::warn!(backlog = EVENT_BACKLOG, "IPC event subscriber is not keeping up; disconnecting it");
                    write_line(&mut w, &serde_json::json!({ "closing": CLOSING_LAGGED })).await?;
                    return Ok(());
                }
                requested = async { closing.wait_for(|c| *c).await.map(|_| ()) } => {
                    // Losing the server task/sender is a broken connection, not
                    // an explicit shutdown request. Only a true value announces stop.
                    if requested.is_ok() {
                        write_line(&mut w, &serde_json::json!({ "closing": CLOSING_STOPPING })).await?;
                    }
                    return Ok(());
                }
                () = node.wait_for_stop() => {
                    write_line(&mut w, &serde_json::json!({ "closing": CLOSING_STOPPING })).await?;
                    return Ok(());
                }
                // 订阅连接接收本客户端的可见视图需求；EOF 表示对方断开。
                line = lines.next_line() => {
                    match line? {
                        None => return Ok(()),
                        Some(line) => {
                            // This connection was authenticated at subscription.
                            if let Ok(views) = serde_json::from_str::<api::LiveViews>(&line) {
                                node.set_live_views(id, views);
                            }
                        }
                    }
                }
            }
        }
    }
    .await;
    node.unsubscribe(id);
    tracing::info!("UI event subscription ended");
    finish_response(&mut lines).await;
    let _ = w.shutdown().await;
    result
}

/// Same completed-result contract for CLI and RemoteApi/UI saves. The server
/// single-flights saves; a transport deadline cannot imply that it cancelled one.
pub(crate) const DIAGNOSTICS_SAVE_TIMEOUT: Option<Duration> = None;

/// Package hydration can legitimately outlast an ordinary IPC call. Its native
/// reader owns completion/failure; a transport deadline would report failure
/// while the backend is still staging. Both UI pooled calls and CLI use this.
pub(crate) fn response_timeout(request: Option<&Request>, ordinary: Duration) -> Option<Duration> {
    match request {
        Some(Request::ExportDiagnostics { .. }) => DIAGNOSTICS_SAVE_TIMEOUT,
        Some(Request::Update {
            command: api::UpdateCommand::OpenLocal { .. },
        }) => None,
        _ => Some(ordinary),
    }
}
pub(crate) fn diagnostics_read_error(error: std::io::Error) -> String {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        "诊断保存等待超时，结果尚未确认；服务端可能仍在保存，请用 diagnostics list 或界面列表查询，重复保存会合并正在执行的请求。".into()
    } else {
        format!("诊断保存连接中断，结果尚未确认：{error}；请用 diagnostics list 或界面列表查询。")
    }
}

/// 命令行一侧：调用正在运行的服务进程。
#[derive(Debug)]
pub struct Client {
    endpoint: Endpoint,
    client_id: [u8; 16],
}

impl Client {
    /// 读取数据目录里的 `ipc.json`。
    pub fn connect(data_dir: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            endpoint: Endpoint::read(data_dir)?,
            client_id: api::request_context::new_client(),
        })
    }

    fn roundtrip(&self, env: &Envelope) -> anyhow::Result<serde_json::Value> {
        self.roundtrip_with_timeout(env, Some(Duration::from_secs(30)))
    }

    // Keep socket setup separate so tests can inspect the kernel's configured deadlines
    // without comparing elapsed wall time (which also includes scheduler delay).
    fn bounded_stream(&self, timeout: Duration) -> anyhow::Result<LocalStream> {
        let s = self.endpoint.address.connect(timeout)?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        Ok(s)
    }

    fn roundtrip_with_timeout(&self, env: &Envelope, timeout: Option<Duration>) -> anyhow::Result<serde_json::Value> {
        use std::io::{BufRead, Write};
        let mut s = self.bounded_stream(timeout.unwrap_or(Duration::from_secs(30)))?;
        s.set_read_timeout(timeout)?;
        let mut line = zeroize::Zeroizing::new(serde_json::to_string(env)?);
        line.push('\n');
        s.write_all(line.as_bytes())?;
        zeroize::Zeroize::zeroize(&mut *line);
        let mut r = std::io::BufReader::new(s);
        let mut out = String::new();
        r.read_line(&mut out).map_err(|error| {
            if matches!(env.call, Some(Request::ExportDiagnostics { .. })) {
                anyhow::anyhow!(diagnostics_read_error(error))
            } else {
                error.into()
            }
        })?;
        let v: serde_json::Value = serde_json::from_str(&out)?;
        if let Some(e) = v.get("err") {
            let e: ApiError = serde_json::from_value(e.clone())?;
            return Err(anyhow::anyhow!("{e}"));
        }
        Ok(v.get("ok").cloned().unwrap_or(serde_json::Value::Null))
    }

    fn envelope(&self) -> Envelope {
        Envelope {
            token: self.endpoint.token.clone(),
            build: Some(BuildIdentity::current()),
            client_id: Some(self.client_id),
            ..Envelope::default()
        }
    }

    /// 调用一个 API（`Request`），返回应答的 JSON。
    pub fn call(&self, req: Request) -> anyhow::Result<serde_json::Value> {
        let timeout = response_timeout(Some(&req), Duration::from_secs(30));
        self.roundtrip_with_timeout(
            &Envelope {
                call: Some(req),
                ..self.envelope()
            },
            timeout,
        )
    }

    /// 请求服务进程优雅停止；应答表示请求已收到。
    pub fn stop(&self) -> anyhow::Result<serde_json::Value> {
        self.stop_with_timeout(Duration::from_secs(30))
    }

    pub(crate) fn stop_with_timeout(&self, timeout: Duration) -> anyhow::Result<serde_json::Value> {
        self.roundtrip_with_timeout(
            &Envelope {
                stop: true,
                ..self.envelope()
            },
            Some(timeout),
        )
    }

    /// Announce that the native UI has installed its event subscription.
    pub fn ui_ready(&self) -> anyhow::Result<()> {
        self.roundtrip(&Envelope {
            ui_ready: true,
            ..self.envelope()
        })?;
        Ok(())
    }

    /// 正在运行的服务进程的版本与进程号。
    pub fn hello(&self) -> anyhow::Result<ServiceInfo> {
        self.hello_with_timeout(Duration::from_secs(30))
    }

    /// Authenticated handshake with bounded connect, read, and write timeouts.
    pub fn hello_with_timeout(&self, timeout: Duration) -> anyhow::Result<ServiceInfo> {
        Ok(serde_json::from_value(self.roundtrip_with_timeout(
            &Envelope {
                hello: true,
                ..self.envelope()
            },
            Some(timeout),
        )?)?)
    }
}

/// Open the authenticated-owner transport without sending a request. Used by
/// local CLI subscriptions and protocol integration fixtures.
#[doc(hidden)]
pub fn open_stream(data_dir: &Path, timeout: Duration) -> anyhow::Result<LocalStream> {
    Endpoint::read(data_dir)?.address.connect(timeout).map_err(Into::into)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn absent_stale_live_and_unreadable_endpoints_have_distinct_stop_results() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_ok());
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = Endpoint {
            address: transport::Address::Legacy {
                port: listener.local_addr().unwrap().port(),
            },
            token: "private-test-token".into(),
        };
        write_private(&dir.path().join(IPC_FILE), &serde_json::to_string(&endpoint).unwrap()).unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_err());
        drop(listener);
        let stale = verify_no_listener(dir.path(), IPC_FILE);
        assert!(stale.is_ok(), "stale endpoint: {stale:?}");
        std::fs::write(dir.path().join(IPC_FILE), b"invalid").unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_err());
        std::fs::remove_file(dir.path().join(IPC_FILE)).unwrap();
        std::fs::create_dir(dir.path().join(IPC_FILE)).unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_err());
    }

    #[test]
    fn live_views_follow_the_authenticated_subscription_and_disconnect() {
        crate::test_watchdog::run(
            module_path!(),
            "live_views_follow_the_authenticated_subscription_and_disconnect",
            Some(2),
            live_views_follow_the_authenticated_subscription_and_disconnect_scenario(),
        );
    }

    async fn live_views_follow_the_authenticated_subscription_and_disconnect_scenario() {
        use crate::api::{AgentApi, LiveViews};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let node = Node::start(crate::node::NodeOptions {
            net: oink_test_support::network_config(dir.path().into(), "live-views", 0),
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        })
        .await
        .unwrap();
        let server = Server::start(node.clone()).await.unwrap();
        let remote = crate::remote::RemoteApi::new(dir.path());
        // This test node has fake native services: exercise the real IPC/client
        // dispatch and unsupported guard without invoking any operating-system restart.
        assert!(matches!(
            node.restart_system(),
            Err(crate::api::ApiError::Unsupported { .. })
        ));
        assert!(matches!(
            remote.restart_system(),
            Err(crate::api::ApiError::Unsupported { .. })
        ));
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        remote.subscribe(Arc::new(move |event| {
            if matches!(event, ApiEvent::Stats(_)) {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        }));
        let follower = remote.clone();
        let (ready, connected) = tokio::sync::oneshot::channel();
        let follow = std::thread::spawn(move || {
            follower.follow_events(|_| {
                let _ = ready.send(());
            })
        });
        connected.await.unwrap();
        assert!(!node.has_stats_subscribers());
        node.emit(ApiEvent::Stats(vec![]));
        remote
            .set_live_views(LiveViews {
                stats: true,
                ..Default::default()
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while count.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(node.has_stats_subscribers());
        remote.set_live_views(LiveViews::default()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while node.has_stats_subscribers() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        crate::test_watchdog::phase("cancel and join idle IPC subscription");
        remote.stop_following();
        assert_eq!(follow.join().unwrap(), crate::remote::EventsEnd::Cancelled);
        // Force cancellation after authentication but before the first event
        // read, without a sleep that masks the shutdown/read boundary.
        let immediate = crate::remote::RemoteApi::new(dir.path());
        let reader = immediate.clone();
        let early = std::thread::spawn(move || reader.follow_events(|_| reader.stop_following()));
        crate::test_watchdog::phase("cancel before first IPC event read");
        assert_eq!(early.join().unwrap(), crate::remote::EventsEnd::Cancelled);
        server.stop();
        node.shutdown().await;
    }

    #[test]
    fn request_from_a_client_that_already_left_is_not_executed() {
        crate::test_watchdog::run(
            module_path!(),
            "request_from_a_client_that_already_left_is_not_executed",
            Some(2),
            request_from_a_client_that_already_left_is_not_executed_scenario(),
        );
    }

    async fn request_from_a_client_that_already_left_is_not_executed_scenario() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let node = Node::start(crate::node::NodeOptions {
            net: oink_test_support::network_config(dir.path().into(), "abandoned-request", 0),
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        })
        .await
        .unwrap();
        let listener = Listener::bind(dir.path(), "abandoned-request").unwrap();
        let (closing, _) = watch::channel(false);
        let mut stop = serde_json::to_string(&Envelope {
            token: "token".into(),
            stop: true,
            ..Envelope::default()
        })
        .unwrap();
        stop.push('\n');
        // The client asked, gave up and left before the backend got to it.
        let mut client = listener.address().connect(Duration::from_secs(1)).unwrap();
        client.write_all(stop.as_bytes()).unwrap();
        let (stream, ()) = listener.accept().await.unwrap();
        drop(client);
        serve(stream, node.clone(), "token".into(), closing.subscribe())
            .await
            .unwrap();
        assert!(
            !*node.stop_receiver().borrow(),
            "a request whose client is gone must not take effect"
        );
        // The same request from a client that is still waiting is honoured.
        let mut client = listener.address().connect(Duration::from_secs(1)).unwrap();
        client.write_all(stop.as_bytes()).unwrap();
        let (stream, ()) = listener.accept().await.unwrap();
        serve(stream, node.clone(), "token".into(), closing.subscribe())
            .await
            .unwrap();
        assert!(*node.stop_receiver().borrow());
        drop(client);
        node.shutdown().await;
    }

    #[test]
    fn package_hydration_has_no_response_deadline_and_next_call_restores_its_bound() {
        let package = Request::Update {
            command: api::UpdateCommand::OpenLocal {
                path: "/received/Oink.dmg".into(),
            },
        };
        let ordinary = Request::Update {
            command: api::UpdateCommand::Check,
        };
        for duration in [Duration::from_secs(30), Duration::from_secs(120)] {
            assert_eq!(response_timeout(Some(&package), duration), None);
            assert_eq!(response_timeout(Some(&ordinary), duration), Some(duration));
            assert_eq!(response_timeout(None, duration), Some(duration));
        }
    }
    #[test]
    fn diagnostic_save_socket_timeout_explains_unconfirmed_completion() {
        let listener = (47470..=47489)
            .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
            .unwrap();
        let client = Client {
            client_id: [1; 16],
            endpoint: Endpoint {
                address: transport::Address::Legacy {
                    port: listener.local_addr().unwrap().port(),
                },
                token: "save-timeout".into(),
            },
        };
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_connection, _) = listener.accept().unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let error = client
            .roundtrip_with_timeout(
                &Envelope {
                    call: Some(Request::ExportDiagnostics { last_minutes: None }),
                    ..client.envelope()
                },
                Some(Duration::from_millis(10)),
            )
            .unwrap_err();
        release.send(()).unwrap();
        server.join().unwrap();
        let message = error.to_string();
        assert!(message.contains("结果尚未确认"), "{message}");
        assert!(message.contains("diagnostics list"), "{message}");
        assert!(!message.contains("os error"), "{message}");
    }

    #[test]
    fn bounded_stream_has_read_and_write_deadlines() {
        let listener = (47470..=47489)
            .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
            .expect("no free Oink test TCP port");
        let client = Client {
            client_id: [1; 16],
            endpoint: Endpoint {
                address: transport::Address::Legacy {
                    port: listener.local_addr().unwrap().port(),
                },
                token: "deadline-test".into(),
            },
        };
        // Inspect configured socket options, not how quickly this process gets scheduled.
        // This also fails immediately if a deadline is removed, rather than hanging a
        // test waiting for an unanswered hello. The CLI fixture separately verifies EOF.
        let timeout = Duration::from_millis(500);
        let stream = client.bounded_stream(timeout).unwrap();
        assert!(
            matches!(stream, LocalStream::Legacy(ref s) if s.read_timeout().unwrap() == Some(timeout) && s.write_timeout().unwrap() == Some(timeout))
        );
    }
}

#[cfg(test)]
mod product_boundary {
    use super::Envelope;

    #[test]
    fn development_envelope_keys_are_rejected() {
        for hook in [
            serde_json::json!({"debug": true}),
            serde_json::json!({"synth": {"kind": "idle_activity"}}),
        ] {
            for operation in ["hello", "stop", "subscribe"] {
                let mut request = hook.clone();
                request["token"] = "test-token".into();
                request[operation] = true.into();
                let decoded = serde_json::from_value::<Envelope>(request);
                assert!(decoded.is_err(), "{operation}: {hook}");
            }
        }
        // Unknown keys fail before dispatch, even beside a valid product operation.
        assert!(
            serde_json::from_value::<Envelope>(serde_json::json!({
                "token": "test-token", "hello": true, "unknown_hook": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Envelope>(serde_json::json!({
                "token": "test-token", "hello": true
            }))
            .is_ok()
        );
    }
}

#[cfg(all(test, windows))]
mod final_response_tests {
    use super::*;
    use std::{
        future::Future,
        io::BufRead,
        task::{Context, Poll, Waker},
    };

    #[tokio::test]
    async fn final_response_waits_past_buffered_views_and_preserves_closing() {
        let dir = tempfile::tempdir().unwrap();
        let listener = Listener::bind(dir.path(), "final-response").unwrap();
        let mut client = listener.address().connect(Duration::from_secs(1)).unwrap();
        // Both frames are already queued before the server reads the first.
        // The second models the UI's view update after subscribing.
        std::io::Write::write_all(&mut client, b"request\npending-view\n").unwrap();
        let (stream, ()) = listener.accept().await.unwrap();
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = Frames::new(r);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("request"));
        write_line(&mut w, &serde_json::json!({"closing": CLOSING_STOPPING}))
            .await
            .unwrap();
        let mut drain = Box::pin(finish_response(&mut lines));
        assert!(
            matches!(
                drain.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ),
            "a buffered view frame is not client EOF; disconnecting now discards the closing response"
        );
        let mut closing = String::new();
        std::io::BufReader::new(&client).read_line(&mut closing).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&closing).unwrap()["closing"],
            CLOSING_STOPPING
        );
        drop(client);
        drain.await;
    }
}
