//! Local usage-day statistics: the first day this feature ran on the device and,
//! per shared feature, the local calendar days on which it was used. Whether a
//! day had use is all that is kept: no counts, times, peers or content.
//!
//! Hot paths call [`mark`]: once a feature is recorded for the day it costs one
//! relaxed load. Only the first mark of a day sets a pending bit and unparks the
//! recorder thread, which dates it at that moment and persists the store. The
//! recorder parks until the next local midnight, recomputed after every wake so
//! zone and DST changes apply, and then re-arms every feature. A parked timeout
//! does not advance while the machine sleeps (mach uptime on macOS, unbiased
//! interrupt time on Windows), so a system resume also wakes it.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;
use time::{Date, OffsetDateTime, UtcOffset};

const FILE: &str = "usage-days.json";
const SCHEMA: u32 = 1;
const FEATURES: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Feature {
    /// Local input forwarded to a remote device, or remote input injected here.
    KeyboardMouse,
    /// A native paste here read clipboard content that came from another device.
    Paste,
    /// A cross-device drag ended in a drop (target), or a remote drop was reported (source).
    Drag,
    /// Non-silent shared speaker audio was sent or played.
    Speaker,
    /// A shared microphone was requested here, or this microphone serves a request.
    Microphone,
}
const ALL: [Feature; FEATURES] = [
    Feature::KeyboardMouse,
    Feature::Paste,
    Feature::Drag,
    Feature::Speaker,
    Feature::Microphone,
];

struct Marks {
    seen: [AtomicBool; FEATURES],
    pending: AtomicU8,
}
impl Marks {
    const fn new() -> Self {
        Self {
            seen: [const { AtomicBool::new(false) }; FEATURES],
            pending: AtomicU8::new(0),
        }
    }
    /// True only for the first mark since the last re-arm.
    #[inline]
    fn mark(&self, feature: Feature) -> bool {
        let seen = &self.seen[feature as usize];
        !seen.load(Ordering::Relaxed) && !seen.swap(true, Ordering::AcqRel) && {
            self.pending.fetch_or(1 << feature as u8, Ordering::AcqRel);
            true
        }
    }
    fn take(&self) -> u8 {
        self.pending.swap(0, Ordering::AcqRel)
    }
    fn rearm(&self) {
        for seen in &self.seen {
            seen.store(false, Ordering::Release);
        }
    }
}

static MARKS: Marks = Marks::new();
static RECORDER: OnceLock<std::thread::Thread> = OnceLock::new();

/// Record that `feature` was used today. Never blocks or allocates.
#[inline]
pub(crate) fn mark(feature: Feature) {
    #[cfg(test)]
    CAPTURED.with(|captured| {
        let mut counts = captured.get();
        counts[feature as usize] += 1;
        captured.set(counts);
    });
    if MARKS.mark(feature) {
        wake();
    }
}

/// The machine resumed from sleep: re-date now instead of at the stale deadline.
pub(crate) fn system_resumed() {
    wake();
}

#[cold]
fn wake() {
    if let Some(recorder) = RECORDER.get() {
        recorder.unpark();
    }
}

/// Start the process-wide recorder for the user backend. Marks made earlier are
/// kept pending and recorded on its first turn.
pub(crate) fn start(data_dir: &Path) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    oink_clipboard::content::native_read::set_paste_observer(|| mark(Feature::Paste));
    let path = data_dir.join(FILE);
    if let Err(error) = std::thread::Builder::new()
        .name("oink-usage-days".into())
        .spawn(move || run(&path))
    {
        tracing::warn!(%error, "usage-day recorder unavailable");
    }
}

fn run(path: &Path) {
    let _ = RECORDER.set(std::thread::current());
    let Some(mut now) = local_now() else { return };
    let mut store = match load_or_create(path, now.date()) {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(%error, "usage-day statistics unavailable");
            return;
        }
    };
    let mut day = now.date();
    loop {
        if now.date() != day {
            day = now.date();
            MARKS.rearm();
        }
        if store.record(day, MARKS.take())
            && let Err(error) = save(path, &store)
        {
            tracing::warn!(%error, "usage-day statistics not saved");
        }
        std::thread::park_timeout(until_midnight(now, |at| UtcOffset::local_offset_at(at).ok()));
        let Some(next) = local_now() else { return };
        now = next;
    }
}

