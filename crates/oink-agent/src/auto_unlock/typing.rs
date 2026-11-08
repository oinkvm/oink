//! One local password transaction; no password or deferred user key is serialized.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::SyncSender,
};
#[cfg(any(target_os = "macos", test))]
use std::time::{Duration, Instant};
/// The existing injection FIFO holds remote keys while its owner runs this job.
/// Two seconds is below the existing five-second input-backlog safety cutoff.
/// Readiness/wake/Keychain work must finish before starting this bounded section.
#[cfg(any(target_os = "macos", test))]
pub(crate) const MAX_TYPING: Duration = Duration::from_secs(2);
#[cfg(any(target_os = "macos", test))]
pub(crate) struct Typing {
    started: bool,
    deadline: Instant,
}
#[cfg(any(target_os = "macos", test))]
impl Typing {
    pub(crate) fn new(at: Instant) -> Self {
        Self {
            started: false,
            deadline: at + MAX_TYPING,
        }
    }
    pub(crate) fn before_character(
        &mut self,
        now: Instant,
        eligible: bool,
        locked: Option<bool>,
    ) -> Result<(), &'static str> {
        if locked != Some(true) {
            return Err("本机已解锁或锁定状态未知，自动输入已停止");
        }
        if now >= self.deadline {
            return Err("自动输入超过两秒，已停止并释放暂存按键");
        }
        if !eligible {
            return Err(if self.started {
                "自动输入期间状态改变，已停止自动解锁"
            } else {
                "输入开始前焦点或连接状态改变，已取消自动解锁"
            });
        }
        // Focus is intentionally absent from this admission bit: a local
        // password transaction continues after the pointer leaves this Mac.
        // Pause/update/disconnect still revoke it and are checked before every
        // character, while the OS lock guard is checked separately.
        self.started = true;
        Ok(())
    }
}
type Task = Box<dyn FnOnce(Arc<AtomicBool>) -> Result<(), &'static str> + Send>;
struct Inner {
    task: Mutex<Option<Task>>,
    allowed: Arc<AtomicBool>,
    reply: SyncSender<Result<(), &'static str>>,
}
#[derive(Clone)]
pub struct Job(Arc<Inner>);
impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalUnlock([redacted])")
    }
}
impl Job {
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn new(task: Task, reply: SyncSender<Result<(), &'static str>>) -> Self {
        Self(Arc::new(Inner {
            task: Mutex::new(Some(task)),
            allowed: Arc::new(AtomicBool::new(true)),
            reply,
        }))
    }
    pub(crate) fn admission(&self) -> Arc<AtomicBool> {
        self.0.allowed.clone()
    }
    pub(crate) fn reject(&self) {
        self.0.allowed.store(false, Ordering::Release);
        let _ = self.0.reply.try_send(Err("输入开始前状态改变，自动解锁已取消"));
    }
    pub(crate) fn run(&self) {
        let task = self.0.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(task) = task {
            let _ = self.0.reply.try_send(task(self.0.allowed.clone()));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prestart_departure_pause_disconnect_or_unknown_cancels() {
        for locked in [Some(true), None, Some(false)] {
            assert!(
                Typing::new(Instant::now())
                    .before_character(Instant::now(), false, locked)
                    .is_err()
            );
        }
    }
    #[test]
    fn pause_or_disconnect_after_first_character_cancels_local_text() {
        let at = Instant::now();
        let mut t = Typing::new(at);
        assert!(t.before_character(at, true, Some(true)).is_ok());
        assert!(
            t.before_character(at + Duration::from_millis(1), false, Some(true))
                .is_err()
        );
    }
    #[test]
    fn unlock_unknown_and_two_second_deadline_override_finishing_text() {
        let at = Instant::now();
        for locked in [Some(false), None] {
            let mut t = Typing::new(at);
            t.before_character(at, true, Some(true)).unwrap();
            assert!(t.before_character(at, false, locked).is_err());
        }
        let mut t = Typing::new(at);
        t.before_character(at, true, Some(true)).unwrap();
        assert!(t.before_character(at + MAX_TYPING, true, Some(true)).is_err());
    }
}
