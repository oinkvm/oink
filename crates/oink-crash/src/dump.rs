//! Bounded local-only minidump projection. No minidump/stack memory/module name
//! is returned. Missing unwind information reduces coverage to the faulting PC.
use oink_telemetry::{capture::Manifest, *};
pub const MAX_DUMP: u64 = 32 * 1024 * 1024;
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}
fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at.checked_add(8)?)?.try_into().ok()?))
}
fn slice(bytes: &[u8], at: u32, size: u32) -> Option<&[u8]> {
    bytes.get(at as usize..(at as usize).checked_add(size as usize)?)
}
fn stream(bytes: &[u8], kind: u32) -> Option<&[u8]> {
    if bytes.get(..4)? != b"MDMP" || bytes.len() as u64 > MAX_DUMP {
        return None;
    }
    let count = u32_at(bytes, 8)?;
    if count > 128 {
        return None;
    }
    let directory = u32_at(bytes, 12)? as usize;
    for i in 0..count as usize {
        let at = directory.checked_add(i.checked_mul(12)?)?;
        if u32_at(bytes, at)? == kind {
            return slice(bytes, u32_at(bytes, at + 8)?, u32_at(bytes, at + 4)?);
        }
    }
    None
}
fn uuid(raw: &[u8]) -> Option<[u8; 16]> {
    let mut id: [u8; 16] = raw.get(..16)?.try_into().ok()?;
    // CodeView stores the first GUID fields little endian; normalize to UUID bytes.
    id[..4].reverse();
    id[4..6].reverse();
    id[6..8].reverse();
    Some(id)
}
pub fn extract(bytes: &[u8], manifest: &Manifest, build: BuildContext, component: Component) -> Option<SafeCrash> {
    if build.app_version != manifest.version {
        return None;
    }
    let exception = stream(bytes, 6)?;
    let code = u32_at(exception, 8)?;
    let class = match code {
        0xc0000005 => ExceptionClass::BadAccess,
        0xc000001d => ExceptionClass::BadInstruction,
        0xc0000094 | 0xc000008e => ExceptionClass::Arithmetic,
        0x80000003 => ExceptionClass::Breakpoint,
        0x40000015 => ExceptionClass::Abort,
        _ => ExceptionClass::Unknown,
    };
    let mut crash = SafeCrash::empty(build, component, CrashKind::NativeException, class);
    crash.exception_code = Some(code);
    // AMD64 context flags + RIP only. Unknown architectures/context layouts
    // produce a safe unsymbolicated event rather than guessing/unwinding memory.
    let context = slice(bytes, u32_at(exception, 164)?, u32_at(exception, 160)?)?;
    if u32_at(context, 48).is_none_or(|f| f & 0x0010_0001 != 0x0010_0001) {
        return Some(crash);
    }
    let Some(pc) = u64_at(context, 248) else {
        return Some(crash);
    };
    if let Some(modules) = stream(bytes, 4) {
        let count = u32_at(modules, 0)?;
        if count > 4096 {
            return Some(crash);
        }
        for i in 0..count as usize {
            let at = 4 + i * 108;
            let base = u64_at(modules, at)?;
            let size = u32_at(modules, at + 8)? as u64;
            let Some(offset) = pc.checked_sub(base).filter(|o| *o < size) else {
                continue;
            };
            let Some(cv) = slice(bytes, u32_at(modules, at + 80)?, u32_at(modules, at + 76)?) else {
                continue;
            };
            if cv.get(..4) != Some(b"RSDS") {
                continue;
            }
            let id = uuid(cv.get(4..20)?)?;
            let preferred = match component {
                Component::UiRust | Component::UiWebview => ImageSlot::UiRust,
                Component::FileProvider => ImageSlot::FileProvider,
                Component::Agent => ImageSlot::Agent,
            };
            if let Some(image) = manifest.image(id, preferred).filter(|image| offset < image.text_size) {
                crash.frames[0][0] = Some(Frame::Native {
                    image: image.slot,
                    offset,
                    debug_id: id,
                });
            }
            break;
        }
    }
    Some(crash)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_names_memory_registers_and_unshipped_modules_never_cross_projection() {
        let mut raw = vec![0u8; 1024];
        raw[..4].copy_from_slice(b"MDMP");
        fn put(b: &mut [u8], at: usize, n: u32) {
            b[at..at + 4].copy_from_slice(&n.to_le_bytes());
        }
        put(&mut raw, 8, 2);
        put(&mut raw, 12, 32);
        put(&mut raw, 32, 6);
        put(&mut raw, 36, 168);
        put(&mut raw, 40, 64);
        put(&mut raw, 44, 4);
        put(&mut raw, 48, 112);
        put(&mut raw, 52, 600);
        put(&mut raw, 72, 0xc0000005);
        put(&mut raw, 224, 256);
        put(&mut raw, 228, 300);
        put(&mut raw, 348, 0x0010_0001);
        raw[548..556].copy_from_slice(&0x1010u64.to_le_bytes());
        put(&mut raw, 600, 1);
        raw[604..612].copy_from_slice(&0x1000u64.to_le_bytes());
        put(&mut raw, 612, 4096);
        put(&mut raw, 680, 48);
        put(&mut raw, 684, 800);
        raw[800..804].copy_from_slice(b"RSDS");
        raw[804..820].fill(1);
        raw[824..838].copy_from_slice(b"poison.pdb.txt");
        raw[900..914].copy_from_slice(b"secret-memory!");
        let version = AppVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 1,
        };
        let build = BuildContext {
            app_version: version,
            os_version_class: OsVersionClass::Windows11,
        };
        let manifest = Manifest {
            version,
            images: vec![oink_telemetry::capture::Image {
                slot: ImageSlot::Agent,
                debug_id: [1; 16],
                text_size: 4096,
            }],
        };
        let crash = extract(&raw, &manifest, build, Component::Agent).unwrap();
        assert_eq!(crash.frames().count(), 1);
        assert_eq!(crash.exception_code, Some(0xc0000005));
        let json = serde_json::to_string(&crash).unwrap();
        assert!(!json.contains("poison"));
        assert!(!json.contains("secret"));
        assert_eq!(
            extract(
                &raw,
                &Manifest {
                    version,
                    images: vec![]
                },
                build,
                Component::Agent
            )
            .unwrap()
            .frames()
            .count(),
            0
        );
        put(&mut raw, 8, u32::MAX);
        assert!(extract(&raw, &manifest, build, Component::Agent).is_none());
    }
}
