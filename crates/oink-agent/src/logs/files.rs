//! Ordinary text logs alone share one decimal-byte budget across all process roles.
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};

const MB: u64 = 1_000_000;
static WRITER_DIR: OnceLock<PathBuf> = OnceLock::new();
const ROLES: &[&str] = &[
    "oink",
    "oink-ui",
    "oink-backend",
    "oink-service",
    "oink-crash-collector",
    "oink-update-worker",
];

/// Update ordinary-log retention, including already closed files. Diagnostics
/// and the private report queue have their own owners and budgets.
pub fn set_max_mb(mb: u64) -> io::Result<()> {
    let bytes = mb.saturating_mul(MB);
    if let Some(dir) = WRITER_DIR.get() {
        let mut lock = directory_lock(dir)?;
        publish_cap(&mut lock, bytes)?;
        prune(dir, bytes, 0)?;
    }
    Ok(())
}

fn directory_lock(dir: &Path) -> io::Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".ordinary-log.lock"))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn publish_cap(lock: &mut File, bytes: u64) -> io::Result<()> {
    lock.rewind()?;
    lock.write_all(b"OINKLOG1")?;
    lock.write_all(&bytes.to_le_bytes())?;
    lock.set_len(16)
}

fn shared_cap(lock: &mut File) -> io::Result<u64> {
    lock.rewind()?;
    let mut bytes = [0; 16];
    lock.read_exact(&mut bytes)?;
    if &bytes[..8] != b"OINKLOG1" {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(u64::from_le_bytes(bytes[8..].try_into().unwrap()))
}

fn ordinary_log(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".log")?;
    let (role, date) = stem.rsplit_once('.')?;
    if !ROLES.contains(&role) || date.len() != 10 {
        return None;
    }
    let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").ok()?;
    time::Date::parse(date, &format).ok()?;
    Some(date)
}

fn prune(dir: &Path, cap: u64, incoming: u64) -> io::Result<()> {
    let mut files = Vec::new();
    let mut total = incoming;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(date) = name.to_str().and_then(ordinary_log) else {
            continue;
        };
        let size = entry.metadata()?.len();
        total = total.saturating_add(size);
        files.push((date.to_owned(), name, entry.path(), size));
    }
    files.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    for (_, _, path, size) in files {
        if total <= cap {
            break;
        }
        fs::remove_file(path)?;
        total = total.saturating_sub(size);
    }
    Ok(())
}

pub(super) struct Writer {
    dir: PathBuf,
    role: &'static str,
}
impl Writer {
    pub(super) fn new(dir: PathBuf, role: &str, configured_mb: impl FnOnce() -> u64) -> io::Result<Self> {
        let role = ROLES
            .iter()
            .copied()
            .find(|r| *r == role)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut lock = directory_lock(&dir)?;
        // Read durable settings while holding the same lock as live updates. A
        // concurrently starting process must not republish a stale snapshot.
        let cap = configured_mb().saturating_mul(MB);
        publish_cap(&mut lock, cap)?;
        prune(&dir, cap, 0)?;
        #[cfg(not(test))]
        let _ = WRITER_DIR.set(dir.clone());
        Ok(Self { dir, role })
    }
    fn append(&self, date: time::Date, bytes: &[u8], cap: Option<u64>) -> io::Result<()> {
        // No writer retains an open log file between batches. The shared lock
        // lets any role remove the oldest daily file on Windows as well as Unix.
        let mut lock = directory_lock(&self.dir)?;
        let cap = match cap {
            Some(cap) => cap,
            None => shared_cap(&mut lock)?,
        };
        let keep = bytes.len().min(usize::try_from(cap).unwrap_or(usize::MAX));
        let bytes = &bytes[bytes.len() - keep..];
        prune(&self.dir, cap, bytes.len() as u64)?;
        if !bytes.is_empty() {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(format!("{}.{date}.log", self.role)))?
                .write_all(bytes)?;
        }
        Ok(())
    }
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append(time::OffsetDateTime::now_utc().date(), bytes, None)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_cap_prunes_only_ordinary_logs_and_has_no_day_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let protected = ["Diagnostics", "reports", "buffer"];
        for folder in protected {
            fs::create_dir(dir.path().join(folder)).unwrap();
            fs::write(dir.path().join(folder).join("oink.2026-01-01.log"), [9; 50]).unwrap();
        }
        for name in [
            "manual.zip",
            "emergency.zip",
            "capsule.bin",
            "oink.unknown.log",
            "other.2026-01-01.log",
        ] {
            fs::write(dir.path().join(name), [9; 50]).unwrap();
        }
        for day in 1..=12 {
            fs::write(dir.path().join(format!("oink.2026-01-{day:02}.log")), [1; 5]).unwrap();
        }
        fs::write(dir.path().join("oink-ui.2026-01-12.log"), [2; 15]).unwrap();
        prune(dir.path(), 75, 0).unwrap();
        assert!(dir.path().join("oink.2026-01-01.log").exists());
        prune(dir.path(), 25, 0).unwrap();
        assert!(!dir.path().join("oink.2026-01-10.log").exists());
        assert!(dir.path().join("oink.2026-01-11.log").exists());
        assert!(dir.path().join("oink-ui.2026-01-12.log").exists());
        prune(dir.path(), 0, 0).unwrap();
        for folder in protected {
            assert_eq!(
                fs::read(dir.path().join(folder).join("oink.2026-01-01.log")).unwrap(),
                [9; 50]
            );
        }
        assert!(dir.path().join("manual.zip").exists());
        assert!(dir.path().join("other.2026-01-01.log").exists());
    }
    #[test]
    fn one_daily_file_and_oversize_batch_obey_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let writer = Writer::new(dir.path().into(), "oink", || 1024).unwrap();
        let day = time::Date::from_calendar_date(2026, time::Month::January, 1).unwrap();
        writer.append(day, b"first", Some(8)).unwrap();
        writer.append(day, b"second", Some(8)).unwrap();
        let path = dir.path().join("oink.2026-01-01.log");
        assert_eq!(fs::read(&path).unwrap(), b"second");
        writer.append(day, b"0123456789", Some(8)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"23456789");
        writer.append(day, b"discarded", Some(0)).unwrap();
        assert!(!path.exists());
        assert_eq!(1024 * MB, 1_024_000_000);
    }

    #[test]
    fn roles_observe_the_same_live_cap_before_each_batch() {
        let dir = tempfile::tempdir().unwrap();
        let mut background = Writer::new(dir.path().into(), "oink", || 1024).unwrap();
        let mut ui = Writer::new(dir.path().into(), "oink-ui", || 1024).unwrap();
        background.write_all(b"background").unwrap();
        {
            let mut lock = directory_lock(dir.path()).unwrap();
            publish_cap(&mut lock, 6).unwrap();
        }
        ui.write_all(b"ui").unwrap();
        background.write_all(b"last").unwrap();
        let total: u64 = fs::read_dir(dir.path())
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_str().and_then(ordinary_log).is_some())
            .map(|entry| entry.metadata().unwrap().len())
            .sum();
        assert_eq!(total, 6);
    }
}
