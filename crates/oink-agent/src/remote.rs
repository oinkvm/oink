//! UI-side implementation of [`AgentApi`] over the local service IPC.
//!
//! - 请求：每次调用从连接池取一条连接（没有就按 `ipc.json` 新建），一行请求、一行应答。连接断开时返回
//!   [`ApiError::Unavailable`]，不重试（请求可能已经执行）。
//! - 事件：[`RemoteApi::follow_events`] 在调用线程上保持一条订阅连接，把事件分发给 [`AgentApi::subscribe`]
//!   注册的回调，直到连接结束；返回结束的原因，界面随之退出，不重连、不重新拉起后台。
//!   订阅回调跨重连保留。
//! - 每次新建连接都重新读 `ipc.json`：服务进程重启后端口与令牌会变。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use oink_foundation::{Point, Size};
use serde::de::DeserializeOwned;

use crate::api::*;
use crate::ipc::{
    BuildIdentity, CLOSING_BUILD_MISMATCH, CLOSING_LAGGED, CLOSING_STOPPING, Endpoint, Envelope, ServiceInfo,
};

/// 连接超时（本机回环，服务进程在运行时立即连上）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// 一次调用最长等多久（`connect_by_ip` 会等网络数秒；导出诊断包、识别照片可能更久）。
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// 连接池最多保留几条空闲连接。
const POOL_MAX: usize = 4;

/// 事件订阅为什么结束。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventsEnd {
    /// A different bundle build owns the endpoint. Exit; never reconnect to it.
    BuildMismatch,
    /// 服务进程告知它正在按请求退出（`oink-agent stop`、界面「退出 Oink」、安装脚本）。
    Stopping,
    /// 服务进程仍在运行，但断开了这条订阅（界面积压太多）；界面仍按订阅结束退出。
    Lagged,
    /// 没有预告就断开（服务进程崩溃或被结束），或连不上。
    Lost(String),
    /// 本进程调用了 [`RemoteApi::stop_following`]。
    Cancelled,
}

struct Conn<R: Read = TcpStream> {
    endpoint: Endpoint,
    reader: BufReader<R>,
}

struct Inner {
    client_id: [u8; 16],
    data_dir: PathBuf,
    pool: Mutex<Vec<Conn>>,
    sinks: Mutex<Vec<(u64, EventSink)>>,
    next_sink: AtomicU64,
    /// 正在跟随事件的连接（`stop_following` 用它打断阻塞的读取）。
    events: Mutex<Option<Arc<TcpStream>>>,
    cancelled: std::sync::atomic::AtomicBool,
    live_views: Mutex<LiveViews>,
    events_ready: std::sync::atomic::AtomicBool,
    build_mismatch: std::sync::atomic::AtomicBool,
}

