//! Numeric sampling only; reservoir candidate counts and random phase never leave disk.
use crate::*;
use serde::{Deserialize, Serialize};

pub fn random_below(n: u64) -> Result<u64> {
    if n == 0 {
        return Err(Error::Invalid);
    }
    // Rejection sampling avoids modulo bias, including the reservoir's early slots.
    let threshold = n.wrapping_neg() % n;
    loop {
        let mut b = [0; 8];
        getrandom::fill(&mut b).map_err(|_| Error::Entropy)?;
        let v = u64::from_le_bytes(b);
        if v >= threshold {
            return Ok(v % n);
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservoir {
    pub(crate) seen: u64,
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) keys: Vec<MeasurementContext>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    pub report: Report,
    pub at: u64,
}
impl Reservoir {
    pub(crate) fn offer(&mut self, report: Report, at: u64, slots: usize) -> Result<()> {
        let key = report.context().ok_or(Error::Invalid)?;
        if !self.keys.contains(&key) {
            if self.keys.len() >= 16 {
                return Err(Error::Limit);
            }
            self.keys.push(key);
        }
        self.seen = self.seen.checked_add(1).ok_or(Error::Limit)?;
        let candidate = Candidate { report, at };
        if self.candidates.len() < slots {
            self.candidates.push(candidate);
        } else {
            let index = random_below(self.seen)? as usize;
            if index < slots {
                self.candidates[index] = candidate;
            }
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct InputGate {
    phase: u8,
}
impl InputGate {
    pub fn new() -> Result<Self> {
        Ok(Self {
            phase: random_below(100)? as u8,
        })
    }
    pub fn select(&mut self) -> bool {
        self.phase = (self.phase + 1) % 100;
        self.phase == 0
    }
}
pub const DURATION_BOUNDS: [u64; 13] = [
    1000, 2000, 5000, 10000, 20000, 50000, 100000, 200000, 500000, 1000000, 2000000, 5000000, 10000000,
];
pub fn singleton(value_us: u64) -> [u32; 14] {
    let mut bins = [0; 14];
    bins[DURATION_BOUNDS.iter().position(|b| value_us <= *b).unwrap_or(13)] = 1;
    bins
}
pub fn jitter_bins(count: u64, late: [u64; 5]) -> Result<([u32; 14], u32, bool)> {
    let mut previous = count;
    let mut bins = [0; 14];
    let mut saturated = false;
    for (i, next) in late.into_iter().chain([0]).enumerate() {
        if next > previous {
            return Err(Error::Invalid);
        }
        let n = previous - next;
        saturated |= n > u32::MAX as u64;
        bins[i] = n.min(u32::MAX as u64) as u32;
        previous = next;
    }
    let total = bins.iter().fold(0u32, |a, b| a.saturating_add(*b));
    Ok((bins, total, saturated || count > u32::MAX as u64))
}
/// Bounded, process-local pairing. Tokens and Instants are never serializable.
#[derive(Debug, Default)]
pub struct Timers {
    next: u64,
    entries: Vec<(u64, std::time::Instant)>,
}
impl Timers {
    pub fn begin(&mut self, now: std::time::Instant) -> Option<u64> {
        self.entries
            .retain(|(_, at)| now.saturating_duration_since(*at).as_secs() < 120);
        if self.entries.len() >= 128 {
            return None;
        }
        self.next = self.next.checked_add(1)?;
        self.entries.push((self.next, now));
        Some(self.next)
    }
    pub fn finish(&mut self, token: u64, now: std::time::Instant) -> Option<u64> {
        let i = self.entries.iter().position(|(id, _)| *id == token)?;
        let (_, at) = self.entries.swap_remove(i);
        let us = now.checked_duration_since(at)?.as_micros();
        (us <= 120_000_000).then_some(us as u64)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_histograms_are_non_cumulative_and_saturate() {
        assert_eq!(singleton(u64::MAX)[13], 1);
        assert_eq!(singleton(1000)[0], 1);
        let (bins, count, saturated) = jitter_bins(100, [50, 40, 20, 10, 5]).unwrap();
        assert_eq!(&bins[..6], &[50, 10, 20, 10, 5, 5]);
        assert_eq!(count, 100);
        assert!(!saturated);
        assert!(jitter_bins(1, [2, 0, 0, 0, 0]).is_err());
        assert!(jitter_bins(u64::MAX, [0; 5]).unwrap().2);
    }
    #[test]
    fn input_phase_and_transient_timers_are_bounded() {
        let mut gate = InputGate::new().unwrap();
        assert_eq!((0..10000).filter(|_| gate.select()).count(), 100);
        let mut timers = Timers::default();
        let at = std::time::Instant::now();
        let first = timers.begin(at).unwrap();
        for _ in 1..128 {
            assert!(timers.begin(at).is_some());
        }
        assert!(timers.begin(at).is_none());
        assert_eq!(
            timers.finish(first, at + std::time::Duration::from_micros(42)),
            Some(42)
        );
        assert_eq!(timers.finish(first, at), None);
        assert!(timers.begin(at + std::time::Duration::from_secs(121)).is_some());
    }
}
