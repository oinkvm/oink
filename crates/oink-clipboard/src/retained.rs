//! Copied bitmaps and rich text kept for phones (docs/design/clipboard.md
//! 「手机的图片与富文本保留」).
//!
//! A deliberate exception to the one-shot source lifetime: the bitmap or rich
//! text a phone shows for a copy made on this PC is materialized here when
//! its offer is announced to a group with a phone (or when a phone joins
//! while that offer is current), so the phone can still fetch it, or a
//! thumbnail of it, by the original offer identity after the native
//! clipboard has moved on. Entries live 24 hours by wall-clock creation time
//! (the store survives restarts) and are removed by `sweep`, on expiry when
//! read, by "清理缓存", and oldest first when the store grows past
//! `MAX_BYTES`. Thumbnails of any image an offer names (retained bitmap,
//! live bitmap or namespace file) are cached in the same entry and expire
//! with it.
use crate::{
    cache::{CacheCategory, CacheSize, SourceLease},
    image_file::Thumbnail,
    model::{OfferId, REPRESENTATION_BYTES},
};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Directory below the agent cache root.
pub const DIR: &str = "clipboard-images";
/// How long a copied bitmap or rich text (and any cached thumbnail) is kept.
pub const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// The store's size bound. A day of ordinary copying (hundreds of
/// screenshots of a few MB) stays well below it; a handful of huge bitmaps
/// (up to `REPRESENTATION_BYTES` each) cannot fill the disk. Beyond it the
/// oldest entries go first, exactly as if their 24 hours were over.
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const CREATED: &str = "created";

/// What a phone asks for: an inventory bitmap slot or a namespace file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Representation { item: u32, representation: u32 },
    File { node: u64, version: u64 },
}
impl Target {
    fn key(self) -> String {
        match self {
            Self::Representation { item, representation } => format!("r-{item}-{representation}"),
            Self::File { node, version } => format!("f-{node}-{version}"),
        }
    }
}

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

#[derive(Clone)]
pub struct RetainedClips {
    root: PathBuf,
    now: Clock,
    max_bytes: u64,
    window_owned: bool,
}
impl std::fmt::Debug for RetainedClips {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedClips").field("root", &self.root).finish()
    }
}

/// A retained bitmap being read; holds "清理缓存" off its file meanwhile.
pub struct RetainedReader {
    file: fs::File,
    _lease: SourceLease,
}
impl std::fmt::Debug for RetainedReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetainedReader")
    }
}
impl Read for RetainedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ThumbnailMeta {
    mime: String,
    width: u32,
    height: u32,
    source_bytes: u64,
}

impl RetainedClips {
    /// The store below `cache_dir` (the agent cache root).
    pub fn new(cache_dir: &Path) -> Self {
        Self::with_clock(cache_dir.join(DIR), Arc::new(SystemTime::now))
    }

    /// Tests inject the wall clock to cross the 24-hour boundary.
    pub fn with_clock(root: PathBuf, now: Clock) -> Self {
        Self {
            root,
            now,
            max_bytes: MAX_BYTES,
            window_owned: false,
        }
    }

    #[cfg(test)]
    fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// History owns lifetime and its shared byte budget, including proxy files.
    pub fn history(cache_dir: &Path) -> Self {
        let mut store = Self::new(cache_dir);
        store.window_owned = true;
        store
    }
    pub fn bytes(&self, offer: OfferId) -> u64 {
        crate::history::data_bytes(&self.offer_dir(offer)).unwrap_or(0)
    }
    pub fn forget(&self, offer: OfferId) -> io::Result<bool> {
        let _mutation = crate::cache::Mutation(&self.root);
        let path = self.offer_dir(offer);
        if !path.exists() {
            return Ok(true);
        }
        crate::cache::remove_unless_active(&path)
    }
    pub(crate) fn forget_sensitive(&self, offer: OfferId) -> io::Result<bool> {
        // Sensitive readers are revoked by the history guard before unlinking.
        // Unlike ordinary cap eviction, do not retain a named cache directory
        // for an active reader. This tree contains only Oink-owned cache bytes.
        let path = self.offer_dir(offer);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        Ok(true)
    }
    pub(crate) fn retain_offers(&self, offers: &[OfferId]) {
        let keep: std::collections::HashSet<_> = offers.iter().map(|id| self.offer_dir(*id)).collect();
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                if !keep.contains(&entry.path()) {
                    let _ = crate::cache::remove_unless_active(&entry.path());
                }
            }
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn now_ms(&self) -> u64 {
        (self.now)()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    }

