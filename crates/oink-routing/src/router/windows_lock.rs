//! Windows-only explicit lock action; ordinary Win+L remains a key stream.
use super::*;
use crate::control::WindowsLockRequest;
use oink_foundation::session::Boot;
use std::sync::atomic::{AtomicBool, Ordering};

/// One-shot authority shared with the ordered native worker. Focus/session changes
/// revoke queued work; consuming it never means the asynchronous OS lock completed.
#[derive(Clone, Debug)]
pub struct WindowsLockPermit(Arc<AtomicBool>);
impl PartialEq for WindowsLockPermit {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl WindowsLockPermit {
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}
struct Pending {
    request: WindowsLockRequest,
    permit: WindowsLockPermit,
}
#[derive(Default)]
pub(super) struct State {
    pub(super) capable: HashSet<DeviceId>,
    next_id: u64,
    pub(super) seen: HashMap<Boot, u64>,
    pending: Vec<Pending>,
}
impl Router {
    pub(super) fn native_key_event(&self, origin: DeviceId, code: KeyCode, pressed: bool) -> InjectEvent {
        // Classify the original source, not the last network hop or pointer holder.
        // No menu mask is added: the ordinary L remains between Win DOWN and UP.
        // Pending source OS facts must not make an unknown Windows origin look
        // like a Mac. Only known Mac remote input requests the compensation.
        let remote_non_mac = origin != self.me() && self.peers.get(&origin).is_none_or(|p| p.os != Os::MacOS);
        if self.cfg.os == Os::Windows && remote_non_mac && code == KeyCode::keyboard(0x0f) {
            InjectEvent::KeyWithoutLock { code, pressed }
        } else {
            InjectEvent::Key { code, pressed }
        }
    }

    pub fn set_peer_windows_lock(&mut self, peer: DeviceId, capable: bool) {
        if capable {
            self.windows_lock.capable.insert(peer);
        } else {
            self.windows_lock.capable.remove(&peer);
        }
        self.refresh_windows_locks();
    }
    pub(super) fn windows_lock_target(&self) -> Option<Boot> {
        if self.cfg.os != Os::Windows
            || self.paused
            || !self.capture_available
            || self.keyboard_local()
            || self.holder == self.me()
            || !self.input_ready(self.holder)
            || self.claim != self.holder
            || !self.scope_allows(self.focus_scope.as_ref(), self.holder)
            || !self.windows_lock.capable.contains(&self.holder)
            || self.peers.get(&self.holder).is_none_or(|p| p.os != Os::Windows)
        {
            return None;
        }
        self.admitted_boots.get(&self.holder).copied()
    }
    pub(super) fn request_windows_lock(&mut self, target: Boot) {
        // Only this source's chord contribution is retired. Other sources retain
        // their mapped modifier references; the lock API does not inspect them.
        let modifiers: Vec<_> = self.keys.keys().copied().filter(|k| k.is_modifier()).collect();
        for code in modifiers {
            if let Some(targets) = self.keys.remove(&code) {
                if self.release_local_key(code, targets) {
                    // A modifier pressed before leaving this source may have a
                    // native DOWN as well as remote contributions. Retire it now:
                    // suppression will swallow the eventual physical UP too.
                    self.out.push(Output::Inject(InjectEvent::Key { code, pressed: false }));
                }
                self.keyboard.suppress(code);
            }
        }
        self.windows_lock.next_id += 1;
        self.send(
            target.device_id,
            Message::Control(ControlMsg::LockRemoteWindows(WindowsLockRequest {
                source: self.cfg.boot,
                target,
                epoch: self.epoch,
                id: self.windows_lock.next_id,
            })),
        );
    }
    fn valid_windows_lock(&self, request: &WindowsLockRequest) -> bool {
        self.cfg.os == Os::Windows
            && request.target == self.cfg.boot
            && request.id != 0
            && self.holder == self.me()
            && self.claim == self.me()
            && self.epoch != 0
            && self.epoch == request.epoch
            && !self.paused
            && self.capture_available
            && self.input_ready(request.source.device_id)
            && self.admitted_boots.get(&request.source.device_id) == Some(&request.source)
            && self.windows_lock.capable.contains(&request.source.device_id)
            && self
                .peers
                .get(&request.source.device_id)
                .is_some_and(|p| p.os == Os::Windows)
            && self.scope_allows(self.focus_scope.as_ref(), request.source.device_id)
    }
    pub(super) fn receive_windows_lock(&mut self, from: DeviceId, request: WindowsLockRequest) {
        if from != request.source.device_id
            || !self.valid_windows_lock(&request)
            || self
                .windows_lock
                .seen
                .get(&request.source)
                .is_some_and(|id| *id >= request.id)
        {
            return;
        }
        self.refresh_windows_locks();
        // The I0 reader already bounds native work; keep router-side pending authority
        // bounded too if the worker is stalled. Never evict authority into execution.
        if self.windows_lock.pending.len() >= 64 {
            return;
        }
        self.windows_lock.seen.insert(request.source, request.id);
        let permit = WindowsLockPermit(Arc::new(AtomicBool::new(true)));
        self.windows_lock.pending.push(Pending {
            request,
            permit: permit.clone(),
        });
        self.out.push(Output::LockWindows(permit));
    }
    pub(super) fn refresh_windows_locks(&mut self) {
        let pending = std::mem::take(&mut self.windows_lock.pending);
        for entry in pending {
            if !self.valid_windows_lock(&entry.request) {
                entry.permit.0.store(false, Ordering::Release);
            } else if entry.permit.0.load(Ordering::Acquire) {
                self.windows_lock.pending.push(entry);
            }
        }
        self.windows_lock
            .seen
            .retain(|boot, _| self.admitted_boots.get(&boot.device_id) == Some(boot));
    }
}
impl Drop for State {
    fn drop(&mut self) {
        for entry in &self.pending {
            entry.permit.0.store(false, Ordering::Release);
        }
    }
}
