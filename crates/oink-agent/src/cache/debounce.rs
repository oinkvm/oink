//! Usage totals may lag: aggregate mutations instead of scanning each catalog write.
use std::time::Duration;
use tokio::time::Instant;

const QUIET: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}
impl Debounce {
    pub(super) fn changed(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }
    fn deadline(&self) -> Option<Instant> {
        Some((self.first? + MAX_INTERVAL).min(self.last? + QUIET))
    }
    pub(super) async fn wait(&self) {
        match self.deadline() {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }
    pub(super) fn scanned(&mut self) {
        self.first = None;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn catalog_burst_schedules_one_scan_after_quiet_period() {
        let mut changes = Debounce::default();
        for _ in 0..100 {
            changes.changed(Instant::now());
            tokio::time::advance(Duration::from_millis(10)).await;
        }
        let due = changes.deadline().unwrap();
        changes.wait().await;
        assert_eq!(Instant::now(), due);
        changes.scanned();
        assert!(changes.deadline().is_none());
        assert!(
            tokio::time::timeout(Duration::from_secs(20), changes.wait())
                .await
                .is_err()
        );
    }
    #[tokio::test(start_paused = true)]
    async fn continuous_catalog_mutations_cannot_postpone_usage_indefinitely() {
        let mut changes = Debounce::default();
        let start = Instant::now();
        for _ in 0..15 {
            changes.changed(Instant::now());
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(changes.deadline(), Some(start + MAX_INTERVAL));
        changes.wait().await;
        assert_eq!(Instant::now(), start + MAX_INTERVAL);
    }
}