/// 经本机 IPC 调用服务进程的 [`AgentApi`]。可以廉价克隆。
#[derive(Clone)]
pub struct RemoteApi {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for RemoteApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteApi")
            .field("data_dir", &self.inner.data_dir)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn unavailable(message: impl Into<String>) -> ApiError {
    ApiError::Unavailable {
        message: message.into(),
    }
}

impl RemoteApi {
    /// 调用数据目录 `data_dir`（`ipc.json` 所在）上的服务进程。不立即连接。
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                client_id: crate::api::request_context::new_client(),
                data_dir: data_dir.into(),
                pool: Mutex::default(),
                sinks: Mutex::default(),
                next_sink: AtomicU64::new(1),
                events: Mutex::default(),
                cancelled: Default::default(),
                live_views: Mutex::default(),
                events_ready: Default::default(),
                build_mismatch: Default::default(),
            }),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    pub fn build_mismatch(&self) -> bool {
        self.inner.build_mismatch.load(Ordering::Acquire)
    }

    fn reject_build(&self) -> ApiError {
        tracing::error!(client = ?BuildIdentity::current(), "local IPC build mismatch; exiting UI; reopen Oink from the current bundle");
        self.inner.build_mismatch.store(true, Ordering::Release);
        self.stop_following();
        unavailable("local IPC build mismatch; reopen Oink from the current bundle")
    }

    fn service_info(&self, value: serde_json::Value) -> ApiResult<ServiceInfo> {
        let info: ServiceInfo = serde_json::from_value(value).map_err(|_| self.reject_build())?;
        if info.build != BuildIdentity::current() {
            tracing::error!(backend = ?info.build, "backend belongs to a different bundle build");
            return Err(self.reject_build());
        }
        Ok(info)
    }

    /// Update this client's subscription, without changing another window's
    /// demand. Saving the value also covers changes before connection startup.
    pub fn set_live_views(&self, views: LiveViews) -> std::io::Result<()> {
        let mut events = lock(&self.inner.events);
        let mut previous = lock(&self.inner.live_views);
        if *previous == views {
            return Ok(());
        }
        *previous = views;
        if self.inner.events_ready.load(Ordering::Acquire)
            && let Some(stream) = events.as_mut()
        {
            let mut line = serde_json::to_vec(&views)?;
            line.push(b'\n');
            (&**stream).write_all(&line)?;
        }
        Ok(())
    }

    fn open(&self, timeout: Option<Duration>) -> Result<Conn, ApiError> {
        let endpoint = Endpoint::read(&self.inner.data_dir).map_err(|e| unavailable(format!("{e:#}")))?;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], endpoint.port));
        let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| unavailable(format!("连不上服务进程（端口 {}）：{e}", endpoint.port)))?;
        let _ = s.set_nodelay(true);
        s.set_read_timeout(timeout)
            .and_then(|()| s.set_write_timeout(Some(CALL_TIMEOUT)))
            .map_err(|e| unavailable(e.to_string()))?;
        Ok(Conn {
            endpoint,
            reader: BufReader::new(s),
        })
    }

    fn roundtrip<R: Read + Write>(conn: &mut Conn<R>, env: &Envelope) -> Result<serde_json::Value, ApiError> {
        let lost = |e: std::io::Error| unavailable(format!("与服务进程的连接中断：{e}"));
        let mut line = serde_json::to_string(env).map_err(|e| ApiError::Internal { message: e.to_string() })?;
        line.push('\n');
        conn.reader.get_mut().write_all(line.as_bytes()).map_err(lost)?;
        let mut out = String::new();
        if conn.reader.read_line(&mut out).map_err(|error| {
            if matches!(env.call, Some(Request::ExportDiagnostics { .. })) {
                unavailable(crate::ipc::diagnostics_read_error(error))
            } else {
                lost(error)
            }
        })? == 0
        {
            return Err(unavailable("服务进程断开了连接"));
        }
        serde_json::from_str(&out).map_err(|e| ApiError::Internal {
            message: format!("服务进程的应答无法解析：{e}"),
        })
    }

    /// 发一个请求信封，返回 `ok` 的内容。
    fn request(&self, make: impl FnOnce(String) -> Envelope) -> Result<serde_json::Value, ApiError> {
        if self.build_mismatch() {
            return Err(unavailable(
                "local IPC build mismatch; reopen Oink from the current bundle",
            ));
        }
        let pooled = lock(&self.inner.pool).pop();
        let mut conn = match pooled {
            Some(c) => c,
            None => self.open(Some(CALL_TIMEOUT))?,
        };
        let mut env = make(conn.endpoint.token.clone());
        env.build = Some(BuildIdentity::current());
        let timeout = if matches!(env.call, Some(Request::ExportDiagnostics { .. })) {
            crate::ipc::DIAGNOSTICS_SAVE_TIMEOUT
        } else {
            Some(CALL_TIMEOUT)
        };
        conn.reader
            .get_mut()
            .set_read_timeout(timeout)
            .map_err(|e| unavailable(e.to_string()))?;
        let mut v = Self::roundtrip(&mut conn, &env)?;
        if v.get("closing").and_then(|value| value.as_str()) == Some(CLOSING_BUILD_MISMATCH) {
            return Err(self.reject_build());
        }
        // A rejected handshake can be a startup race: the listener is already
        // bound while ipc.json still contains the previous process's token.
        // Reusing that connection also reuses its token forever. Drop rejected
        // connections so the next call reads the current endpoint; never retry
        // a request here, since a mutating request may already have executed.
        if let Some(e) = v.get_mut("err") {
            let e: ApiError = serde_json::from_value(e.take()).map_err(|e| ApiError::Internal {
                message: format!("服务进程的错误无法解析：{e}"),
            })?;
            return Err(e);
        }
        {
            let mut pool = lock(&self.inner.pool);
            if pool.len() < POOL_MAX {
                pool.push(conn);
            }
        }
        Ok(v.get_mut("ok").map(serde_json::Value::take).unwrap_or_default())
    }

    fn call<T: DeserializeOwned>(&self, req: Request) -> ApiResult<T> {
        let v = self.request(|token| Envelope {
            token,
            call: Some(req),
            client_id: Some(self.inner.client_id),
            ..Envelope::default()
        })?;
        serde_json::from_value(v).map_err(|e| ApiError::Internal {
            message: format!("服务进程的应答与界面版本不符：{e}"),
        })
    }

    /// 正在运行的服务进程（版本、进程号）；连不上时返回 [`ApiError::Unavailable`]。
    pub fn hello(&self) -> ApiResult<ServiceInfo> {
        let v = self.request(|token| Envelope {
            token,
            hello: true,
            ..Envelope::default()
        })?;
        self.service_info(v)
    }

    /// 请求服务进程优雅退出（应答表示请求已收到）。
    pub fn request_stop(&self) -> ApiResult<()> {
        self.request(|token| Envelope {
            token,
            stop: true,
            ..Envelope::default()
        })
        .map(|_| ())
    }

    /// 丢掉空闲连接（服务进程重启后旧连接都已失效）。
    pub fn reset_connections(&self) {
        lock(&self.inner.pool).clear();
    }

    /// 订阅服务进程的事件并在本线程上分发，直到订阅结束。连上后先调用 `connected`（此时可以重新拉取状态：
    /// 之后的变化都会作为事件到达）。
    pub fn follow_events(&self, connected: impl FnOnce(&ServiceInfo)) -> EventsEnd {
        if self.build_mismatch() {
            return EventsEnd::BuildMismatch;
        }
        self.inner.cancelled.store(false, Ordering::SeqCst);
        self.inner.events_ready.store(false, Ordering::Release);
        let conn = match self.open(None) {
            Ok(c) => c,
            Err(e) => return EventsEnd::Lost(e.to_string()),
        };
        // Share the exact receiving socket, not a duplicated Winsock descriptor.
        // Windows 11 parallel lifecycle evidence showed a pending recv surviving
        // shutdown on the duplicate and the UI waiting forever in join. Arc also
        // keeps this handle alive until the reader and cancellation have finished.
        let stream = Arc::new(conn.reader.into_inner());
        *lock(&self.inner.events) = Some(stream.clone());
        if self.inner.cancelled.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let mut conn = Conn {
            endpoint: conn.endpoint,
            reader: BufReader::new(&*stream),
        };
        let end = self.follow(&mut conn, connected);
        self.inner.events_ready.store(false, Ordering::Release);
        *lock(&self.inner.events) = None;
        if self.build_mismatch() {
            return EventsEnd::BuildMismatch;
        }
        if self.inner.cancelled.load(Ordering::SeqCst) {
            return EventsEnd::Cancelled;
        }
        end
    }

    fn follow<R: Read + Write>(&self, conn: &mut Conn<R>, connected: impl FnOnce(&ServiceInfo)) -> EventsEnd {
        let env = Envelope {
            token: conn.endpoint.token.clone(),
            build: Some(BuildIdentity::current()),
            subscribe: true,
            live_views: Some(*lock(&self.inner.live_views)),
            ..Envelope::default()
        };
        let hello = match Self::roundtrip(conn, &env) {
            Ok(v) => v,
            Err(e) => return EventsEnd::Lost(e.to_string()),
        };
        if hello.get("closing").and_then(|value| value.as_str()) == Some(CLOSING_BUILD_MISMATCH) {
            self.reject_build();
            return EventsEnd::BuildMismatch;
        }
        let info = match hello.get("ok").cloned().map(|value| self.service_info(value)) {
            Some(Ok(info)) => info,
            Some(Err(_)) => return EventsEnd::BuildMismatch,
            _ => return EventsEnd::Lost(format!("服务进程拒绝订阅：{hello}")),
        };
        // 新的服务进程（或重新订阅）：旧的请求连接可能指向已退出的进程。
        self.reset_connections();
        {
            // Serialize with setters: demand may have changed while the
            // authenticated subscribe handshake was in flight.
            let mut events = lock(&self.inner.events);
            self.inner.events_ready.store(true, Ordering::Release);
            if let Some(stream) = events.as_mut() {
                let mut line = serde_json::to_vec(&*lock(&self.inner.live_views)).expect("live views serialize");
                line.push(b'\n');
                if let Err(error) = (&**stream).write_all(&line) {
                    return EventsEnd::Lost(error.to_string());
                }
            }
        }
        connected(&info);
        let mut line = String::new();
        loop {
            if self.inner.cancelled.load(Ordering::SeqCst) {
                return EventsEnd::Cancelled;
            }
            line.clear();
            match conn.reader.read_line(&mut line) {
                Ok(0) => return EventsEnd::Lost("服务进程断开了连接".into()),
                Ok(_) => {}
                Err(e) => return EventsEnd::Lost(format!("与服务进程的连接中断：{e}")),
            }
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(%e, "unparsable line on the IPC event stream");
                    continue;
                }
            };
            if let Some(ev) = v.get("event") {
                match serde_json::from_value::<ApiEvent>(ev.clone()) {
                    Ok(ev) => self.emit(&ev),
                    Err(e) => return EventsEnd::Lost(format!("invalid event from same-build backend: {e}")),
                }
            } else if let Some(reason) = v.get("closing").and_then(|c| c.as_str()) {
                return match reason {
                    CLOSING_STOPPING => EventsEnd::Stopping,
                    CLOSING_LAGGED => EventsEnd::Lagged,
                    other => EventsEnd::Lost(format!("服务进程关闭了订阅：{other}")),
                };
            }
        }
    }

    /// 打断 [`Self::follow_events`]（界面退出时）。
    pub fn stop_following(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        if let Some(s) = lock(&self.inner.events).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn emit(&self, ev: &ApiEvent) {
        let sinks: Vec<EventSink> = lock(&self.inner.sinks).iter().map(|(_, s)| s.clone()).collect();
        for s in sinks {
            s(ev);
        }
    }
}

