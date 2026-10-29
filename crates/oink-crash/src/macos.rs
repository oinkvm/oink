//! Attributable Apple .ips projection, separate from local diagnostics' latest()
//! heuristic. Only a prior grant-armed process lifetime may authorize extraction.
use oink_telemetry::{capture::Manifest, *};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io::Read, path::Path};
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lifetime {
    pub schema: u16,
    pub pid: u32,
    pub launched_at: u64,
    pub armed_at: u64,
    pub generation: u64,
    pub component: Component,
    pub image: ImageSlot,
    pub debug_id: [u8; 16],
    pub build: BuildContext,
}
/// Read only the fixed Mach-O header and its bounded load-command region.
/// Packaged executables are thin 64-bit images; names and symbol tables are
/// neither needed nor read to bind a lifetime to the running executable.
pub fn executable_uuid(path: &Path) -> Option<[u8; 16]> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 32];
    file.read_exact(&mut header).ok()?;
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
        return None;
    }
    let ncmds = u32::from_le_bytes(header[16..20].try_into().ok()?) as usize;
    let size = u32::from_le_bytes(header[20..24].try_into().ok()?) as usize;
    if size > 64 * 1024 || ncmds > size / 8 {
        return None;
    }
    let mut commands = vec![0u8; size];
    file.read_exact(&mut commands).ok()?;
    let mut at = 0usize;
    let mut found = None;
    for _ in 0..ncmds {
        let command = commands.get(at..at.checked_add(8)?)?;
        let kind = u32::from_le_bytes(command[..4].try_into().ok()?);
        let length = u32::from_le_bytes(command[4..].try_into().ok()?) as usize;
        if length < 8 || !length.is_multiple_of(8) {
            return None;
        }
        let bytes = commands.get(at..at.checked_add(length)?)?;
        if kind == 0x1b {
            if length != 24 || found.is_some() {
                return None;
            }
            let id: [u8; 16] = bytes[8..24].try_into().ok()?;
            if id == [0; 16] {
                return None;
            }
            found = Some(id);
        }
        at += length;
    }
    if at != size {
        return None;
    }
    found
}
#[cfg(target_os = "macos")]
pub fn process_launch(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // SAFETY: proc_pidinfo writes at most the supplied proc_bsdinfo allocation.
    let size = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if size != std::mem::size_of::<libc::proc_bsdinfo>() as i32 {
        return None;
    }
    // SAFETY: the exact initialized struct size was returned above.
    Some(unsafe { info.assume_init() }.pbi_start_tvsec)
}
fn timestamp(s: &str) -> Option<u64> {
    if s.len() > 64 {
        return None;
    }
    use time::{
        OffsetDateTime,
        format_description::{parse_borrowed, well_known::Rfc3339},
    };
    let t = OffsetDateTime::parse(s, &Rfc3339).ok().or_else(|| {
        [
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond] [offset_hour sign:mandatory][offset_minute]",
            "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]",
        ]
        .iter()
        .find_map(|s_format| OffsetDateTime::parse(s, &parse_borrowed::<2>(s_format).ok()?).ok())
    })?;
    u64::try_from(t.unix_timestamp()).ok()
}
pub fn debug_id(s: &str) -> Option<[u8; 16]> {
    if s.len() != 36 || [8, 13, 18, 23].iter().any(|i| s.as_bytes()[*i] != b'-') {
        return None;
    }
    let chars: Vec<_> = s.bytes().filter(|b| *b != b'-').collect();
    let mut id = [0; 16];
    for i in 0..16 {
        id[i] = (chars[i * 2] as char).to_digit(16)? as u8 * 16 + (chars[i * 2 + 1] as char).to_digit(16)? as u8;
    }
    Some(id)
}
pub fn extract(
    bytes: &[u8],
    lifetime: Lifetime,
    consent: ReportConsent,
    manifest: &Manifest,
    now: u64,
) -> Option<(SafeCrash, u64)> {
    if bytes.len() > 8 * 1024 * 1024
        || lifetime.schema != 1
        || !consent.granted()
        || lifetime.generation != consent.generation
        || lifetime.build.app_version != manifest.version
        || lifetime.armed_at < lifetime.launched_at
    {
        return None;
    }
    let mut objects = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let first = objects.next()?.ok()?;
    let raw = objects.next().transpose().ok()?.unwrap_or(first);
    if raw.get("pid")?.as_u64()? != lifetime.pid as u64
        || timestamp(raw.get("procLaunch")?.as_str()?)? != lifetime.launched_at
    {
        return None;
    }
    let at = timestamp(raw.get("captureTime")?.as_str()?)?;
    if at < lifetime.armed_at || at > now || now - at > 7 * 86400 {
        return None;
    }
    let images = raw.get("usedImages")?.as_array()?;
    if images.len() > 4096
        || !images
            .iter()
            .any(|i| i.get("uuid").and_then(Value::as_str).and_then(debug_id) == Some(lifetime.debug_id))
    {
        return None;
    }
    if !manifest
        .images
        .iter()
        .any(|i| i.slot == lifetime.image && i.debug_id == lifetime.debug_id)
    {
        return None;
    }
    let class = match raw.pointer("/exception/type").and_then(Value::as_str) {
        Some("EXC_BAD_ACCESS") => ExceptionClass::BadAccess,
        Some("EXC_BAD_INSTRUCTION") => ExceptionClass::BadInstruction,
        Some("EXC_ARITHMETIC") => ExceptionClass::Arithmetic,
        Some("EXC_BREAKPOINT") => ExceptionClass::Breakpoint,
        Some("EXC_CRASH") => ExceptionClass::Abort,
        Some("EXC_RESOURCE") => ExceptionClass::Resource,
        Some("EXC_GUARD") => ExceptionClass::Guard,
        _ => ExceptionClass::Unknown,
    };
    let mut crash = SafeCrash::empty(lifetime.build, lifetime.component, CrashKind::NativeException, class);
    for (slot, frame) in crash
        .frames
        .iter_mut()
        .flatten()
        .zip(project_frames(&raw, manifest, lifetime.image)?)
    {
        *slot = Some(frame);
    }
    Some((crash, at))
}
/// Shared numeric projection for attributable upload and local diagnostic views.
pub fn project_frames(raw: &Value, manifest: &Manifest, preferred: ImageSlot) -> Option<Vec<Frame>> {
    let images = raw.get("usedImages")?.as_array()?;
    if images.len() > 4096 {
        return None;
    }
    let thread = raw
        .get("faultingThread")
        .and_then(Value::as_u64)
        .and_then(|i| raw.get("threads")?.as_array()?.get(i as usize))?;
    let frames = thread.get("frames")?.as_array()?;
    Some(
        frames
            .iter()
            .enumerate()
            .filter_map(|(position, f)| {
                let index = f.get("imageIndex")?.as_u64()? as usize;
                let image = images.get(index)?;
                let offset = f.get("imageOffset")?.as_u64()?;
                // .ips frame zero is the context PC; subsequent frames are return
                // PCs. Apply Sentry symbolic's architecture rule BEFORE dropping
                // system frames, or the first retained caller becomes a false PC.
                let offset = if position == 0 {
                    offset
                } else {
                    match image.get("arch")?.as_str()? {
                        "arm64" | "arm64e" => (offset & !3).checked_sub(4)?,
                        "x86_64" | "x86_64h" => offset.checked_sub(1)?,
                        _ => return None, // Never guess a return-PC adjustment.
                    }
                };
                let id = debug_id(image.get("uuid")?.as_str()?)?;
                let shipped = manifest.image(id, preferred)?;
                (offset < shipped.text_size).then_some(Frame::Native {
                    image: shipped.slot,
                    offset,
                    debug_id: id,
                    debug_age: 0,
                    code_id: None,
                    image_size: shipped.text_size,
                })
            })
            .take(64)
            .collect(),
    )
}
/// Bounded scan only in the presence of a previously armed lifetime. No startup
/// backfill or OS-owned file deletion; all attribution checks precede projection.
pub fn recover(
    directory: &Path,
    lifetime: Lifetime,
    consent: ReportConsent,
    manifest: &Manifest,
    now: u64,
) -> Option<(SafeCrash, u64)> {
    if !consent.granted() || lifetime.generation != consent.generation {
        return None;
    }
    let entries = std::fs::read_dir(directory).ok()?;
    for entry in entries.take(1024).flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|s| s != "ips") {
            continue;
        }
        let metadata = entry.metadata().ok()?;
        if metadata.len() > 8 * 1024 * 1024 || !metadata.is_file() {
            continue;
        }
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        if modified < lifetime.armed_at {
            continue;
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if let Some(c) = extract(&bytes, lifetime, consent, manifest, now) {
            return Some(c);
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caller_addresses_are_normalized_before_filtering_system_frames() {
        let manifest = Manifest {
            version: AppVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 1,
            },
            images: vec![oink_telemetry::capture::Image {
                slot: ImageSlot::Agent,
                debug_id: [1; 16],
                debug_age: 0,
                code_id: None,
                text_size: 4096,
            }],
        };
        for (arch, expected) in [("arm64", 20), ("x86_64", 23)] {
            let raw = serde_json::json!({"usedImages":[
                {"uuid":"02020202-0202-0202-0202-020202020202", "arch":arch},
                {"uuid":"01010101-0101-0101-0101-010101010101", "arch":arch}],
                "faultingThread":0,"threads":[{"frames":[
                    {"imageIndex":0,"imageOffset":24},
                    {"imageIndex":1,"imageOffset":24},
                    {"imageIndex":1,"imageOffset":4096}]}]});
            let frames = project_frames(&raw, &manifest, ImageSlot::Agent).unwrap();
            assert!(matches!(frames[0], Frame::Native{offset,..} if offset == expected));
            assert_eq!(frames.len(), 2, "normalize before checking image extent too");
            let mut context = raw;
            context["threads"][0]["frames"] = serde_json::json!([{ "imageIndex":1, "imageOffset":24 }]);
            assert!(matches!(
                project_frames(&context, &manifest, ImageSlot::Agent).unwrap()[0],
                Frame::Native { offset: 24, .. }
            ));
        }
    }
    #[test]
    fn executable_identity_comes_from_the_image_and_rejects_ambiguous_commands() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut bytes = vec![0u8; 56];
        bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&24u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&0x1bu32.to_le_bytes());
        bytes[36..40].copy_from_slice(&24u32.to_le_bytes());
        bytes[40..56].fill(7);
        std::fs::write(file.path(), &bytes).unwrap();
        assert_eq!(executable_uuid(file.path()), Some([7; 16]));
        bytes[16..20].copy_from_slice(&2u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&48u32.to_le_bytes());
        bytes.extend_from_within(32..56);
        std::fs::write(file.path(), &bytes).unwrap();
        assert_eq!(executable_uuid(file.path()), None);
        bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(file.path(), &bytes).unwrap();
        assert_eq!(executable_uuid(file.path()), None);
    }
    #[test]
    fn exact_lifetime_grant_build_and_only_shipped_frames() {
        let manifest = Manifest {
            version: AppVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 1,
            },
            images: vec![oink_telemetry::capture::Image {
                slot: ImageSlot::Agent,
                debug_id: [1; 16],
                debug_age: 0,
                code_id: None,
                text_size: 4096,
            }],
        };
        let launched = timestamp("2026-10-15T00:00:00Z").unwrap();
        let lifetime = Lifetime {
            schema: 1,
            pid: 42,
            launched_at: launched,
            armed_at: launched + 1,
            generation: 1,
            component: Component::Agent,
            image: ImageSlot::Agent,
            debug_id: [1; 16],
            build: BuildContext {
                app_version: manifest.version,
                os_version_class: OsVersionClass::MacOSMajor(14),
            },
        };
        let consent = ReportConsent {
            choice: Choice::Granted,
            policy_version: 3,
            generation: 1,
        };
        let raw=br#"{"pid":42,"procLaunch":"2026-10-15T00:00:00Z","captureTime":"2026-10-15T00:01:00Z","procPath":"poison-path","exception":{"type":"EXC_BAD_ACCESS","reason":"poison-content"},"usedImages":[{"uuid":"01010101-0101-0101-0101-010101010101","path":"poison-image"},{"uuid":"02020202-0202-0202-0202-020202020202"}],"faultingThread":0,"threads":[{"name":"poison-name","frames":[{"imageIndex":0,"imageOffset":24,"symbol":"poison-symbol"},{"imageIndex":1,"imageOffset":2}]}]}"#;
        let (crash, _) = extract(raw, lifetime, consent, &manifest, launched + 120).unwrap();
        assert_eq!(crash.frames().count(), 1);
        assert!(!serde_json::to_string(&crash).unwrap().contains("poison"));
        for bad in [
            Lifetime { schema: 2, ..lifetime },
            Lifetime { pid: 43, ..lifetime },
            Lifetime {
                launched_at: launched - 1,
                ..lifetime
            },
            Lifetime {
                armed_at: launched + 61,
                ..lifetime
            },
            Lifetime {
                generation: 0,
                ..lifetime
            },
            Lifetime {
                debug_id: [2; 16],
                ..lifetime
            },
        ] {
            assert!(extract(raw, bad, consent, &manifest, launched + 120).is_none());
        }
        assert!(extract(raw, lifetime, ReportConsent::default(), &manifest, launched + 120).is_none());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod native_tests {
    use super::*;
    #[test]
    fn extension_signal_capsule_subprocess_is_bounded_and_grant_armed() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("capture-fixture");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/file-provider");
        assert!(
            std::process::Command::new("xcrun")
                .args(["clang", "-std=c11", "-O2"])
                .arg(root.join("CrashCapture.c"))
                .arg(root.join("ReportCaptureTests.c"))
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        let path = dir.path().join("capsule");
        let version = AppVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 1,
        };
        let manifest = Manifest {
            version,
            images: vec![],
        };
        let consent = ReportConsent {
            choice: Choice::Granted,
            policy_version: 3,
            generation: 1,
        };
        assert!(
            !std::process::Command::new(&executable)
                .arg(&path)
                .arg("1")
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128);
        let (crash, _) = crate::capsule::recover(&path, consent, &manifest, OsVersionClass::Unknown, now()).unwrap();
        assert_eq!(crash.component, Component::FileProvider);
        assert_eq!(crash.exception_code, Some(libc::SIGABRT as u32));
        assert!(
            !std::process::Command::new(&executable)
                .arg(&path)
                .arg("0")
                .status()
                .unwrap()
                .success()
        );
        assert!(crate::capsule::recover(&path, consent, &manifest, OsVersionClass::Unknown, now()).is_none());
    }
}
