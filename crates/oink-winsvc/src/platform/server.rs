use super::*;
use crate::wire::{Command, ErrorStage, FRAME_SIZE, REPLY_SIZE, Reply, Request};
use std::{collections::BTreeMap, time::Instant};
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::{Pipes::*, RemoteDesktop::*, StationsAndDesktops::*},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
const TAG: usize = 0x4F49_4E4B;
struct Desktop {
    handle: HDESK,
    name: String,
}
impl Drop for Desktop {
    fn drop(&mut self) {
        // SAFETY: owned desktop handle; never close borrowed original.
        unsafe {
            CloseDesktop(self.handle);
        }
    }
}
fn open_desktop() -> io::Result<Desktop> {
    // SAFETY: no inheritance, minimal input desktop access; thread has no hooks/windows.
    let h = unsafe {
        OpenInputDesktop(
            0,
            0,
            DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_SWITCHDESKTOP | DESKTOP_JOURNALPLAYBACK,
        )
    };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut desk = Desktop {
        handle: h,
        name: String::new(),
    };
    let mut name = [0u16; 128];
    let mut needed = 0;
    // SAFETY: output buffer sized in bytes and desktop handle valid.
    check(unsafe {
        GetUserObjectInformationW(
            h,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            size_of_val(&name) as u32,
            &mut needed,
        )
    })?;
    desk.name = String::from_utf16_lossy(&name[..name.iter().position(|n| *n == 0).unwrap_or(name.len())]);
    Ok(desk)
}
#[derive(Default)]
struct Injector {
    desktop: Option<Desktop>,
    held: BTreeMap<(u16, u16), Command>,
    pending: BTreeMap<String, BTreeMap<(u16, u16), Command>>,
    cleanup_required: bool,
}
fn input(cmd: Command) -> Option<INPUT> {
    let (kind, mi, ki) = match cmd {
        Command::MoveRelative { dx, dy } => (
            INPUT_MOUSE,
            MOUSEINPUT {
                dx,
                dy,
                // User-tested on Windows 11 build 26200: Endfield's camera Y
                // inverted with NOCOALESCE; MOVE with the OINK tag worked.
                // The game-side cause is unknown. Match the local builder,
                // preserving signed deltas/echo filtering and leaving absolute moves separate.
                // See docs/design/game-mode.md.
                dwFlags: MOUSEEVENTF_MOVE,
                dwExtraInfo: TAG,
                ..Default::default()
            },
            KEYBDINPUT::default(),
        ),
        command @ (Command::Move { x, y } | Command::DragPress { x, y, .. } | Command::DragRelease { x, y, .. }) => (
            INPUT_MOUSE,
            MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE
                    | MOUSEEVENTF_ABSOLUTE
                    | MOUSEEVENTF_VIRTUALDESK
                    | if matches!(command, Command::DragPress { .. }) {
                        MOUSEEVENTF_LEFTDOWN
                    } else if matches!(command, Command::DragRelease { .. }) {
                        MOUSEEVENTF_LEFTUP
                    } else {
                        0
                    },
                time: 0,
                dwExtraInfo: match command {
                    Command::DragPress { untagged: true, .. } | Command::DragRelease { untagged: true, .. } => 0,
                    _ => TAG,
                },
            },
            KEYBDINPUT::default(),
        ),
        Command::Wheel { amount, horizontal } => (
            INPUT_MOUSE,
            MOUSEINPUT {
                mouseData: amount as u32,
                dwFlags: if horizontal {
                    MOUSEEVENTF_HWHEEL
                } else {
                    MOUSEEVENTF_WHEEL
                },
                dwExtraInfo: TAG,
                ..Default::default()
            },
            KEYBDINPUT::default(),
        ),
        Command::Button { button, down, untagged } => {
            let flags = match (button, down) {
                (0, true) => MOUSEEVENTF_LEFTDOWN,
                (0, false) => MOUSEEVENTF_LEFTUP,
                (1, true) => MOUSEEVENTF_RIGHTDOWN,
                (1, false) => MOUSEEVENTF_RIGHTUP,
                (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                (2, false) => MOUSEEVENTF_MIDDLEUP,
                (_, true) => MOUSEEVENTF_XDOWN,
                (_, false) => MOUSEEVENTF_XUP,
            };
            (
                INPUT_MOUSE,
                MOUSEINPUT {
                    mouseData: if button >= 3 { u32::from(button) - 2 } else { 0 },
                    dwFlags: flags,
                    // Quicker compatibility mode (see oink-input inject::untag_for_quicker).
                    dwExtraInfo: if untagged { 0 } else { TAG },
                    ..Default::default()
                },
                KEYBDINPUT::default(),
            )
        }
        Command::Key {
            code,
            scan,
            extended,
            down,
            vk_scan,
        } => (
            INPUT_KEYBOARD,
            MOUSEINPUT::default(),
            KEYBDINPUT {
                wVk: if scan { 0 } else { code },
                wScan: if scan { code } else { vk_scan },
                dwFlags: if scan { KEYEVENTF_SCANCODE } else { 0 }
                    | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }
                    | if down { 0 } else { KEYEVENTF_KEYUP },
                time: 0,
                dwExtraInfo: TAG,
            },
        ),
        // Text is a press and release per unit; nothing is held across commands.
        Command::Unicode { unit, down } => (
            INPUT_KEYBOARD,
            MOUSEINPUT::default(),
            KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | if down { 0 } else { KEYEVENTF_KEYUP },
                time: 0,
                dwExtraInfo: TAG,
            },
        ),
        _ => return None,
    };
    Some(INPUT {
        r#type: kind,
        Anonymous: if kind == INPUT_MOUSE {
            INPUT_0 { mi }
        } else {
            INPUT_0 { ki }
        },
    })
}
fn send(cmd: Command) -> io::Result<()> {
    if let Command::DragRelease { x, y, .. } = cmd {
        let mut clip = RECT::default();
        // Validate on the actual injection desktop too, not only in the client.
        // SAFETY: read-only desktop metrics and valid output rectangle.
        let (px, py) = unsafe {
            check(GetClipCursor(&mut clip))?;
            (
                i64::from(GetSystemMetrics(SM_XVIRTUALSCREEN))
                    + i64::from(x) * i64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN)) / 65536,
                i64::from(GetSystemMetrics(SM_YVIRTUALSCREEN))
                    + i64::from(y) * i64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN)) / 65536,
            )
        };
        if px < i64::from(clip.left)
            || py < i64::from(clip.top)
            || px >= i64::from(clip.right)
            || py >= i64::from(clip.bottom)
        {
            return Err(denied());
        }
    }
    if let Some(i) = input(cmd) {
        // SAFETY: input constructed only from validated limited protocol commands.
        let (sent, error) = unsafe {
            SetLastError(0);
            let sent = SendInput(1, &i, size_of::<INPUT>() as i32);
            (sent, GetLastError())
        };
        if sent != 1 {
            return Err(io::Error::from_raw_os_error(
                if error == 0 { ERROR_GEN_FAILURE } else { error } as i32,
            ));
        }
    }
    Ok(())
}