impl AgentApi for RemoteApi {
    fn layout_profile_command(&self, command: crate::layout_profiles::types::Command) -> ApiResult<serde_json::Value> {
        use crate::layout_profiles::types::Command;
        self.call(match command {
            Command::Query => Request::LayoutProfiles,
            Command::Create(v) => Request::CreateLayoutProfile(v),
            Command::Rename(v) => Request::RenameLayoutProfile(v),
            Command::Delete(v) => Request::DeleteLayoutProfile(v),
            Command::Load(v) => Request::LoadLayoutProfile(v),
            Command::Operation(v) => Request::LayoutProfileOperation(v),
            Command::Setting(v) => Request::SetSyncSystemArrangement(v),
            Command::Preview(v) => Request::PreviewSystemArrangement(v),
            Command::Commit(v) => Request::CommitArrangement(v),
            Command::Reset(v) => Request::ResetArrangement(v),
            Command::Replay(v) => Request::ReplayArrangement(v),
        })
    }

    fn device_cards(&self) -> ApiResult<crate::device_cards::DeviceCardsView> {
        self.call(Request::DeviceCards)
    }
    fn invoke_device_card_action(
        &self,
        request_id: String,
        token: String,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        self.call(Request::InvokeDeviceCardAction { request_id, token })
    }
    fn set_forwarding_controls(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u16,
        muted: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        self.set_forwarding_controls_with_persistence(request_id, version_token, volume_percent, muted, false)
    }
    fn set_forwarding_controls_with_persistence(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u16,
        muted: bool,
        defer_persistence: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        self.call(Request::SetForwardingControls {
            request_id,
            version_token,
            volume_percent,
            muted,
            defer_persistence,
        })
    }
    fn set_exit_volume(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u8,
        muted: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        self.call(Request::SetExitVolume {
            request_id,
            version_token,
            volume_percent,
            muted,
        })
    }
    fn set_local_paused(&self, request_id: String, paused: bool) -> ApiResult<crate::device_cards::DeviceControlReply> {
        self.call(Request::SetLocalPaused { request_id, paused })
    }
    fn set_device_paused(
        &self,
        request_id: String,
        token: String,
        paused: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        self.call(Request::SetDevicePaused {
            request_id,
            token,
            paused,
        })
    }
    fn pause_all(&self, request_id: String) -> ApiResult<crate::device_cards::PauseAllAccepted> {
        self.call(Request::PauseAll { request_id })
    }
    fn member_addresses(&self, device: DeviceKey) -> ApiResult<Vec<String>> {
        self.call(Request::MemberAddresses { device })
    }
    fn set_member_address(&self, device: DeviceKey, address: String) -> ApiResult<()> {
        self.call(Request::SetMemberAddress { device, address })
    }
    fn nearby(&self) -> ApiResult<pairing::NearbySnapshot> {
        self.call(Request::Nearby)
    }
    fn refresh_nearby(&self, operation_id: String) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.call(Request::RefreshNearby { operation_id })
    }
    fn query_pairing_address(
        &self,
        operation_id: String,
        generation: crate::device_cards::DecimalU64,
        ip: String,
        port: Option<u16>,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.call(Request::QueryPairingAddress {
            operation_id,
            generation,
            ip,
            port,
        })
    }
    fn release_pairing_query(&self, query_id: String, generation: crate::device_cards::DecimalU64) -> ApiResult<()> {
        self.call(Request::ReleasePairingQuery { query_id, generation })
    }
    fn start_pairing(
        &self,
        operation_id: String,
        target: pairing::PairTarget,
    ) -> ApiResult<pairing::PairReply<pairing::Started>> {
        self.call(Request::StartPairing { operation_id, target })
    }
    fn pairings(&self) -> ApiResult<pairing::PairingSnapshot> {
        self.call(Request::Pairings)
    }
    fn set_onboarding_connect(&self, active: bool) -> ApiResult<()> {
        self.call(Request::SetOnboardingConnect { active })
    }
    fn group_migration(&self) -> ApiResult<Option<pairing::GroupMigrationStatus>> {
        self.call(Request::GroupMigration)
    }
    fn accept_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.call(Request::AcceptPairing {
            pair_id,
            expected_revision,
        })
    }
    fn decline_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.call(Request::DeclinePairing {
            pair_id,
            expected_revision,
        })
    }
    fn cancel_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::CancelResult>> {
        self.call(Request::CancelPairing {
            pair_id,
            expected_revision,
        })
    }
    fn pairing_code(&self, pair_id: String) -> ApiResult<pairing::CodeView> {
        self.call(Request::PairingCode { pair_id })
    }
    fn submit_pairing_code(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
        attempt: u8,
        code: String,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.call(Request::SubmitPairingCode {
            pair_id,
            expected_revision,
            attempt,
            code,
        })
    }
    fn undo_pairing(
        &self,
        operation_id: String,
        handle: pairing::UndoHandle,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.call(Request::UndoPairing { operation_id, handle })
    }
    fn subscribe_pairing_events(
        &self,
        after_event_id: crate::device_cards::DecimalU64,
    ) -> ApiResult<Vec<pairing::PairingEvent>> {
        self.call(Request::SubscribePairingEvents { after_event_id })
    }
    fn ack_pairing_event(&self, event_id: crate::device_cards::DecimalU64) -> ApiResult<()> {
        self.call(Request::AckPairingEvent { event_id })
    }
    fn blocked_devices(&self) -> ApiResult<crate::device_cards::BlockedDevicesSnapshot> {
        self.call(Request::BlockedDevices)
    }
    fn block(&self, operation_id: String, device: DeviceKey) -> ApiResult<crate::device_cards::BlockReply> {
        self.call(Request::Block { operation_id, device })
    }
    fn unblock(&self, operation_id: String, device: DeviceKey) -> ApiResult<crate::device_cards::BlockReply> {
        self.call(Request::Unblock { operation_id, device })
    }
    fn notify(&self, notification: Notification) -> ApiResult<Option<NotificationReceipt>> {
        self.call(Request::Notify { notification })
    }
    fn ui_preferences(&self) -> ApiResult<UiPreferences> {
        self.call(Request::UiPreferences)
    }
    fn set_ui_preferences(&self, settings: PreferencePatch<UiPreferences>) -> ApiResult<UiPreferences> {
        self.call(Request::SetUiPreferences { settings })
    }
    fn audio_status(&self) -> ApiResult<oink_audio::Status> {
        self.call(Request::AudioStatus)
    }
    fn refresh_audio_devices(&self) -> ApiResult<()> {
        self.call(Request::RefreshAudioDevices)
    }
    fn set_audio_settings(&self, settings: PreferencePatch<oink_audio::Settings>) -> ApiResult<()> {
        self.call(Request::SetAudioSettings { settings })
    }

    fn overview(&self) -> ApiResult<Overview> {
        self.call(Request::Overview)
    }
    fn pause_status(&self) -> ApiResult<PauseStatus> {
        self.call(Request::PauseStatus)
    }

    fn game_mode(&self) -> ApiResult<GameModeStatus> {
        self.call(Request::GameMode)
    }
    fn set_screen_disabled(&self, device: DeviceKey, disabled: bool) -> ApiResult<()> {
        self.call(Request::SetScreenDisabled { device, disabled })
    }
    fn set_input_transform(&self, device: DeviceKey, settings: InputTransform) -> ApiResult<()> {
        self.call(Request::SetInputTransform { device, settings })
    }
    fn set_keyboard_disabled(&self, device: DeviceKey, disabled: bool) -> ApiResult<()> {
        self.call(Request::SetKeyboardDisabled { device, disabled })
    }
    fn set_game_mode(&self, mode: oink_routing::GameMode) -> ApiResult<()> {
        self.call(Request::SetGameMode { mode })
    }
    fn toggle_game_mode(&self) -> ApiResult<()> {
        self.call(Request::ToggleGameMode)
    }
    fn lock_focus_to(&self, device: DeviceKey) -> ApiResult<()> {
        self.call(Request::LockFocusTo { device })
    }
    fn set_focus_locked(&self, locked: bool) -> ApiResult<()> {
        self.call(Request::SetFocusLocked { locked })
    }
    fn return_pointer_home(&self) -> ApiResult<()> {
        self.call(Request::ReturnPointerHome)
    }

    fn group(&self) -> ApiResult<GroupView> {
        self.call(Request::Group)
    }
    fn rename_device(&self, device: DeviceKey, name: String) -> ApiResult<()> {
        self.call(Request::RenameDevice { device, name })
    }

    fn remove_member(&self, device: DeviceKey) -> ApiResult<()> {
        self.call(Request::RemoveMember { device })
    }
    fn leave_group(&self) -> ApiResult<()> {
        self.call(Request::LeaveGroup)
    }

    fn network_info(&self) -> ApiResult<NetworkInfo> {
        self.call(Request::NetworkInfo)
    }

    fn reset_initial_layout(&self) -> ApiResult<Layout> {
        self.call(Request::ResetInitialLayout)
    }
    fn layout(&self) -> ApiResult<Layout> {
        self.call(Request::Layout)
    }
    fn save_layout(&self, placements: Vec<Placement>) -> ApiResult<Layout> {
        self.call(Request::SaveLayout { placements })
    }
    fn set_screen_remote_gain(&self, screen: ScreenKey, gain: f64) -> ApiResult<()> {
        self.call(Request::SetScreenRemoteGain { screen, gain })
    }

    fn set_screen_gain(&self, screen: ScreenKey, gain: f64) -> ApiResult<()> {
        self.call(Request::SetScreenGain { screen, gain })
    }
    fn set_screen_hidden(&self, screen: ScreenKey, hidden: bool) -> ApiResult<()> {
        self.call(Request::SetScreenHidden { screen, hidden })
    }
    fn wallpapers(&self, refresh: bool) -> ApiResult<Vec<crate::api::ScreenWallpaper>> {
        self.call(Request::Wallpapers { refresh })
    }
    fn identify_screens(&self) -> ApiResult<()> {
        self.call(Request::IdentifyScreens)
    }
    fn aim_status(&self) -> ApiResult<AimStatus> {
        self.call(Request::AimStatus)
    }
    fn aim_start(&self) -> ApiResult<AimStatus> {
        self.call(Request::AimStart)
    }
    fn aim_stop(&self) -> ApiResult<()> {
        self.call(Request::AimStop)
    }
    fn layout_projection(&self) -> ApiResult<ProjectionStatus> {
        self.call(Request::LayoutProjection)
    }
    fn set_layout_projection(&self, enabled: bool) -> ApiResult<ProjectionStatus> {
        self.call(Request::SetLayoutProjection { enabled })
    }
    fn preview_layout_projection(
        &self,
        gesture: u64,
        placements: Vec<Placement>,
        affected: Vec<ScreenKey>,
        ended: bool,
        arrangement: Option<Box<crate::layout_profiles::types::ArrangementSnapshot>>,
    ) -> ApiResult<()> {
        self.call(Request::PreviewLayoutProjection {
            gesture,
            placements,
            affected,
            ended,
            arrangement,
        })
    }
    fn preview_move_screen(&self, screen: ScreenKey, origin: Point) -> ApiResult<Option<LayoutGeometry>> {
        self.call(Request::PreviewMoveScreen { screen, origin })
    }
    fn preview_move_device(&self, device: DeviceKey, delta: Point) -> ApiResult<Option<LayoutGeometry>> {
        self.call(Request::PreviewMoveDevice { device, delta })
    }
    fn move_screen(&self, screen: ScreenKey, origin: Point) -> ApiResult<Layout> {
        self.call(Request::MoveScreen { screen, origin })
    }
    fn move_device(&self, device: DeviceKey, delta: Point) -> ApiResult<Layout> {
        self.call(Request::MoveDevice { device, delta })
    }
    fn set_screen_size(&self, screen: ScreenKey, size: Size) -> ApiResult<Layout> {
        self.call(Request::SetScreenSize { screen, size })
    }

    fn submit_crash_capsule(
        &self,
        generation: u64,
        crash: oink_telemetry::SafeCrash,
        collected_at: u64,
    ) -> ApiResult<()> {
        self.call(Request::SubmitCrashCapsule {
            generation,
            crash,
            collected_at,
        })
    }
    fn report_status(&self) -> ApiResult<crate::telemetry::ReportStatus> {
        self.call(Request::ReportStatus)
    }
    fn set_report_consent(
        &self,
        expected_generation: u64,
        choice: oink_telemetry::Choice,
        policy_version: u16,
    ) -> ApiResult<()> {
        self.call(Request::SetReportConsent {
            expected_generation,
            choice,
            policy_version,
        })
    }
    fn input_settings(&self) -> ApiResult<InputSettings> {
        self.call(Request::InputSettings)
    }
    fn set_input_settings(&self, settings: PreferencePatch<InputSettings>) -> ApiResult<()> {
        self.call(Request::SetInputSettings { settings })
    }
    fn cache_usage(&self, device: Option<DeviceKey>) -> ApiResult<Vec<CacheDevice>> {
        self.call(Request::CacheUsage { device })
    }
    fn clear_cache(&self, device: Option<DeviceKey>) -> ApiResult<CacheDeviceClear> {
        self.call(Request::ClearCache { device })
    }
    fn received_files_usage(&self) -> ApiResult<ReceivedFilesUsage> {
        self.call(Request::ReceivedFilesUsage)
    }
    fn set_received_files_cleanup(&self, cleanup: ReceivedFilesCleanup) -> ApiResult<()> {
        self.call(Request::SetReceivedFilesCleanup { cleanup })
    }
    fn open_received_files(&self) -> ApiResult<()> {
        self.call(Request::OpenReceivedFiles)
    }
    fn clipboard_settings(&self) -> ApiResult<ClipboardSettings> {
        self.call(Request::ClipboardSettings)
    }
    fn set_clipboard_settings(&self, settings: PreferencePatch<ClipboardSettings>) -> ApiResult<()> {
        self.call(Request::SetClipboardSettings { settings })
    }
    fn clipboard_activity(&self) -> ApiResult<Vec<ClipActivity>> {
        self.call(Request::ClipboardActivity)
    }
    fn clipboard_transfer(&self) -> ApiResult<Option<FileTransferStatus>> {
        self.call(Request::ClipboardTransfer)
    }
    fn fetch_clipboard_files(&self) -> ApiResult<()> {
        self.call(Request::FetchClipboardFiles)
    }
    fn cancel_clipboard_files(&self) -> ApiResult<()> {
        self.call(Request::CancelClipboardFiles)
    }
    fn file_provider_status(&self) -> ApiResult<FileProviderStatus> {
        self.call(Request::FileProviderStatus)
    }
    fn open_file_provider_enable(&self) -> ApiResult<()> {
        self.call(Request::OpenFileProviderEnable)
    }
    fn file_provider_placeholder_pool(&self) -> ApiResult<FileProviderPlaceholderStatus> {
        self.call(Request::FileProviderPlaceholderPool)
    }
    fn set_file_provider_placeholder_pool(&self, enabled: bool) -> ApiResult<()> {
        self.call(Request::SetFileProviderPlaceholderPool { enabled })
    }

    fn update_status(&self) -> ApiResult<UpdateStatus> {
        self.call(Request::QueryUpdateStatus)
    }
    fn update_command(&self, command: UpdateCommand) -> ApiResult<UpdateStatus> {
        self.call(Request::Update { command })
    }
    fn audio_quality(&self) -> ApiResult<AudioQuality> {
        self.call(Request::AudioQuality)
    }
    fn connection_quality(&self) -> ApiResult<ConnectionQuality> {
        self.call(Request::ConnectionQuality)
    }
    fn status(&self) -> ApiResult<StatusView> {
        self.call(Request::Status)
    }
    fn logs(&self, after: Option<u64>, limit: u32) -> ApiResult<Vec<LogLine>> {
        self.call(Request::Logs { after, limit })
    }
    fn export_diagnostics_range(&self, last_minutes: Option<u32>) -> ApiResult<DiagnosticsBundle> {
        self.call(Request::ExportDiagnostics { last_minutes })
    }
    fn diagnostics_folder(&self) -> ApiResult<String> {
        self.call(Request::DiagnosticsFolder)
    }
    fn diagnostics_status(&self) -> ApiResult<DiagnosticRecorderStatus> {
        self.call(Request::DiagnosticsStatus)
    }
    fn set_diagnostics_enabled(&self, enabled: bool) -> ApiResult<()> {
        self.call(Request::SetDiagnosticsEnabled { enabled })
    }

    fn permissions(&self) -> ApiResult<Vec<PermissionItem>> {
        self.call(Request::Permissions)
    }
    fn request_permission(&self, kind: PermissionKind) -> ApiResult<()> {
        self.call(Request::RequestPermission { kind })
    }
    fn open_permission_settings(&self, kind: PermissionKind) -> ApiResult<()> {
        self.call(Request::OpenPermissionSettings { kind })
    }
    fn complete_onboarding(&self) -> ApiResult<()> {
        self.call(Request::CompleteOnboarding)
    }
    fn launch_at_login_status(&self) -> ApiResult<LaunchAtLoginStatus> {
        self.call(Request::LaunchAtLoginStatus)
    }
    fn set_launch_at_login(&self, enabled: bool) -> ApiResult<LaunchAtLoginStatus> {
        self.call(Request::SetLaunchAtLogin { enabled })
    }
    fn open_launch_at_login_settings(&self) -> ApiResult<()> {
        self.call(Request::OpenLaunchAtLoginSettings)
    }
    fn reopen_oink(&self) -> ApiResult<()> {
        self.call(Request::ReopenOink)
    }
    fn reset_group_state(&self) -> ApiResult<()> {
        self.call(Request::ResetGroupState)
    }
    fn show_window(&self) -> ApiResult<()> {
        self.call(Request::ShowWindow)
    }
    fn subscribe(&self, sink: EventSink) -> SubscriptionId {
        let id = self.inner.next_sink.fetch_add(1, Ordering::Relaxed);
        lock(&self.inner.sinks).push((id, sink));
        SubscriptionId(id)
    }
    fn unsubscribe(&self, id: SubscriptionId) {
        lock(&self.inner.sinks).retain(|(i, _)| *i != id.0);
    }
}