    fn offer_dir(&self, offer: OfferId) -> PathBuf {
        let origin: String = offer.clip.origin.0.iter().map(|byte| format!("{byte:02x}")).collect();
        self.root
            .join(format!("{origin}-{}-{}", offer.clip.seq, offer.generation))
    }

    /// An entry is live from its creation for `RETENTION`. A creation time
    /// more than `RETENTION` in the future (the wall clock was set back)
    /// also ends it, so a clock change cannot keep data indefinitely.
    fn expired(&self, created_ms: u64) -> bool {
        if self.window_owned {
            return false;
        }
        let now = self.now_ms();
        let retention = RETENTION.as_millis() as u64;
        now.saturating_sub(created_ms) >= retention || created_ms.saturating_sub(now) >= retention
    }

    fn created(dir: &Path) -> Option<u64> {
        let bytes = fs::read(dir.join(CREATED)).ok()?;
        Some(u64::from_le_bytes(bytes.as_slice().try_into().ok()?))
    }

    /// The entry directory when it exists and is live. An expired entry is
    /// removed on the spot.
    fn live(&self, offer: OfferId) -> Option<PathBuf> {
        let dir = self.offer_dir(offer);
        let created = Self::created(&dir)?;
        if self.expired(created) {
            let _mutation = crate::cache::Mutation(&self.root);
            let _ = crate::cache::remove_unless_active(&dir);
            return None;
        }
        Some(dir)
    }

    /// The live entry, created now when missing or expired.
    fn entry(&self, offer: OfferId) -> io::Result<PathBuf> {
        if let Some(dir) = self.live(offer) {
            return Ok(dir);
        }
        let dir = self.offer_dir(offer);
        fs::create_dir_all(&dir)?;
        write_atomic(&dir, CREATED, &self.now_ms().to_le_bytes())?;
        Ok(dir)
    }

    /// Whether this slot is retained and its 24 hours are not over.
    pub fn contains(&self, offer: OfferId, item: u32, representation: u32) -> bool {
        self.live(offer).is_some_and(|dir| {
            dir.join(Target::Representation { item, representation }.key())
                .is_file()
        })
    }

    /// Copies one bitmap or rich-text representation (bounded by the
    /// representation limit), then keeps the store within its size bound.
    /// An already retained slot is kept as it is.
    pub fn retain(&self, offer: OfferId, item: u32, representation: u32, reader: &mut dyn Read) -> io::Result<u64> {
        let _mutation = crate::cache::Mutation(&self.root);
        let dir = self.entry(offer)?;
        let name = Target::Representation { item, representation }.key();
        if let Ok(meta) = fs::metadata(dir.join(&name)) {
            return Ok(meta.len());
        }
        let temporary = dir.join(format!(".{name}.{}", random_suffix()?));
        let result = (|| {
            let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            let copied = io::copy(&mut reader.take(REPRESENTATION_BYTES + 1), &mut file)?;
            if copied > REPRESENTATION_BYTES {
                return Err(io::Error::from(io::ErrorKind::FileTooLarge));
            }
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, dir.join(&name))?;
            Ok(copied)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        } else {
            self.enforce_cap(&dir);
        }
        result
    }

