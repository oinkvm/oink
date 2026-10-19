//! UTC is sampled at capture, never extrapolated from a suspend-dependent monotonic clock.
use super::*;
pub(super) const EPOCH_TOLERANCE_US: u64 = 100_000;

pub(super) trait Source: Send + Sync {
    fn read(&self) -> (u64, i64);
}
struct SystemSource(Instant);
impl Source for SystemSource {
    fn read(&self) -> (u64, i64) {
        (self.0.elapsed().as_micros() as u64, utc_us())
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Stamp {
    pub mono_us: u64,
    pub utc_us: i64,
    pub clock_id: i64,
}
pub(super) struct Clock {
    source: Arc<dyn Source>,
    correlation: arc_swap::ArcSwap<Correlation>,
}
struct Correlation {
    basis: i64,
    id: i64,
}
impl Clock {
    pub fn system() -> Arc<Self> {
        Self::new(Arc::new(SystemSource(Instant::now())))
    }
    pub fn new(source: Arc<dyn Source>) -> Arc<Self> {
        let (mono, wall) = source.read();
        let initial = wall.saturating_sub(mono as i64);
        Arc::new(Self {
            source,
            correlation: arc_swap::ArcSwap::from_pointee(Correlation { basis: initial, id: 0 }),
        })
    }
    pub fn now(&self) -> Stamp {
        let (mono_us, utc_us) = self.source.read();
        let observed = utc_us.saturating_sub(mono_us as i64);
        let mut correlation = self.correlation.load();
        // A clock epoch scopes offset samples, not event timestamps. macOS Instant can stop
        // during suspend; wall time also steps under NTP/manual changes. Do not repair UTC
        // with an anchor or clamp it: retain the actual wall reading and original monotonic
        // time. 100 ms hysteresis bounds normal sampling jitter; each transition receives a distinct
        // epoch. Offset uncertainty includes this tolerance on both clocks. This uses
        // portable clock semantics, not a platform-specific workaround.
        if observed.abs_diff(correlation.basis) > EPOCH_TOLERANCE_US {
            let next = Arc::new(Correlation {
                basis: observed,
                id: correlation.id.saturating_add(1),
            });
            let previous = self.correlation.compare_and_swap(&*correlation, next.clone());
            correlation = if Arc::ptr_eq(&previous, &correlation) {
                arc_swap::Guard::from_inner(next)
            } else {
                previous
            };
        }
        Stamp {
            mono_us,
            utc_us,
            // A concurrent clock transition may win the CAS with a different basis.
            // Keep the actual timestamp, but do not label it with that unrelated epoch.
            // -1 is explicitly uncalibratable; the capture path never spins or waits.
            clock_id: if observed.abs_diff(correlation.basis) <= EPOCH_TOLERANCE_US {
                correlation.id
            } else {
                -1
            },
        }
    }
}
