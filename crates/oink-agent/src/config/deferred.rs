//! Only forwarding slider edits defer durability; the retained owner and its
//! effective projection change immediately. Every write still uses atomic_write.
use super::AgentConfig;
use oink_foundation::maintenance::{self, ActivityKind, Lease};
use oink_protocol::v1::{RawValue, dto};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const WRITE_DELAY: Duration = Duration::from_millis(500);
#[derive(Clone, Debug)]
pub(super) struct PendingWrite {
    deadline: Instant,
    // Cloned config candidates share admission until the pending edit is saved.
    _lease: Arc<Lease>,
}
impl AgentConfig {
    pub(crate) fn edit_forwarding_deferred(
        &mut self,
        expected: u64,
        patch: RawValue,
    ) -> Result<dto::DevicePreference, dto::ControlFailure> {
        self.edit_forwarding_at(expected, patch, Instant::now())
    }
    fn edit_forwarding_at(
        &mut self,
        expected: u64,
        patch: RawValue,
        now: Instant,
    ) -> Result<dto::DevicePreference, dto::ControlFailure> {
        let lease = maintenance::process()
            .enter(ActivityKind::Configuration)
            .ok_or(dto::ControlFailure::Busy)?;
        let mut candidate = self.clone();
        let item = candidate
            .owner
            .edit(dto::LocalOption::ForwardingControls, expected, patch)?;
        super::projection::apply(&candidate.owner, &mut candidate.settings)
            .map_err(|_| dto::ControlFailure::InvalidValue)?;
        if item.revision() != Ok(expected) {
            // baseline is the accepted in-memory projection, not a durability
            // promise. This prevents the next ordinary save bumping it twice.
            candidate.baseline = candidate.settings.clone();
            candidate.deferred = Some(PendingWrite {
                deadline: now + WRITE_DELAY,
                _lease: Arc::new(lease),
            });
            *self = candidate;
        }
        Ok(item)
    }
    pub(crate) fn deferred_deadline(&self) -> Option<Instant> {
        self.deferred.as_ref().map(|pending| pending.deadline)
    }
    pub(crate) fn forwarding_persisted(&self) -> bool {
        self.deferred.is_none()
    }
    pub(crate) fn flush_deferred_if_due(&mut self, now: Instant) -> anyhow::Result<bool> {
        if self.deferred_deadline().is_none_or(|deadline| now < deadline) {
            return Ok(false);
        }
        let result = self.flush_deferred();
        if result.is_err()
            && let Some(pending) = &mut self.deferred
        {
            // Retain the accepted state on I/O failure and retry without a busy
            // loop. A later synchronous settings save also drains this snapshot.
            pending.deadline = now + WRITE_DELAY;
        }
        result
    }
    /// Drain an already-admitted edit, including while update admission is shut.
    /// Failure retains dirty state and the last atomic durable file.
    pub(crate) fn flush_deferred(&mut self) -> anyhow::Result<bool> {
        if self.deferred.is_none() {
            return Ok(false);
        }
        let mut candidate = self.clone();
        candidate.persist_admitted()?;
        *self = candidate;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::schema::WireValue;
    fn patch(percent: u16) -> RawValue {
        RawValue::Map([(1, percent.write()), (2, false.write())].into_iter().collect())
    }
    #[test]
    fn burst_changes_memory_immediately_and_only_last_deadline_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config.save().unwrap();
        let durable = std::fs::read(config.settings_path()).unwrap();
        let initial = config.owner().revision(dto::LocalOption::ForwardingControls).unwrap();
        let start = Instant::now();
        let mut writes = 0;
        for index in 0..25u16 {
            let now = start + Duration::from_millis(u64::from(index) * 40);
            let item = config
                .edit_forwarding_at(initial + u64::from(index), patch(40 + index), now)
                .unwrap();
            assert_eq!(item.revision().unwrap(), initial + u64::from(index) + 1);
            assert_eq!(config.settings.audio.forwarding_volume_percent, 40 + index);
            assert!(!config.forwarding_persisted());
            writes += usize::from(config.flush_deferred_if_due(now).unwrap());
            assert_eq!(std::fs::read(config.settings_path()).unwrap(), durable);
        }
        // A crash before the deadline still loads the last complete durable state.
        assert_eq!(
            AgentConfig::load_from(dir.path())
                .unwrap()
                .owner()
                .revision(dto::LocalOption::ForwardingControls),
            Some(initial)
        );
        let deadline = config.deferred_deadline().unwrap();
        assert!(
            !config
                .flush_deferred_if_due(deadline - Duration::from_millis(1))
                .unwrap()
        );
        writes += usize::from(config.flush_deferred_if_due(deadline).unwrap());
        assert_eq!(writes, 1);
        assert!(!config.flush_deferred_if_due(deadline + WRITE_DELAY).unwrap());
        let saved = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(saved.settings.audio.forwarding_volume_percent, 64);
        assert_eq!(
            saved.owner().revision(dto::LocalOption::ForwardingControls),
            Some(initial + 25)
        );
    }
    #[test]
    fn explicit_flush_and_ordinary_save_drain_without_double_revision_bumps() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        let option = dto::LocalOption::ForwardingControls;
        let revision = config.owner().revision(option).unwrap();
        config.edit_forwarding_deferred(revision, patch(70)).unwrap();
        assert!(config.flush_deferred().unwrap());
        assert!(!config.flush_deferred().unwrap());
        assert_eq!(
            AgentConfig::load_from(dir.path())
                .unwrap()
                .settings
                .audio
                .forwarding_volume_percent,
            70
        );
        config.edit_forwarding_deferred(revision + 1, patch(80)).unwrap();
        config.update(|settings| settings.onboarding_done = true).unwrap();
        assert!(config.forwarding_persisted());
        let saved = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(saved.owner().revision(option), Some(revision + 2));
        assert_eq!(saved.settings.audio.forwarding_volume_percent, 80);
        assert!(saved.settings.onboarding_done);
    }
    #[test]
    fn failed_flush_retains_applied_state_and_retries_latest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config.save().unwrap();
        let path = config.settings_path();
        let previous = std::fs::read(&path).unwrap();
        let revision = config.owner().revision(dto::LocalOption::ForwardingControls).unwrap();
        config.edit_forwarding_deferred(revision, patch(75)).unwrap();
        // Simulate an unavailable destination without depending on user privileges.
        std::fs::rename(&path, dir.path().join("last-durable.json")).unwrap();
        std::fs::create_dir(&path).unwrap();
        let deadline = config.deferred_deadline().unwrap();
        assert!(config.flush_deferred_if_due(deadline).is_err());
        assert_eq!(config.settings.audio.forwarding_volume_percent, 75);
        assert!(!config.forwarding_persisted());
        assert_eq!(std::fs::read(dir.path().join("last-durable.json")).unwrap(), previous);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(dir.path().join("last-durable.json"), &path).unwrap();
        assert!(config.flush_deferred_if_due(deadline + WRITE_DELAY).unwrap());
        assert_eq!(
            AgentConfig::load_from(dir.path())
                .unwrap()
                .settings
                .audio
                .forwarding_volume_percent,
            75
        );
    }
}
