//! Received files own their OS copies, never the completed-deliveries cache.
use oink_foundation::DeviceId;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
pub const DAY: u64 = 86_400_000;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    #[default]
    Never,
    Days7,
    Days30,
}
impl Cleanup {
    pub fn days(self) -> u64 {
        match self {
            Self::Never => 0,
            Self::Days7 => 7,
            Self::Days30 => 30,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    #[default]
    Received,
    LastUsed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reader {
    System,
    Shell,
    App,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Shell24h,
    Age7,
    Age30,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub received_at: u64,
    pub last_used: u64,
    pub materialized: bool,
    pub shell: bool,
    pub app: bool,
    pub unknown: bool,
    pub observation_gap: bool,
}
impl Item {
    pub fn new(received_at: u64) -> Self {
        Self {
            received_at,
            last_used: received_at,
            materialized: false,
            shell: false,
            app: false,
            unknown: false,
            observation_gap: false,
        }
    }
    pub fn observe(&mut self, reader: Reader, now: u64) {
        match reader {
            Reader::System => return,
            Reader::Shell => self.shell = true,
            Reader::App => self.app = true,
            Reader::Unknown => self.unknown = true,
        }
        self.last_used = self.last_used.max(now);
    }
    pub fn gap(&mut self, now: u64) {
        self.observation_gap = true;
        self.last_used = self.last_used.max(now);
    }
    pub fn shell_only(&self) -> bool {
        self.shell && !self.app && !self.unknown && !self.observation_gap
    }
    pub fn cleanup(&self, now: u64, setting: Cleanup, basis: Basis, busy: bool) -> Option<Reason> {
        if busy || !self.materialized {
            return None;
        }
        if basis == Basis::LastUsed && self.shell_only() && now.saturating_sub(self.received_at) >= DAY {
            return Some(Reason::Shell24h);
        }
        let age = now.saturating_sub(match basis {
            Basis::Received => self.received_at,
            Basis::LastUsed => self.last_used,
        });
        match setting {
            Cleanup::Days7 if age >= 7 * DAY => Some(Reason::Age7),
            Cleanup::Days30 if age >= 30 * DAY => Some(Reason::Age30),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub materialized_bytes: u64,
    pub incomplete: bool,
    pub time_basis: Basis,
    pub cleanup: Cleanup,
}
static SETTING: AtomicU8 = AtomicU8::new(0);
pub fn configure(value: Cleanup) {
    SETTING.store(value.days() as u8, Ordering::Release);
}
pub fn setting() -> Cleanup {
    match SETTING.load(Ordering::Acquire) {
        7 => Cleanup::Days7,
        30 => Cleanup::Days30,
        _ => Cleanup::Never,
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
static NAMES: OnceLock<Mutex<BTreeMap<DeviceId, String>>> = OnceLock::new();
pub fn remember_source(id: DeviceId, name: String) {
    NAMES.get_or_init(Default::default).lock().unwrap().insert(id, name);
}
pub fn source_name(id: DeviceId) -> String {
    NAMES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .unwrap_or_else(|| "Device".into())
}
/// Persistent ID mapping prevents device renames or short-ID collisions moving paths.
pub fn source_folder(root: &Path, id: DeviceId, name: &str) -> io::Result<String> {
    static GATE: Mutex<()> = Mutex::new(());
    let _gate = GATE.lock().unwrap();
    let name = normalized_name(name);
    let name = name.as_str();
    fs::create_dir_all(root)?;
    let path = root.join("received.sources");
    let mut map: BTreeMap<String, String> = match fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e),
    };
    let full = id.to_hex();
    if let Some(name) = map.get(&full) {
        if name.is_empty()
            || std::path::Path::new(name).components().count() != 1
            || name.contains(['/', '\\'])
            || name == "."
            || name == ".."
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        return Ok(name.clone());
    }
    let mut clean = String::new();
    for c in name.chars() {
        let c = if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
            '_'
        } else {
            c
        };
        if clean.len() + c.len_utf8() > 120 || clean.encode_utf16().count() + c.len_utf16() > 80 {
            break;
        }
        clean.push(c);
    }
    let clean = crate::sync::files::sanitize_component(
        clean.trim_end_matches([' ', '.']),
        crate::sync::files::TargetOs::Windows,
    );
    for len in (12..=64).step_by(4) {
        let candidate = format!("{clean} {}", &full[..len]);
        if !map.keys().any(|other| other.get(..len) == Some(&full[..len]))
            && !map.values().any(|v| v.eq_ignore_ascii_case(&candidate))
        {
            map.insert(full, candidate.clone());
            oink_foundation::storage::atomic_write(&path, &serde_json::to_vec(&map)?)?;
            return Ok(candidate);
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}
pub fn allocate_batch(root: &Path, stamp: &str) -> io::Result<PathBuf> {
    allocate_batch_with(root, stamp, |path| fs::create_dir(path))
}
pub(crate) fn allocate_batch_with(
    root: &Path,
    stamp: &str,
    mut create: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    if stamp.len() != 19
        || !stamp.bytes().enumerate().all(|(i, c)| {
            if i == 8 || i == 15 {
                c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    for suffix in 1..=100_000 {
        let path = root.join(if suffix == 1 {
            stamp.to_owned()
        } else {
            format!("{stamp}-{suffix}")
        });
        match create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::ErrorKind::AlreadyExists.into())
}
/// Formatting is delegated to the OS local calendar; no dependency on a global TZ mutation.
pub fn local_stamp() -> String {
    #[cfg(target_os = "macos")]
    {
        let millis = now();
        let seconds = (millis / 1000) as libc::time_t;
        let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: localtime_r writes the provided tm and reads the live time value.
        unsafe {
            if !libc::localtime_r(&seconds, tm.as_mut_ptr()).is_null() {
                let t = tm.assume_init();
                return format!(
                    "{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}",
                    t.tm_year + 1900,
                    t.tm_mon + 1,
                    t.tm_mday,
                    t.tm_hour,
                    t.tm_min,
                    t.tm_sec,
                    millis % 1000
                );
            }
        }
    }
    #[cfg(windows)]
    {
        let mut t = windows_sys::Win32::Foundation::SYSTEMTIME::default();
        // SAFETY: GetLocalTime fills this live system-time value.
        unsafe {
            windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        }
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        )
    }
    #[cfg(not(windows))]
    "19700101-000000-000".into()
}

pub fn usage() -> io::Result<Usage> {
    #[cfg(target_os = "macos")]
    {
        crate::file_provider::received_usage(false)
    }
    #[cfg(windows)]
    {
        crate::windows::cloud::received_usage()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err(io::ErrorKind::Unsupported.into())
    }
}
pub fn open() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::file_provider::open_received()
    }
    #[cfg(windows)]
    {
        crate::windows::cloud::open_received()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err(io::ErrorKind::Unsupported.into())
    }
}

fn normalized_name(value: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        objc2_foundation::NSString::from_str(value)
            .precomposedStringWithCanonicalMapping()
            .to_string()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let input: Vec<u16> = std::ffi::OsStr::new(value).encode_wide().collect();
        let mut output = vec![0u16; input.len() * 3 + 8];
        // SAFETY: both counted UTF-16 buffers are live and do not overlap.
        let count = unsafe {
            windows_sys::Win32::Globalization::NormalizeString(
                windows_sys::Win32::Globalization::NormalizationC,
                input.as_ptr(),
                input.len() as i32,
                output.as_mut_ptr(),
                output.len() as i32,
            )
        };
        if count > 0 {
            String::from_utf16_lossy(&output[..count as usize])
        } else {
            value.into()
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_and_unknown_survive_cache_expiry_but_explicit_age_overrides() {
        let mut item = Item::new(100);
        item.materialized = true;
        item.observe(Reader::App, 200);
        assert_eq!(
            item.cleanup(100 + 30 * DAY, Cleanup::Never, Basis::Received, false),
            None
        );
        assert_eq!(
            item.cleanup(100 + 7 * DAY, Cleanup::Days7, Basis::Received, false),
            Some(Reason::Age7)
        );
        assert_eq!(
            item.cleanup(100 + 7 * DAY, Cleanup::Days7, Basis::LastUsed, false),
            None
        );
        item.gap(300);
        assert!(!item.shell_only());
        assert_eq!(
            item.cleanup(100 + 30 * DAY, Cleanup::Days30, Basis::Received, true),
            None
        );
    }
    #[test]
    fn shell_rule_requires_continuous_observation_and_materialization() {
        let mut item = Item::new(10);
        item.observe(Reader::System, 20);
        assert!(!item.shell_only());
        item.observe(Reader::Shell, 30);
        item.materialized = true;
        assert_eq!(
            item.cleanup(10 + DAY, Cleanup::Never, Basis::LastUsed, false),
            Some(Reason::Shell24h)
        );
        assert_eq!(
            item.cleanup(10 + DAY, Cleanup::Never, Basis::Received, false),
            None,
            "macOS never applies shell-only cleanup"
        );
        item.gap(40);
        assert_eq!(item.cleanup(10 + DAY, Cleanup::Never, Basis::LastUsed, false), None);
        assert_eq!(item.last_used, 40);
    }
    #[test]
    fn system_activity_never_renews_user_usage_and_classification_is_monotonic() {
        let mut item = Item::new(10);
        item.observe(Reader::App, 20);
        item.observe(Reader::Shell, 30);
        item.observe(Reader::System, 999);
        assert_eq!(item.last_used, 30);
        assert!(!item.shell_only());
        let restored: Item = serde_json::from_slice(&serde_json::to_vec(&item).unwrap()).unwrap();
        assert_eq!(restored, item);
    }
    #[test]
    fn normalized_names_and_persisted_mapping_reject_traversal() {
        let root = tempfile::tempdir().unwrap();
        let a = source_folder(root.path(), DeviceId([1; 32]), "e\u{301}").unwrap();
        assert!(a.starts_with("é "));
        let id = DeviceId([2; 32]);
        fs::write(
            root.path().join("received.sources"),
            serde_json::to_vec(&BTreeMap::from([(id.to_hex(), "../escape")])).unwrap(),
        )
        .unwrap();
        assert_eq!(
            source_folder(root.path(), id, "safe").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn directories_are_stable_sanitized_and_collision_safe() {
        let root = tempfile::tempdir().unwrap();
        let a = source_folder(root.path(), oink_foundation::DeviceId([1; 32]), "bad/PC: .").unwrap();
        assert!(!a.contains('/'));
        assert_eq!(
            a,
            source_folder(root.path(), oink_foundation::DeviceId([1; 32]), "new name").unwrap()
        );
        let mut id = [1; 32];
        id[31] = 2;
        let b = source_folder(root.path(), oink_foundation::DeviceId(id), "bad/PC: .").unwrap();
        assert_ne!(a, b);
        let first = allocate_batch(root.path(), "20261021-234512-123").unwrap();
        let second = allocate_batch(root.path(), "20261021-234512-123").unwrap();
        assert_eq!(first.file_name().unwrap(), "20261021-234512-123");
        assert_eq!(second.file_name().unwrap(), "20261021-234512-123-2");
        assert!(allocate_batch(root.path(), "../escape").is_err());
    }
}