fn local_now() -> Option<OffsetDateTime> {
    // Picks up a changed system zone where `time` knows tzset is sound (macOS);
    // Windows reads the active zone on every conversion.
    let _ = time::util::refresh_tz();
    OffsetDateTime::now_local()
        .inspect_err(|error| tracing::warn!(%error, "local time unavailable; usage-day recorder stopped"))
        .ok()
}

/// Time from `now` until the next local midnight. `offset_at` gives the zone's
/// offset at an instant, so a DST change before midnight is honoured. Where a
/// change skips midnight itself, that midnight is already behind: wake at the
/// change, when the local date has moved on.
fn until_midnight(now: OffsetDateTime, offset_at: impl Fn(OffsetDateTime) -> Option<UtcOffset>) -> Duration {
    let Some(next) = now.date().next_day() else {
        return Duration::MAX;
    };
    let current = next.midnight().assume_offset(now.offset());
    let at = offset_at(current)
        .map(|offset| next.midnight().assume_offset(offset))
        .filter(|at| *at > now)
        .unwrap_or(current);
    Duration::try_from(at - now).unwrap_or(Duration::ZERO)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Day(Date);
const DAY_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]");
impl Serialize for Day {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.format(DAY_FORMAT).map_err(serde::ser::Error::custom)?)
    }
}
impl<'de> Deserialize<'de> for Day {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Date::parse(&text, DAY_FORMAT)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Days {
    keyboard_mouse: BTreeSet<Day>,
    paste: BTreeSet<Day>,
    drag: BTreeSet<Day>,
    speaker: BTreeSet<Day>,
    microphone: BTreeSet<Day>,
}
impl Days {
    fn of(&mut self, feature: Feature) -> &mut BTreeSet<Day> {
        match feature {
            Feature::KeyboardMouse => &mut self.keyboard_mouse,
            Feature::Paste => &mut self.paste,
            Feature::Drag => &mut self.drag,
            Feature::Speaker => &mut self.speaker,
            Feature::Microphone => &mut self.microphone,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Store {
    schema: u32,
    installed_on: Day,
    days: Days,
}
impl Store {
    fn new(today: Date) -> Self {
        Self {
            schema: SCHEMA,
            installed_on: Day(today),
            days: Days::default(),
        }
    }
    /// Add `day` to every feature in `pending`; true when the store changed.
    fn record(&mut self, day: Date, pending: u8) -> bool {
        ALL.into_iter()
            .filter(|feature| pending & (1 << *feature as u8) != 0)
            .fold(false, |changed, feature| {
                self.days.of(feature).insert(Day(day)) | changed
            })
    }
}

/// An unreadable store is reported and left untouched: rewriting it would lose
/// `installed_on`, which is set once when the store is first created.
fn load_or_create(path: &Path, today: Date) -> io::Result<Store> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let store: Store = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if store.schema != SCHEMA {
                return Err(io::Error::other(format!(
                    "unsupported usage-day schema {}",
                    store.schema
                )));
            }
            Ok(store)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let store = Store::new(today);
            save(path, &store)?;
            Ok(store)
        }
        Err(error) => Err(error),
    }
}

fn save(path: &Path, store: &Store) -> io::Result<()> {
    oink_foundation::storage::atomic_write(path, &serde_json::to_vec_pretty(store)?)
}

#[cfg(test)]
thread_local! {
    static CAPTURED: std::cell::Cell<[u32; FEATURES]> = const { std::cell::Cell::new([0; FEATURES]) };
}
/// Every [`mark`] call made by `work` on this thread, per feature, before deduplication.
#[cfg(test)]
pub(crate) fn captured<T>(work: impl FnOnce() -> T) -> Vec<Feature> {
    let before = CAPTURED.get();
    let _ = work();
    let after = CAPTURED.get();
    ALL.into_iter()
        .filter(|feature| after[*feature as usize] > before[*feature as usize])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime, offset};