    /// Removes the oldest entries until the store is within its size bound.
    /// `keep` (the entry just written) and entries being read stay. Returns
    /// the bytes freed.
    fn enforce_cap(&self, keep: &Path) -> u64 {
        if self.window_owned {
            return 0;
        }
        let Ok(entries) = fs::read_dir(&self.root) else {
            return 0;
        };
        let mut entries: Vec<(u64, u64, PathBuf)> = entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                let bytes = crate::cache::tree_bytes(&path).unwrap_or(0);
                (Self::created(&path).unwrap_or(0), bytes, path)
            })
            .collect();
        let mut total = entries
            .iter()
            .fold(0u64, |sum, (_, bytes, _)| sum.saturating_add(*bytes));
        if total <= self.max_bytes {
            return 0;
        }
        entries.sort_by_key(|(created, _, _)| *created);
        let mut freed = 0u64;
        for (_, bytes, path) in entries {
            if total <= self.max_bytes {
                break;
            }
            if path == keep {
                continue;
            }
            if crate::cache::remove_unless_active(&path).unwrap_or(false) {
                total = total.saturating_sub(bytes);
                freed = freed.saturating_add(bytes);
            }
        }
        if freed > 0 {
            crate::cache::changed(&self.root);
        }
        freed
    }

    /// A retained bitmap or rich text and its size, or `None` when it was
    /// never retained or its 24 hours are over.
    pub fn open(&self, offer: OfferId, item: u32, representation: u32) -> io::Result<Option<(RetainedReader, u64)>> {
        let Some(dir) = self.live(offer) else {
            return Ok(None);
        };
        let path = dir.join(Target::Representation { item, representation }.key());
        let lease = SourceLease::busy(path.clone());
        match fs::File::open(&path) {
            Ok(file) => {
                let size = file.metadata()?.len();
                Ok(Some((RetainedReader { file, _lease: lease }, size)))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn thumbnail_name(target: Target, edge: u32, max_bytes: usize) -> String {
        format!("t-{}-{edge}-{max_bytes}", target.key())
    }

    /// A thumbnail cached for exactly these limits within the entry's 24 hours.
    pub fn thumbnail(&self, offer: OfferId, target: Target, edge: u32, max_bytes: usize) -> Option<Thumbnail> {
        let dir = self.live(offer)?;
        let name = Self::thumbnail_name(target, edge, max_bytes);
        let meta: ThumbnailMeta = serde_json::from_slice(&fs::read(dir.join(format!("{name}.json"))).ok()?).ok()?;
        let bytes = fs::read(dir.join(format!("{name}.bin"))).ok()?;
        let mime = crate::image_file::THUMBNAIL_MIMES
            .into_iter()
            .find(|mime| *mime == meta.mime)?;
        (!bytes.is_empty() && bytes.len() <= max_bytes).then_some(Thumbnail {
            mime,
            width: meta.width,
            height: meta.height,
            source_bytes: meta.source_bytes,
            bytes,
        })
    }

    /// Caches a generated thumbnail. An entry made only for a thumbnail (a
    /// live or proxied source) expires 24 hours after this call.
    pub fn store_thumbnail(
        &self,
        offer: OfferId,
        target: Target,
        edge: u32,
        max_bytes: usize,
        thumbnail: &Thumbnail,
    ) -> io::Result<()> {
        let _mutation = crate::cache::Mutation(&self.root);
        let dir = self.entry(offer)?;
        let name = Self::thumbnail_name(target, edge, max_bytes);
        let meta = serde_json::to_vec(&ThumbnailMeta {
            mime: thumbnail.mime.to_owned(),
            width: thumbnail.width,
            height: thumbnail.height,
            source_bytes: thumbnail.source_bytes,
        })?;
        // The metadata names a complete image: it is written last.
        write_atomic(&dir, &format!("{name}.bin"), &thumbnail.bytes)?;
        write_atomic(&dir, &format!("{name}.json"), &meta)
    }

    /// Removes expired and unreadable entries. Run at startup and
    /// periodically; returns the bytes freed.
    pub fn sweep(&self) -> u64 {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return 0;
        };
        let mut freed = 0u64;
        let mut changed = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = Self::created(&path).is_none_or(|created| self.expired(created));
            if !stale {
                continue;
            }
            let before = crate::cache::tree_bytes(&path).unwrap_or(0);
            if crate::cache::remove_unless_active(&path).is_ok() && !path.exists() {
                freed = freed.saturating_add(before);
                changed = true;
            }
        }
        if changed {
            crate::cache::changed(&self.root);
        }
        freed
    }

    /// Cache accounting (category `SourceSnapshots`: copies kept of this
    /// device's own sources). `clear` removes every entry not being read.
    /// Returns the size after the call, the bytes removed and whether some
    /// storage could not be inspected.
    pub fn usage(&self, clear: bool) -> (CacheSize, u64, bool) {
        let mut size = CacheSize {
            category: CacheCategory::SourceSnapshots,
            bytes: 0,
            kept_bytes: 0,
        };
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return (size, 0, false),
            Err(_) => return (size, 0, true),
        };
        let (mut removed, mut incomplete) = (0u64, false);
        for entry in entries {
            let Ok(entry) = entry else {
                incomplete = true;
                continue;
            };
            let path = entry.path();
            let Ok(before) = crate::cache::tree_bytes(&path) else {
                incomplete = true;
                continue;
            };
            if clear && crate::cache::remove_unless_active(&path).is_err() {
                incomplete = true;
            }
            let after = match crate::cache::tree_bytes(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
                Err(_) => {
                    incomplete = true;
                    before
                }
            };
            removed = removed.saturating_add(before.saturating_sub(after));
            size.bytes = size.bytes.saturating_add(after);
            size.kept_bytes = size.kept_bytes.saturating_add(after);
        }
        if clear && removed > 0 {
            crate::cache::changed(&self.root);
        }
        (size, removed, incomplete)
    }
}