impl Injector {
    fn needs_maintenance(&self) -> bool {
        !self.held.is_empty() || self.pending.values().any(|held| !held.is_empty())
    }
    fn release(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        self.held.retain(|_, cmd| match send(*cmd) {
            Ok(()) => false,
            Err(e) => {
                result = Err(e);
                true
            }
        });
        self.cleanup_required = !self.held.is_empty();
        result
    }
    fn attach(&mut self) -> io::Result<()> {
        let next = open_desktop()?;
        if self.desktop.as_ref().is_some_and(|d| d.name == next.name) {
            return if self.cleanup_required { self.release() } else { Ok(()) };
        }
        // Try releases on the old desktop, but an inactive desktop may reject SendInput.
        // Retain a per-desktop cleanup set and retry when that desktop becomes active again.
        // Also clear global physical key state on the newly active desktop, without replaying downs.
        let old_held = self.held.clone();
        let _ = self.release();
        if let Some(old) = &self.desktop {
            self.pending
                .entry(old.name.clone())
                .or_default()
                .extend(self.held.clone());
        }
        // SAFETY: this dedicated thread never owns a window or a hook.
        check(unsafe { SetThreadDesktop(next.handle) })?;
        self.held = self.pending.remove(&next.name).unwrap_or_default();
        self.held.extend(old_held);
        self.desktop = Some(next);
        self.release()?;
        Ok(())
    }
    fn apply(&mut self, cmd: Command, stage: &mut ErrorStage) -> io::Result<()> {
        if cmd == Command::Release {
            *stage = ErrorStage::Release;
            return self.release();
        }
        *stage = ErrorStage::Desktop;
        // Original native drag authority belongs to Default. Do not reinterpret
        // its recorded point on a newly active credential desktop.
        if matches!(cmd, Command::DragRelease { .. }) && open_desktop()?.name != "Default" {
            return Err(denied());
        }
        self.attach()?;
        *stage = ErrorStage::Inject;
        send(cmd)?;
        self.record(cmd);
        Ok(())
    }
    /// Ledger of the Ups that close presses this helper injected.
    fn record(&mut self, cmd: Command) {
        match cmd {
            Command::DragRelease { .. } => {
                self.held.remove(&(0, 0));
            }
            Command::DragPress { untagged, .. } => {
                // Cleanup keeps the press's tagging so Quicker sees a paired Up.
                self.held.insert(
                    (0, 0),
                    Command::Button {
                        button: 0,
                        down: false,
                        untagged,
                    },
                );
            }
            Command::Key {
                code,
                scan,
                extended,
                down,
                vk_scan,
            } => {
                let key = (1 + u16::from(scan) + u16::from(extended) * 2, code);
                if down {
                    self.held.insert(
                        key,
                        Command::Key {
                            code,
                            scan,
                            extended,
                            down: false,
                            vk_scan,
                        },
                    );
                } else {
                    self.held.remove(&key);
                }
            }
            Command::Button { button, down, untagged } => {
                if down {
                    self.held.insert(
                        (0, button),
                        Command::Button {
                            button,
                            down: false,
                            untagged,
                        },
                    );
                } else {
                    self.held.remove(&(0, button));
                }
            }
            _ => {}
        }
    }
}
pub(super) fn run(parent_pid: u32, route: Option<crate::prelogin_route::Source>) -> io::Result<()> {
    // SAFETY: dedicated helper process sets DPI awareness before any desktop/input access.
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }

    // SAFETY: pin broker process; only SYSTEM can enter this helper. Parent exit triggers cleanup.
    let parent = Handle::new(unsafe { OpenProcess(SYNCHRONIZE, 0, parent_pid) })?;
    let id = session()?;
    // SAFETY: read-only active console session query.
    if id == 0 || unsafe { WTSGetActiveConsoleSessionId() } != id {
        // Retired before an injector exists: no ledger requires cleanup.
        return Ok(());
    }
    // The service selects this one source at helper creation. An arbitrary
    // SYSTEM process or a source-provided session is never authorization.
    let _source = route.as_ref().map(super::prelogin_peer::pin).transpose()?;
    let account = console::account(id);
    if let Some(source) = &route {
        if !crate::console_policy::allows(source.identity.session, id, &account, &source.owner_sid) {
            return Err(denied());
        }
        if let Some(sid) = &source.user_sid {
            security::grant_token_query(sid)?;
        }
    }
    let current = || console::matches_account(id, &account) && route.as_ref().is_none_or(console::source_current);
    if !current() {
        // Token creation can race helper startup at logon. No input has been
        // admitted, so retirement is clean and must not latch a release failure.
        return Ok(());
    }
    if route.is_some() {
        // SAFETY: own kernel PID; command-line arguments alone do not prove parentage.
        if prelogin::parent_pid(unsafe { GetCurrentProcessId() })? != parent_pid {
            return Err(denied());
        }
    } else {
        security::grant_own_token_query(id)?;
    }
    let sd = if let Some(source) = &route {
        match &source.user_sid {
            Some(sid) => security::sid_security(sid, 0x12019b)?,
            None => security::system_security()?,
        }
    } else {
        security::pipe_security(id)?
    };
    let sa = sd.attrs();
    // SAFETY: fixed local name, protected DACL, one instance, overlapped message mode.
    let pipe = Handle::new(unsafe {
        CreateNamedPipeW(
            if route.is_some() {
                prelogin_pipe_name(id)
            } else {
                pipe_name(id)
            }
            .as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            &sa,
        )
    })?;
    let _ = oink_foundation::realtime::apply(
        oink_foundation::realtime::ThreadRole::Inject,
        oink_foundation::realtime::Tier::Realtime,
    );
    let signal = pipe_io::Signal::new()?;
    let notify = signal.clone();
    let priority = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-1));
    let native_priority = priority.clone();
    let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let native_released = released.clone();
    let native_account = account.clone();
    let native_source = route.clone();
    let mut workers = crate::helper::Server::new(
        move || {
            let applied = oink_foundation::realtime::apply(
                oink_foundation::realtime::ThreadRole::Inject,
                oink_foundation::realtime::Tier::Realtime,
            )?;
            native_priority.store(applied.native, std::sync::atomic::Ordering::Release);
            Ok(Native {
                id,
                injector: Injector::default(),
                priority: native_priority,
                released: native_released,
                account: native_account,
                source: native_source,
            })
        },
        move || Ordinary {
            id,
            desktop: None,
            priority,
        },
        std::sync::Arc::new(move || notify.notify()),
    )?;
    let stop_name = wide(&format!(r"Global\Oink.Input.Stop.{id}"));
    // SAFETY: existing broker-owned SYSTEM-only event; ACL/identity unchanged.
    let stop = Handle::new(unsafe { OpenEventW(SYNCHRONIZE, 0, stop_name.as_ptr()) })?;
    // SAFETY: initially unarmed private lease timer. The service alone owns
    // session retirement and signals stop before replacing this helper.
    let timer = Handle::new(unsafe { CreateWaitableTimerW(ptr::null(), 0, ptr::null()) })?;
    'serve: loop {
        let mut connecting = pipe_io::Operation::new(&pipe)?;
        if connecting.connect().is_err() {
            workers.disconnect();
            // SAFETY: our one owned server instance, no pending I/O after cancel/drop.
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
            continue;
        }
        let handles = [connecting.event(), stop.0, parent.0];
        // SAFETY: all handles remain owned/live through wait.
        match unsafe { WaitForMultipleObjects(3, handles.as_ptr(), 0, INFINITE) } {
            WAIT_OBJECT_0 => {}
            n if n == WAIT_OBJECT_0 + 1 || n == WAIT_OBJECT_0 + 2 => break 'serve,
            _ => return Err(io::Error::last_os_error()),
        }
        connecting.cancel();
        drop(connecting);
        serve_connection(
            &pipe,
            ConnectionContext {
                id,
                stop: &stop,
                parent: &parent,
                timer: &timer,
                signal: &signal,
            },
            &mut workers,
            |pipe, id| match &route {
                Some(expected) => super::prelogin_peer::peer(pipe, expected),
                None => security::peer(pipe, id, false),
            },
            current,
            Instant::now,
        )?;
        workers.disconnect();
        // SAFETY: connection operations have cancelled/drained before disconnect.
        unsafe {
            DisconnectNamedPipe(pipe.0);
        }
        // SAFETY: zero-time queries on live owned handles and a read-only
        // console query. No thread/desktop/input state is changed.
        let stopped = unsafe {
            WaitForSingleObject(stop.0, 0) == WAIT_OBJECT_0
                || WaitForSingleObject(parent.0, 0) == WAIT_OBJECT_0
                || !current()
        };
        if stopped {
            break;
        }
    }
    workers.disconnect();
    workers.stop_native();
    if !released.load(std::sync::atomic::Ordering::Acquire) {
        return Err(io::Error::other("helper release unconfirmed"));
    }
    Ok(())
}
/// Own the connection's one-shot timer through every early return/error.
struct LeaseTimer<'a> {
    timer: &'a Handle,
    lease: crate::lease::Lease,
}
impl<'a> LeaseTimer<'a> {
    fn new(timer: &'a Handle, now: Instant) -> io::Result<Self> {
        let mut alarm = Self {
            timer,
            lease: Default::default(),
        };
        alarm.lease.connect(now);
        alarm.arm(now)?;
        Ok(alarm)
    }
    fn arm(&self, now: Instant) -> io::Result<()> {
        let deadline = self.lease.deadline().expect("connected lease");
        // Round up to the kernel's 100ns relative-time units. Never request a
        // periodic timer; rearming occurs only on renewal or an early delivery.
        let ticks = deadline
            .saturating_duration_since(now)
            .as_nanos()
            .div_ceil(100)
            .clamp(1, i64::MAX as u128) as i64;
        let due = -ticks;
        // SAFETY: private owned timer outlives this connection scope.
        check(unsafe { SetWaitableTimer(self.timer.0, &due, 0, None, ptr::null(), 0) })
    }
}
impl Drop for LeaseTimer<'_> {
    fn drop(&mut self) {
        self.lease.disconnect();
        // SAFETY: still-live private timer, including failed/closed connections.
        unsafe {
            CancelWaitableTimer(self.timer.0);
        }
    }
}
struct ConnectionContext<'a> {
    id: u32,
    stop: &'a Handle,
    parent: &'a Handle,
    timer: &'a Handle,
    signal: &'a pipe_io::Signal,
}
fn serve_connection(
    pipe: &Handle,
    context: ConnectionContext<'_>,
    workers: &mut crate::helper::Server,
    mut authenticate: impl FnMut(HANDLE, u32) -> io::Result<Handle>,
    active_session: impl Fn() -> bool,
    clock: impl Fn() -> Instant,
) -> io::Result<()> {
    let ConnectionContext {
        id,
        stop,
        parent,
        timer,
        signal,
    } = context;
    // Buffers precede BOTH operations; cancellation drains before their storage drops.
    let mut frame = [0u8; FRAME_SIZE];
    let mut response = [0u8; REPLY_SIZE];
    let mut read = pipe_io::Operation::new(pipe)?;
    let mut write = pipe_io::Operation::new(pipe)?;
    let mut writing = false;
    let mut peer: Option<Handle> = None;
    let mut generation = 0;
    let mut alarm = LeaseTimer::new(timer, clock())?;
    let mut closing = false;
    let mut failure = None;
    // SAFETY: frame remains live and untouched until read completion/cancellation.
    if unsafe { read.read(&mut frame) }.is_err() {
        // A client can disappear between ConnectNamedPipe and the first read.
        // That ends this connection, not the helper's single-instance listener.
        return Ok(());
    }
    loop {
        // A continuously signaled read/reply cannot outrank stop or expiry.
        // SAFETY: retained event/process objects, nonblocking stop queries.
        let stopped = unsafe {
            WaitForSingleObject(stop.0, 0) == WAIT_OBJECT_0 || WaitForSingleObject(parent.0, 0) == WAIT_OBJECT_0
        };
        if stopped || alarm.lease.expired(clock()) {
            break;
        }
        // Completion checks run even under a continuous inbound stream: a
        // ready ACK is never starved by lower-priority query arrivals.
        // SAFETY: zero-time event queries on owned, live operations.
        if writing && unsafe { WaitForSingleObject(write.event(), 0) } == WAIT_OBJECT_0 {
            if write.finish().ok() != Some(REPLY_SIZE as u32) {
                break;
            }
            writing = false;
        }
        if !writing {
            let mut next = failure.take();
            while next.is_none()
                && let Some(result) = workers.receive()
            {
                if result.generation != generation {
                    continue;
                }
                if !result.ordinary && result.reply.0[..4] != [0; 4] {
                    closing = true;
                }
                next = Some(result.reply);
            }
            if let Some(reply) = next {
                response.copy_from_slice(&reply.0);
                // SAFETY: response remains immutable through completion/cancel.
                if unsafe { write.write(&response) }.is_err() {
                    return Ok(());
                }
                writing = true;
            }
        }
        if !writing {
            write.reset_idle_event();
        }
        // WaitForMultipleObjects forbids duplicate handles. Keep the dormant
        // write event nonsignaled; do not insert a second copy of the wake event.
        let handles = [read.event(), write.event(), signal.handle(), stop.0, parent.0, timer.0];
        // SAFETY: all six distinct handles remain live; idle write is nonsignaled.
        match unsafe { WaitForMultipleObjects(6, handles.as_ptr(), 0, INFINITE) } {
            WAIT_OBJECT_0 => {
                let count = match read.finish() {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if !closing {
                    let request = Request::decode(&frame[..count as usize]);
                    let mut stage = ErrorStage::Peer;
                    let result = (|| {
                        if peer.is_none() {
                            peer = Some(authenticate(pipe.0, id)?);
                            generation = workers.begin();
                        }
                        stage = ErrorStage::Liveness;
                        // SAFETY: process handle pinned by original authenticated peer check.
                        if unsafe { WaitForSingleObject(peer.as_ref().expect("authenticated").0, 0) } != WAIT_TIMEOUT {
                            return Err(denied());
                        }
                        if !active_session() {
                            return Err(denied());
                        }
                        stage = ErrorStage::Decode;
                        let request =
                            request.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid input frame"))?;
                        if !alarm.lease.frame(clock(), request.command == Command::Heartbeat) {
                            return Err(denied());
                        }
                        if request.command == Command::Heartbeat {
                            alarm.arm(clock())?;
                        } else {
                            workers.submit(crate::helper::Work { generation, request })?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        // Preserve the former stage-bearing error ACK. Closing
                        // drains further input without executing or refreshing
                        // the lease; cleanup remains on the native owner.
                        failure = Some(error_reply(error, stage, id, request.map_or(0, |r| r.id)));
                        closing = true;
                        workers.disconnect();
                        // Input is already disconnected. Preserve the existing
                        // two-second error-ACK drain window without renewing any
                        // admitted input; closing frames are never executed.
                        alarm.lease.connect(clock());
                        alarm.arm(clock())?;
                        signal.notify();
                    }
                }
                // SAFETY: completed read; frame may be reused, including closing drain.
                if unsafe { read.read(&mut frame) }.is_err() {
                    break;
                }
            }
            n if n == WAIT_OBJECT_0 + 1 || n == WAIT_OBJECT_0 + 2 => {}
            n if n == WAIT_OBJECT_0 + 3 || n == WAIT_OBJECT_0 + 4 => break,
            n if n == WAIT_OBJECT_0 + 5 => {
                if alarm.lease.expired(clock()) {
                    break;
                }
                alarm.arm(clock())?;
            }
            _ => return Err(io::Error::last_os_error()),
        }
    }
    Ok(())
}
fn error_reply(error: io::Error, stage: ErrorStage, session_id: u32, request_id: u32) -> Reply {
    let mut reply = Reply([0; REPLY_SIZE]);
    let error = error
        .raw_os_error()
        .filter(|n| *n != 0)
        .unwrap_or(ERROR_ACCESS_DENIED as i32) as u32;
    // SAFETY: read-only current process identity, no desktop query on the I/O owner.
    let pid = unsafe { GetCurrentProcessId() };
    for (i, value) in [error, pid, session_id].into_iter().enumerate() {
        reply.0[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    reply.0[84..88].copy_from_slice(&(stage as u32).to_le_bytes());
    reply.0[88..92].copy_from_slice(&(-1i32).to_le_bytes());
    reply.with_id(request_id)
}
struct Native {
    id: u32,
    injector: Injector,
    priority: std::sync::Arc<std::sync::atomic::AtomicI32>,
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    account: crate::console_policy::Account,
    source: Option<crate::prelogin_route::Source>,
}
impl crate::helper::Native for Native {
    fn apply(&mut self, command: Command) -> Reply {
        let mut stage = ErrorStage::Desktop;
        // OpenInputDesktop only follows desktops within this process's session.
        // Check the physical console at execution (not just the service notification), so
        // queued input cannot enter a retired session after PIN/account switching.
        // Release still acts on the old attached desktop before the helper exits.
        // SAFETY: read-only console query.
        let result = if command != Command::Release
            && (!console::matches_account(self.id, &self.account)
                || self.source.as_ref().is_some_and(|s| !console::source_current(s)))
        {
            stage = ErrorStage::Liveness;
            Err(denied())
        } else {
            self.injector.apply(command, &mut stage).and_then(|()| {
                if command == Command::Release && self.injector.pending.values().any(|held| !held.is_empty()) {
                    Err(io::Error::other("old desktop release remains pending"))
                } else {
                    Ok(())
                }
            })
        };
        let priority = oink_foundation::realtime::read_back().map_or(-1, |p| p.native);
        self.priority.store(priority, std::sync::atomic::Ordering::Release);
        response(
            result,
            stage,
            self.injector.desktop.as_ref(),
            priority,
            self.id,
            command,
        )
    }
    fn release(&mut self) {
        let result = self.injector.release();
        self.released.store(
            result.is_ok() && self.injector.pending.values().all(BTreeMap::is_empty),
            std::sync::atomic::Ordering::Release,
        );
    }
    fn needs_maintenance(&self) -> bool {
        self.injector.needs_maintenance()
    }
    fn maintain(&mut self) {
        if self.needs_maintenance() {
            let _ = self.injector.attach();
        }
    }
}
struct Ordinary {
    id: u32,
    desktop: Option<Desktop>,
    priority: std::sync::Arc<std::sync::atomic::AtomicI32>,
}
impl crate::helper::Query for Ordinary {
    fn query(&mut self, command: Command) -> Reply {
        if command == Command::PowerSnapshot {
            let mut reply = Reply([0; REPLY_SIZE]);
            // Query-specific fixed payload: version, availability, two booleans.
            // No desktop work and no native diagnostic strings in this reply.
            reply.0[4] = 1;
            if let Ok(state) = super::power::observe() {
                reply.0[5] = 1;
                reply.0[6] = u8::from(state.display);
                reply.0[7] = u8::from(state.system);
            }
            return reply;
        }
        let result = (|| {
            let next = open_desktop()?;
            // Ordinary queries attach ONLY their own thread, never the input
            // ledger. This preserves fresh desktop/position observations without
            // holding native cleanup behind an ordinary desktop-query round trip.
            // SAFETY: query thread has no windows/hooks and owns the next desktop.
            check(unsafe { SetThreadDesktop(next.handle) })?;
            self.desktop = Some(next);
            Ok(())
        })();
        response(
            result,
            ErrorStage::Desktop,
            self.desktop.as_ref(),
            self.priority.load(std::sync::atomic::Ordering::Acquire),
            self.id,
            command,
        )
    }
}
fn response(
    result: io::Result<()>,
    stage: ErrorStage,
    desktop: Option<&Desktop>,
    priority: i32,
    id: u32,
    command: Command,
) -> Reply {
    // SAFETY: read-only query on THIS owner's attached desktop.
    let cursor = unsafe {
        let mut p = POINT::default();
        if GetCursorPos(&mut p) == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(p)
        }
    };
    response_with_cursor(result, stage, desktop, priority, id, command, cursor)
}
fn response_with_cursor(
    mut result: io::Result<()>,
    mut stage: ErrorStage,
    desktop: Option<&Desktop>,
    priority: i32,
    id: u32,
    command: Command,
    cursor: io::Result<POINT>,
) -> Reply {
    let mut bytes = [0u8; REPLY_SIZE];
    let pos = match cursor {
        Ok(p) => p,
        Err(error) => {
            // Release acknowledges the old desktop's ledger, not cursor facts.
            // A never-used native lane can still be attached to Winlogon after
            // login; GetCursorPos requires the input desktop and may fail even
            // though there are no holds (or every Up already succeeded). Do not
            // switch that thread merely for metadata, or turn cleanup into a
            // false handoff failure. Real release errors remain authoritative;
            // probes and other input replies retain their cursor requirement.
            if command != Command::Release && result.is_ok() {
                stage = ErrorStage::Cursor;
                result = Err(error);
            }
            POINT::default()
        }
    };
    // SAFETY: read-only own process identity.
    let pid = unsafe { GetCurrentProcessId() };
    let error = result.err().map_or(0, |e| {
        e.raw_os_error()
            .filter(|n| *n != 0)
            .unwrap_or(ERROR_ACCESS_DENIED as i32) as u32
    });
    for (i, value) in [error, pid, id, pos.x as u32, pos.y as u32].iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    if error != 0 {
        bytes[84..88].copy_from_slice(&(stage as u32).to_le_bytes());
    }
    if let Some(desktop) = desktop {
        let n = desktop.name.len().min(63);
        bytes[20..20 + n].copy_from_slice(&desktop.name.as_bytes()[..n]);
    }
    bytes[crate::wire::REPLY_PRIORITY_OFFSET..92].copy_from_slice(&priority.to_le_bytes());
    // SAFETY: read-only toggle query on the owning thread's desktop.
    bytes[92..96].copy_from_slice(&u32::from(unsafe { GetKeyState(VK_CAPITAL as i32) } & 1 != 0).to_le_bytes());
    Reply(bytes)
}
fn detach() {
    // Windows disallows closing the current desktop. Each owner detaches before
    // dropping its own HDESK; no hook/window or other thread's desktop is touched.
    // SAFETY: fixed Default desktop; owned temporary handle, thread has no windows.
    unsafe {
        let d = OpenDesktopW(
            wide("Default").as_ptr(),
            0,
            0,
            DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS,
        );
        if !d.is_null() {
            SetThreadDesktop(d);
            CloseDesktop(d);
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        detach();
    }
}
impl Drop for Ordinary {
    fn drop(&mut self) {
        detach();
    }
}

#[cfg(test)]
mod isolated_pipe_tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    #[test]
    fn empty_desktop_ledgers_do_not_need_maintenance() {
        let mut injector = Injector::default();
        assert!(!injector.needs_maintenance());
        injector.pending.insert("inactive".into(), BTreeMap::new());
        assert!(!injector.needs_maintenance());
        let up = Command::Button {
            button: 0,
            down: false,
            untagged: false,
        };
        injector.pending.get_mut("inactive").unwrap().insert((0, 0), up);
        assert!(injector.needs_maintenance());
        injector.pending.clear();
        injector.held.insert((0, 0), up);
        assert!(injector.needs_maintenance());
        injector.held.clear();
        assert!(!injector.needs_maintenance());
    }
    #[test]
    fn final_release_ack_survives_unavailable_cursor_on_retired_desktop() {
        let reply = response_with_cursor(
            Ok(()),
            ErrorStage::Release,
            None,
            -1,
            2,
            Command::Release,
            Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)),
        );
        assert_eq!(
            &reply.0[..4],
            &0u32.to_le_bytes(),
            "empty/cleared ledger is a successful final ACK"
        );
    }
    #[test]
    fn cursor_unavailability_does_not_hide_real_cleanup_or_query_failures() {
        let release = response_with_cursor(
            Err(io::Error::from_raw_os_error(ERROR_GEN_FAILURE as i32)),
            ErrorStage::Release,
            None,
            -1,
            2,
            Command::Release,
            Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)),
        );
        assert_eq!(&release.0[..4], &ERROR_GEN_FAILURE.to_le_bytes());
        assert_eq!(&release.0[84..88], &(ErrorStage::Release as u32).to_le_bytes());
        for command in [Command::Ping, Command::Position, Command::Move { x: 1, y: 2 }] {
            let reply = response_with_cursor(
                Ok(()),
                ErrorStage::Desktop,
                None,
                -1,
                2,
                command,
                Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)),
            );
            assert_eq!(&reply.0[..4], &ERROR_ACCESS_DENIED.to_le_bytes());
            assert_eq!(&reply.0[84..88], &(ErrorStage::Cursor as u32).to_le_bytes());
        }
    }
    #[test]
    fn virtual_lease_expiry_and_pipe_disconnect_release_held_input() {
        use crate::isolation_tests::{Native as FakeNative, Os};
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Query;
        impl crate::helper::Query for Query {
            fn query(&mut self, _: Command) -> Reply {
                Reply([0; REPLY_SIZE])
            }
        }
        fn send(pipe: &Handle, command: Command) {
            let frame = Request {
                id: u32::from(command != Command::Heartbeat),
                ordinary: command == Command::Ping,
                command,
            }
            .encode();
            let mut op = pipe_io::Operation::new(pipe).unwrap();
            // SAFETY: frame outlives the cancelled/drained private operation.
            unsafe { op.write(&frame) }.unwrap();
            assert_eq!(op.wait(2000).unwrap(), FRAME_SIZE as u32);
            if command != Command::Heartbeat {
                let mut response = [0u8; REPLY_SIZE];
                let mut read = pipe_io::Operation::new(pipe).unwrap();
                // SAFETY: response outlives this private operation and its drain.
                unsafe { read.read(&mut response) }.unwrap();
                assert_eq!(read.wait(2000).unwrap(), REPLY_SIZE as u32);
                assert_eq!(&response[..4], &[0; 4]);
            }
        }
        for expire in [false, true] {
            let (server, client) = pipe_io::tests::pair();
            let server = pipe_io::SharedPipe::new(server);
            let os = Arc::new(Mutex::new(Os::default()));
            let native = os.clone();
            let milliseconds = Arc::new(AtomicU64::new(0));
            let clock = milliseconds.clone();
            let signal = pipe_io::Signal::new().unwrap();
            let wake = signal.clone();
            let (done, finished) = mpsc::channel();
            let task = std::thread::spawn(move || {
                let notify = signal.clone();
                let mut workers = crate::helper::Server::new(
                    move || Ok(FakeNative { os: native }),
                    || Query,
                    Arc::new(move || notify.notify()),
                )
                .unwrap();
                // SAFETY: private unnamed events/timer and our own process pin.
                let (stop, timer, parent) = unsafe {
                    (
                        Handle::new(CreateEventW(ptr::null(), 1, 0, ptr::null())).unwrap(),
                        Handle::new(CreateWaitableTimerW(ptr::null(), 0, ptr::null())).unwrap(),
                        Handle::new(OpenProcess(SYNCHRONIZE, 0, GetCurrentProcessId())).unwrap(),
                    )
                };
                let origin = Instant::now();
                serve_connection(
                    server.handle(),
                    ConnectionContext {
                        id: 1,
                        stop: &stop,
                        timer: &timer,
                        parent: &parent,
                        signal: &signal,
                    },
                    &mut workers,
                    |_, _| {
                        // SAFETY: fake authenticated peer is this test process.
                        Handle::new(unsafe { OpenProcess(SYNCHRONIZE, 0, GetCurrentProcessId()) })
                    },
                    || true,
                    || origin + Duration::from_millis(clock.load(Ordering::Acquire)),
                )
                .unwrap();
                workers.disconnect();
                workers.stop_native();
                // The connection scope has ended, so its timer must be disarmed.
                done.send(()).unwrap();
            });
            send(
                &client,
                Command::Button {
                    button: 0,
                    down: true,
                    untagged: false,
                },
            );
            assert_eq!(os.lock().unwrap().held.len(), 1);
            if expire {
                milliseconds.store(400, Ordering::Release);
                send(&client, Command::Heartbeat);
                send(&client, Command::Ping); // ordered barrier after renewal
                milliseconds.store(2399, Ordering::Release);
                send(&client, Command::Ping); // queries must not renew the lease
                assert_eq!(os.lock().unwrap().held.len(), 1);
                milliseconds.store(2400, Ordering::Release);
                wake.notify(); // injected event at the exact virtual deadline
            } else {
                drop(client); // cleanup must not wait for the lease deadline
            }
            finished
                .recv_timeout(Duration::from_secs(2))
                .expect("cleanup after disconnect/lease expiry");
            task.join().unwrap();
            assert!(os.lock().unwrap().held.is_empty());
            assert!(os.lock().unwrap().events.contains(&Command::Button {
                button: 0,
                down: false,
                untagged: false
            }));
        }
    }

    #[test]
    fn retired_console_rejects_new_input_then_releases_the_old_ledger() {
        use crate::isolation_tests::{Native as FakeNative, Os};
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Query;
        impl crate::helper::Query for Query {
            fn query(&mut self, _: Command) -> Reply {
                Reply([0; REPLY_SIZE])
            }
        }
        let (server_pipe, client_pipe) = pipe_io::tests::pair();
        let server_pipe = pipe_io::SharedPipe::new(server_pipe);
        let os = Arc::new(Mutex::new(Os::default()));
        let native_os = os.clone();
        let active = Arc::new(AtomicBool::new(true));
        let current = active.clone();
        let stop = pipe_io::Signal::new().unwrap();
        let stop_server = stop.clone();
        let helper = std::thread::spawn(move || {
            let signal = pipe_io::Signal::new().unwrap();
            let notify = signal.clone();
            let mut workers = crate::helper::Server::new(
                move || Ok(FakeNative { os: native_os }),
                || Query,
                Arc::new(move || notify.notify()),
            )
            .unwrap();
            // SAFETY: private test process/event, not the installed service.
            let parent = Handle::new(unsafe { OpenProcess(SYNCHRONIZE, 0, GetCurrentProcessId()) }).unwrap();
            // Only a one-shot lease timer is used. The next frame must reject
            // the retired target, independently of a service stop event.
            // SAFETY: unnamed, private timer, never performs a desktop operation.
            let timer = Handle::new(unsafe { CreateWaitableTimerW(ptr::null(), 0, ptr::null()) }).unwrap();
            // SAFETY: duplicate owned private event for ConnectionContext.
            let stop_handle = Handle::new(unsafe {
                let mut duplicate = ptr::null_mut();
                check(DuplicateHandle(
                    GetCurrentProcess(),
                    stop_server.handle(),
                    GetCurrentProcess(),
                    &mut duplicate,
                    SYNCHRONIZE,
                    0,
                    0,
                ))
                .unwrap();
                duplicate
            })
            .unwrap();
            serve_connection(
                server_pipe.handle(),
                ConnectionContext {
                    id: 1,
                    stop: &stop_handle,
                    parent: &parent,
                    timer: &timer,
                    signal: &signal,
                },
                &mut workers,
                |_, _| {
                    // SAFETY: fake authenticated peer is the current test process.
                    Handle::new(unsafe { OpenProcess(SYNCHRONIZE, 0, GetCurrentProcessId()) })
                },
                || current.load(Ordering::Acquire),
                Instant::now,
            )
            .unwrap();
            workers.disconnect();
            workers.stop_native();
        });
        let client = super::super::client::private_pipe_client(client_pipe).unwrap();
        let down = client
            .request(
                Command::Button {
                    button: 0,
                    down: true,
                    untagged: false,
                },
                false,
                false,
            )
            .unwrap()
            .unwrap();
        assert_eq!(&down.0[..4], &[0; 4]);
        active.store(false, Ordering::Release);
        let rejected = client.request(Command::Move { x: 12, y: 34 }, false, false);
        stop.notify();
        helper.join().unwrap();
        let rejected = rejected.unwrap().unwrap();
        assert_ne!(&rejected.0[..4], &[0; 4]);
        assert_eq!(
            u32::from_le_bytes(rejected.0[84..88].try_into().unwrap()),
            ErrorStage::Liveness as u32
        );
        assert!(os.lock().unwrap().held.is_empty());
    }

    #[test]
    fn private_overlapped_pipe_delivers_input_ack_while_query_is_blocked() {
        use crate::isolation_tests::{Native as FakeNative, Os, Query as FakeQuery};
        let (server_pipe, client_pipe) = pipe_io::tests::pair();
        let server_pipe = pipe_io::SharedPipe::new(server_pipe);
        let os = Arc::new(Mutex::new(Os::default()));
        let native_os = os.clone();
        let (entered, ready) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let stop = pipe_io::Signal::new().unwrap();
        let stop_server = stop.clone();
        let helper = std::thread::spawn(move || {
            let signal = pipe_io::Signal::new().unwrap();
            let notify = signal.clone();
            let mut workers = crate::helper::Server::new(
                move || Ok(FakeNative { os: native_os }),
                move || FakeQuery { entered, gate },
                Arc::new(move || notify.notify()),
            )
            .unwrap();
            // SAFETY: own process lifetime pin; this is not the installed broker.
            let parent = Handle::new(unsafe { OpenProcess(SYNCHRONIZE, 0, GetCurrentProcessId()) }).unwrap();
            // SAFETY: unnamed owned test timer, no service/system configuration.
            let timer = Handle::new(unsafe { CreateWaitableTimerW(ptr::null(), 0, ptr::null()) }).unwrap();
            // Test authentication/session callbacks are statically substituted;
            // only the private kernel transport runs, with no desktop backend.
            // SAFETY: duplicate the private event so ConnectionContext owns a live wrapper.
            let stop_handle = Handle::new(unsafe {
                let mut duplicate = ptr::null_mut();
                check(DuplicateHandle(
                    GetCurrentProcess(),
                    stop_server.handle(),
                    GetCurrentProcess(),
                    &mut duplicate,
                    SYNCHRONIZE,
                    0,
                    0,
                ))
                .unwrap();
                duplicate
            })
            .unwrap();
            serve_connection(
                server_pipe.handle(),
                ConnectionContext {
                    id: 0,
                    stop: &stop_handle,
                    parent: &parent,
                    timer: &timer,
                    signal: &signal,
                },
                &mut workers,
                |_, _| {
                    // SAFETY: fake peer pin is THIS test process, not an authenticated product client.
                    Handle::new(unsafe { OpenProcess(SYNCHRONIZE, 0, GetCurrentProcessId()) })
                },
                || true,
                Instant::now,
            )
            .unwrap();
            workers.disconnect();
            workers.stop_native();
        });
        let client = Arc::new(super::super::client::private_pipe_client(client_pipe).unwrap());
        let ordinary = client.clone();
        let query = std::thread::spawn(move || ordinary.request(Command::Ping, true, false));
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let input = client.clone();
        let (done, acked) = mpsc::channel();
        let injection = std::thread::spawn(move || {
            use oink_input::timing::{CallClass, InjectionTiming};
            let timing = InjectionTiming::new(2);
            let at = Instant::now();
            let pending = timing.pending(at);
            timing.native_begin(CallClass::Button, at);
            let result = input.request(
                Command::Button {
                    button: 0,
                    down: true,
                    untagged: false,
                },
                false,
                false,
            );
            timing.native_end(CallClass::Button, at.elapsed().as_micros() as u64);
            drop(pending);
            eprintln!("PRIVATE_PIPE_INPUT_TIMING {:?}", timing.take(Instant::now()));
            done.send(result).unwrap();
        });
        let result = acked.recv_timeout(Duration::from_secs(3));
        release.send(()).unwrap();
        injection.join().unwrap();
        let _ = query.join().unwrap();
        stop.notify();
        helper.join().unwrap();
        assert!(
            matches!(result, Ok(Ok(Some(_)))),
            "private pipe input ACK waited for ordinary query: {result:?}"
        );
        assert!(
            os.lock().unwrap().held.is_empty(),
            "disconnect/native stop owns release"
        );
    }
}