    #[test]
    fn only_the_first_mark_per_feature_reaches_the_recorder_until_rearmed() {
        let marks = Marks::new();
        assert!(marks.mark(Feature::Paste));
        assert!(!marks.mark(Feature::Paste));
        assert!(marks.mark(Feature::Drag));
        assert_eq!(marks.take(), (1 << Feature::Paste as u8) | (1 << Feature::Drag as u8));
        assert_eq!(marks.take(), 0);
        assert!(!marks.mark(Feature::Paste), "recorded today stays a single load");
        assert_eq!(marks.take(), 0);
        marks.rearm();
        assert!(marks.mark(Feature::Paste));
        assert_eq!(marks.take(), 1 << Feature::Paste as u8);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    static SHARED: Marks = Marks::new();
                    (0..1000).filter(|_| SHARED.mark(Feature::Speaker)).count()
                })
            })
            .collect();
        assert_eq!(threads.into_iter().map(|t| t.join().unwrap()).sum::<usize>(), 1);
    }

    #[test]
    fn store_merges_unique_sorted_days_and_keeps_the_install_date() {
        let mut store = Store::new(date!(2026 - 11 - 10));
        let all = ALL.iter().fold(0, |bits, feature| bits | (1 << *feature as u8));
        assert!(store.record(date!(2026 - 11 - 12), 1 << Feature::Drag as u8));
        assert!(store.record(date!(2026 - 11 - 11), all));
        assert!(!store.record(date!(2026 - 11 - 12), 1 << Feature::Drag as u8));
        assert!(!store.record(date!(2026 - 11 - 13), 0));
        let json: serde_json::Value = serde_json::to_value(&store).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "schema": 1,
                "installed_on": "2026-11-10",
                "days": {
                    "keyboard_mouse": ["2026-11-11"],
                    "paste": ["2026-11-11"],
                    "drag": ["2026-11-11", "2026-11-12"],
                    "speaker": ["2026-11-11"],
                    "microphone": ["2026-11-11"],
                }
            })
        );
    }

    #[test]
    fn store_round_trips_and_install_date_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut store = load_or_create(&path, date!(2026 - 11 - 10)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Store>(&std::fs::read(&path).unwrap()).unwrap(),
            store
        );
        store.record(date!(2026 - 11 - 11), 1 << Feature::Microphone as u8);
        save(&path, &store).unwrap();
        let loaded = load_or_create(&path, date!(2027 - 1 - 1)).unwrap();
        assert_eq!(loaded, store);
        assert_eq!(loaded.installed_on, Day(date!(2026 - 11 - 10)));
        std::fs::write(&path, b"{\"schema\":1,").unwrap();
        assert!(load_or_create(&path, date!(2027 - 1 - 1)).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"schema\":1,",
            "an unreadable store is never replaced"
        );
        std::fs::write(&path, br#"{"schema":2,"installed_on":"2026-11-10","days":{}}"#).unwrap();
        assert!(load_or_create(&path, date!(2027 - 1 - 1)).is_err());
        std::fs::write(&path, br#"{"schema":1,"installed_on":"2026-11-1","days":{}}"#).unwrap();
        assert!(load_or_create(&path, date!(2027 - 1 - 1)).is_err());
    }

    #[test]
    fn next_midnight_follows_fixed_offsets_and_dst() {
        let fixed = |_| Some(offset!(+8));
        assert_eq!(
            until_midnight(datetime!(2026-11-10 23:59:30 +8), fixed),
            Duration::from_secs(30)
        );
        assert_eq!(
            until_midnight(datetime!(2026-12-31 00:00:00 +8), fixed),
            Duration::from_secs(86_400)
        );
        assert_eq!(
            until_midnight(datetime!(2026-11-10 12:00:00 -5), |_| Some(offset!(-5))),
            Duration::from_secs(12 * 3600)
        );
        // Spring forward at 02:00 on the same day: midnight is under the new offset.
        assert_eq!(
            until_midnight(datetime!(2026-03-29 01:00:00 +1), |_| Some(offset!(+2))),
            Duration::from_secs(22 * 3600)
        );
        // Fall back at 03:00: one more hour until midnight.
        assert_eq!(
            until_midnight(datetime!(2026-10-25 01:00:00 +2), |_| Some(offset!(+1))),
            Duration::from_secs(24 * 3600)
        );
        // A zone that skips midnight (00:00 -> 01:00) wakes at the transition,
        // when the local date has already changed, not after a spin.
        assert_eq!(
            until_midnight(datetime!(2026-09-05 23:30:00 -4), |_| Some(offset!(-3))),
            Duration::from_secs(30 * 60)
        );
        // A zone that repeats the last hour (00:00 -> 23:00) waits for the new date.
        assert_eq!(
            until_midnight(datetime!(2026-04-04 23:30:00 -3), |_| Some(offset!(-4))),
            Duration::from_secs(90 * 60)
        );
        assert_eq!(
            until_midnight(datetime!(2026-04-04 23:30:00 -4), |_| Some(offset!(-4))),
            Duration::from_secs(30 * 60)
        );
    }
}
