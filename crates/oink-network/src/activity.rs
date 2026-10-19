//! Local observations only: physical user activity comes from the host's existing
//! group broadcast, audio from endpoint media. None of these observations is sent.
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

pub(crate) const HOLD: Duration = Duration::from_secs(3);
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    Local,
    Group,
    Audio,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Status {
    pub local: bool,
    pub group: bool,
    pub audio: bool,
    pub expires: Option<Instant>,
}
impl Status {
    pub fn active(self) -> bool {
        self.local || self.group || self.audio
    }
    #[cfg(test)]
    pub fn reasons(self) -> (bool, bool, bool) {
        (self.local, self.group, self.audio)
    }
}
#[derive(Debug)]
pub(crate) struct Activity {
    epoch: Instant,
    expires: [AtomicU64; 3],
    pub changed: Notify,
}
impl Default for Activity {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            expires: std::array::from_fn(|_| AtomicU64::new(0)),
            changed: Notify::new(),
        }
    }
}
impl Activity {
    fn elapsed(&self) -> u64 {
        Instant::now()
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .min(u64::MAX as u128) as u64
    }
    pub fn status(&self) -> Status {
        let now = self.elapsed();
        let expires = self.expires.each_ref().map(|e| e.load(Ordering::Acquire));
        Status {
            local: expires[0] > now,
            group: expires[1] > now,
            audio: expires[2] > now,
            expires: expires
                .into_iter()
                .filter(|e| *e > now)
                .min()
                .map(|e| self.epoch + Duration::from_nanos(e)),
        }
    }
    pub fn note(&self, kind: Kind) {
        let now = self.elapsed();
        let index = match kind {
            Kind::Local => 0,
            Kind::Group => 1,
            Kind::Audio => 2,
        };
        let before = self.expires[index].fetch_max(now.saturating_add(HOLD.as_nanos() as u64), Ordering::AcqRel);
        if before <= now {
            self.changed.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn exact_boundary_and_flapping() {
        let a = Activity::default();
        assert!(!a.status().active());
        a.note(Kind::Local);
        tokio::time::advance(HOLD - Duration::from_millis(1)).await;
        assert!(a.status().local);
        a.note(Kind::Local);
        tokio::time::advance(HOLD - Duration::from_millis(1)).await;
        assert!(a.status().local);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!a.status().active());
    }
    #[tokio::test(start_paused = true)]
    async fn one_second_broadcasts_missing_pulses_and_first_after_idle() {
        let a = Activity::default();
        a.note(Kind::Group);
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(a.status().group);
            a.note(Kind::Group);
        }
        // Missing two one-second broadcasts does not expire before the exact
        // third-second boundary. A late/first new broadcast immediately resumes.
        tokio::time::advance(Duration::from_millis(2999)).await;
        assert!(a.status().group);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!a.status().active());
        a.note(Kind::Group);
        assert!(a.status().group);
        // A disappearing source requires no removal callback and cannot latch on.
        tokio::time::advance(HOLD).await;
        assert!(!a.status().active());
    }
    #[tokio::test(start_paused = true)]
    async fn last_sound_expires_at_exactly_three_seconds() {
        let activity = Activity::default();
        activity.note(Kind::Audio);
        tokio::time::advance(Duration::from_millis(10)).await;
        activity.note(Kind::Audio);
        tokio::time::advance(Duration::from_millis(2999)).await;
        assert!(activity.status().audio);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!activity.status().active());
    }
    #[tokio::test(start_paused = true)]
    async fn own_group_and_audio_expire_independently() {
        let a = Activity::default();
        a.note(Kind::Local);
        tokio::time::advance(Duration::from_secs(1)).await;
        a.note(Kind::Group);
        tokio::time::advance(Duration::from_secs(1)).await;
        a.note(Kind::Audio);
        assert_eq!(a.status().reasons(), (true, true, true));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(a.status().reasons(), (false, true, true));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(a.status().reasons(), (false, false, true));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!a.status().active());
    }
}