fn random_suffix() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = dir.join(format!(".{name}.{}", random_suffix()?));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, dir.join(name))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::{DeviceId, content::ClipId};
    use std::sync::Mutex;

    fn offer(seq: u64) -> OfferId {
        OfferId {
            clip: ClipId {
                origin: DeviceId([4; 32]),
                seq,
            },
            generation: seq,
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: RetainedClips,
        now: Arc<Mutex<SystemTime>>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let now = Arc::new(Mutex::new(UNIX_EPOCH + Duration::from_secs(1_800_000_000)));
            let clock = now.clone();
            let store = RetainedClips::with_clock(dir.path().join(DIR), Arc::new(move || *clock.lock().unwrap()));
            Self { _dir: dir, store, now }
        }
        fn advance(&self, by: Duration) {
            *self.now.lock().unwrap() += by;
        }
        fn rewind(&self, by: Duration) {
            *self.now.lock().unwrap() -= by;
        }
    }

    fn read(store: &RetainedClips, offer: OfferId) -> Option<Vec<u8>> {
        let (mut reader, size) = store.open(offer, 0, 1).unwrap()?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, size);
        Some(bytes)
    }

    #[test]
    fn retained_bitmap_is_readable_for_24_hours_then_swept() {
        let fixture = Fixture::new();
        let store = &fixture.store;
        assert_eq!(store.retain(offer(1), 0, 1, &mut &b"png bytes"[..]).unwrap(), 9);
        // A second retain of the same slot keeps the first copy.
        assert_eq!(store.retain(offer(1), 0, 1, &mut &b"other"[..]).unwrap(), 9);
        assert_eq!(read(store, offer(1)).as_deref(), Some(&b"png bytes"[..]));
        assert!(store.open(offer(1), 0, 2).unwrap().is_none(), "never retained");
        assert!(store.open(offer(2), 0, 1).unwrap().is_none(), "another offer");
        fixture.advance(RETENTION - Duration::from_secs(1));
        assert_eq!(store.sweep(), 0);
        assert!(read(store, offer(1)).is_some());
        fixture.advance(Duration::from_secs(1));
        assert!(store.sweep() > 0);
        assert!(read(store, offer(1)).is_none());
        assert!(!store.offer_dir(offer(1)).exists());
    }

    #[test]
    fn past_the_size_bound_the_oldest_entries_go_first_except_one_being_read() {
        let fixture = Fixture::new();
        let payload = vec![7u8; 64 * 1024];
        fixture.store.retain(offer(1), 0, 1, &mut payload.as_slice()).unwrap();
        let one = crate::cache::tree_bytes(fixture.store.root()).unwrap();
        // Room for two entries, not three.
        let store = fixture.store.clone().with_max_bytes(one * 2 + one / 2);
        fixture.advance(Duration::from_secs(1));
        store.retain(offer(2), 0, 1, &mut payload.as_slice()).unwrap();
        assert!(store.contains(offer(1), 0, 1) && store.contains(offer(2), 0, 1));
        fixture.advance(Duration::from_secs(1));
        store.retain(offer(3), 0, 1, &mut payload.as_slice()).unwrap();
        assert!(!store.contains(offer(1), 0, 1), "the oldest entry is evicted");
        assert!(!store.offer_dir(offer(1)).exists());
        assert!(store.contains(offer(2), 0, 1) && store.contains(offer(3), 0, 1));
        // An entry being read is kept; the next oldest goes instead.
        let (reading, _) = store.open(offer(2), 0, 1).unwrap().unwrap();
        fixture.advance(Duration::from_secs(1));
        store.retain(offer(4), 0, 1, &mut payload.as_slice()).unwrap();
        assert!(store.contains(offer(2), 0, 1), "being read");
        assert!(!store.contains(offer(3), 0, 1));
        assert!(store.contains(offer(4), 0, 1), "the entry just written stays");
        drop(reading);
    }

    #[test]
    fn expiry_on_read_and_a_clock_set_back_ends_the_entry() {
        let fixture = Fixture::new();
        let store = &fixture.store;
        store.retain(offer(1), 0, 1, &mut &b"a"[..]).unwrap();
        fixture.advance(RETENTION);
        assert!(read(store, offer(1)).is_none(), "expired without a sweep");
        assert!(!store.offer_dir(offer(1)).exists());
        store.retain(offer(2), 0, 1, &mut &b"b"[..]).unwrap();
        fixture.rewind(RETENTION * 2);
        assert!(read(store, offer(2)).is_none());
    }

    #[test]
    fn oversized_and_failed_copies_leave_nothing_behind() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        let fixture = Fixture::new();
        let store = &fixture.store;
        assert!(store.retain(offer(1), 0, 1, &mut Failing).is_err());
        assert!(store.open(offer(1), 0, 1).unwrap().is_none());
        let names: Vec<_> = fs::read_dir(store.offer_dir(offer(1)))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(CREATED)]);
    }

    #[test]
    fn thumbnails_are_cached_per_target_and_limits_and_expire_with_the_entry() {
        let fixture = Fixture::new();
        let store = &fixture.store;
        let thumbnail = Thumbnail {
            mime: "image/jpeg",
            width: 512,
            height: 256,
            bytes: vec![0xff, 0xd8, 1, 2],
            source_bytes: 6_000_000,
        };
        let file = Target::File { node: 3, version: 9 };
        store.store_thumbnail(offer(1), file, 512, 4096, &thumbnail).unwrap();
        assert_eq!(store.thumbnail(offer(1), file, 512, 4096), Some(thumbnail.clone()));
        assert_eq!(store.thumbnail(offer(1), file, 256, 4096), None);
        assert_eq!(
            store.thumbnail(offer(1), Target::File { node: 3, version: 10 }, 512, 4096),
            None
        );
        assert_eq!(
            store.thumbnail(
                offer(1),
                Target::Representation {
                    item: 3,
                    representation: 9
                },
                512,
                4096
            ),
            None
        );
        fixture.advance(RETENTION);
        assert_eq!(store.thumbnail(offer(1), file, 512, 4096), None);
    }

    #[test]
    fn clearing_the_cache_removes_entries_except_one_being_read() {
        let fixture = Fixture::new();
        let store = &fixture.store;
        store.retain(offer(1), 0, 1, &mut &[7u8; 4096][..]).unwrap();
        store.retain(offer(2), 0, 1, &mut &[7u8; 4096][..]).unwrap();
        let (size, removed, incomplete) = store.usage(false);
        assert!(!incomplete && removed == 0 && size.bytes > 0);
        assert_eq!(size.category, CacheCategory::SourceSnapshots);
        let (reading, _) = store.open(offer(1), 0, 1).unwrap().unwrap();
        let (size, removed, incomplete) = store.usage(true);
        assert!(!incomplete && removed > 0);
        assert!(size.kept_bytes > 0, "the entry being read is kept");
        assert!(read(store, offer(2)).is_none());
        drop(reading);
        let (size, _, _) = store.usage(true);
        assert_eq!(size.bytes, 0);
        assert!(read(store, offer(1)).is_none());
    }
}