#[cfg(test)]
mod relative_input_tests {
    use super::*;

    #[test]
    fn game_relative_input_uses_only_move_and_keeps_echo_tag() {
        for (dx, dy) in [(7, -11), (-7, 11), (0, 0)] {
            let event = input(Command::MoveRelative { dx, dy }).unwrap();
            assert_eq!(event.r#type, INPUT_MOUSE);
            // SAFETY: MoveRelative initializes INPUT_MOUSE's mi member; no input is sent.
            let mouse = unsafe { event.Anonymous.mi };
            assert_eq!((mouse.dx, mouse.dy), (dx, dy));
            assert_eq!(mouse.dwFlags, MOUSEEVENTF_MOVE);
            assert_eq!(mouse.dwExtraInfo, TAG);
            assert_eq!((mouse.mouseData, mouse.time), (0, 0));
        }
    }
}

#[cfg(test)]
mod drag_press_tests {
    use super::*;
    #[test]
    fn quicker_mode_clears_only_button_tags_and_cleanup_keeps_the_press_tagging() {
        for untagged in [false, true] {
            let expected = if untagged { 0 } else { TAG };
            let mut commands = vec![
                Command::DragPress { x: 1, y: 2, untagged },
                Command::DragRelease { x: 1, y: 2, untagged },
            ];
            for button in 0..5 {
                for down in [false, true] {
                    commands.push(Command::Button { button, down, untagged });
                }
            }
            for command in commands {
                // SAFETY: these commands construct INPUT_MOUSE without sending it.
                assert_eq!(unsafe { input(command).unwrap().Anonymous.mi.dwExtraInfo }, expected);
            }
        }
        for command in [
            Command::Move { x: 1, y: 2 },
            Command::MoveRelative { dx: 1, dy: 2 },
            Command::Wheel {
                amount: 120,
                horizontal: false,
            },
        ] {
            // SAFETY: motion and wheel construct INPUT_MOUSE without sending it.
            assert_eq!(unsafe { input(command).unwrap().Anonymous.mi.dwExtraInfo }, TAG);
        }
        let key = input(Command::Key {
            code: 0x1e,
            scan: true,
            extended: false,
            down: true,
            vk_scan: 0,
        })
        .unwrap();
        // SAFETY: Key constructs INPUT_KEYBOARD without sending it.
        assert_eq!(unsafe { key.Anonymous.ki.dwExtraInfo }, TAG);
        // The ledger stores an Up with the press's tagging, so a desktop-switch
        // or lease cleanup closes an untagged press with an untagged Up.
        for untagged in [false, true] {
            let mut injector = Injector::default();
            for (press, key) in [
                (
                    Command::Button {
                        button: 1,
                        down: true,
                        untagged,
                    },
                    (0, 1),
                ),
                (Command::DragPress { x: 1, y: 2, untagged }, (0, 0)),
            ] {
                // Record only: apply() would attach to a desktop and inject.
                injector.record(press);
                assert_eq!(
                    injector.held.get(&key),
                    Some(&Command::Button {
                        button: key.1,
                        down: false,
                        untagged
                    })
                );
            }
        }
    }
    #[test]
    fn transport_failure_ack_keeps_request_identity_and_fixed_error_stage() {
        for stage in [ErrorStage::Peer, ErrorStage::Liveness, ErrorStage::Decode] {
            let reply = error_reply(denied(), stage, 7, 11);
            assert_eq!(reply.id(), 11);
            assert_eq!(
                u32::from_le_bytes(reply.0[..4].try_into().unwrap()),
                ERROR_ACCESS_DENIED
            );
            assert_eq!(u32::from_le_bytes(reply.0[8..12].try_into().unwrap()), 7);
            assert_eq!(u32::from_le_bytes(reply.0[84..88].try_into().unwrap()), stage as u32);
        }
    }
    #[test]
    fn helper_preserves_position_and_up_in_one_input() {
        let event = input(Command::DragRelease {
            x: 123,
            y: 456,
            untagged: false,
        })
        .unwrap();
        // SAFETY: DragRelease initializes INPUT_MOUSE's mi member.
        let mouse = unsafe { event.Anonymous.mi };
        assert_eq!((mouse.dx, mouse.dy), (123, 456));
        assert_eq!(
            mouse.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_LEFTUP
        );
        assert_eq!(mouse.dwExtraInfo, TAG);
    }
    #[test]
    fn helper_preserves_position_and_down_in_one_input() {
        let event = input(Command::DragPress {
            x: 123,
            y: 456,
            untagged: false,
        })
        .unwrap();
        // SAFETY: DragPress constructs INPUT_MOUSE and initializes its mi member.
        let mouse = unsafe { event.Anonymous.mi };
        assert_eq!((mouse.dx, mouse.dy), (123, 456));
        assert_eq!(
            mouse.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_LEFTDOWN
        );
        assert_eq!(mouse.dwExtraInfo, TAG);
    }
}
