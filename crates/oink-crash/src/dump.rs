//! Local-only minidump stack walking. No symbol names, memory, registers or
//! runtime module paths cross the typed projection. No HTTP symbol features.
use minidump::{Minidump, MinidumpException, MinidumpModuleList, MinidumpSystemInfo, MinidumpThreadList, Module};
use oink_telemetry::{
    capture::{Image, Manifest},
    *,
};
use std::path::Path;
pub const MAX_DUMP: u64 = 32 * 1024 * 1024;

/// PE CodeView identity and executable range; also used by isolated crash tests.
pub fn executable_image(path: &Path, slot: ImageSlot) -> Option<Image> {
    use object::{Object, ObjectSection};
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > 128 * 1024 * 1024 {
        return None;
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    let object = object::File::parse(bytes.as_slice()).ok()?;
    let pdb = object.pdb_info().ok()??;
    let mut id = pdb.guid();
    id[..4].reverse();
    id[4..6].reverse();
    id[6..8].reverse();
    let base = object.relative_address_base();
    let text_size = object
        .sections()
        .filter(|s| s.kind() == object::SectionKind::Text)
        .map(|s| s.address().saturating_sub(base) + s.size())
        .max()?;
    // PE code id is timestamp + SizeOfImage, not the PDB age.
    let pe = u32::from_le_bytes(bytes.get(60..64)?.try_into().ok()?) as usize;
    let timestamp = u32::from_le_bytes(bytes.get(pe + 8..pe + 12)?.try_into().ok()?);
    let size = u32::from_le_bytes(bytes.get(pe + 80..pe + 84)?.try_into().ok()?);
    Some(Image {
        slot,
        debug_id: id,
        debug_age: pdb.age(),
        code_id: Some((u64::from(timestamp) << 32) | u64::from(size)),
        text_size,
    })
}
pub fn exception_class(code: u32) -> ExceptionClass {
    match code {
        0xc0000005 => ExceptionClass::BadAccess,
        0xc000001d => ExceptionClass::BadInstruction,
        0xc0000094 | 0xc000008e => ExceptionClass::Arithmetic,
        0x80000003 => ExceptionClass::Breakpoint,
        0x40000015 => ExceptionClass::Abort,
        _ => ExceptionClass::Unknown,
    }
}

fn same_directory(left: &Path, right: &Path) -> bool {
    // DbgHelp's module paths can differ in casing from SystemRoot and the
    // executable path. Compare these fixed ASCII Windows directory components
    // case-insensitively (verified on Windows 11); GUID/age matching still binds
    // the actual file. This does not authorize any additional directory.
    #[cfg(windows)]
    {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}
fn preferred(component: Component) -> ImageSlot {
    match component {
        Component::UiRust | Component::UiWebview => ImageSlot::UiRust,
        Component::FileProvider => ImageSlot::FileProvider,
        Component::Agent => ImageSlot::Agent,
    }
}
// Validate raw descriptor counts/ranges before rust-minidump can decode strings.
// Repeated RVAs count once PER descriptor: file size alone does not bound the
// decoded allocation. This parser only accepts the little-endian Windows dump.
fn bounded_layout(bytes: &[u8]) -> Option<()> {
    fn u32_at(bytes: &[u8], at: usize) -> Option<usize> {
        Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
    }
    fn region(bytes: &[u8], at: usize, len: usize) -> Option<&[u8]> {
        bytes.get(at..at.checked_add(len)?)
    }
    fn charge(bytes: &[u8], at: usize, len: usize, budget: &mut usize) -> Option<()> {
        if len > 65536 {
            return None;
        }
        *budget = budget.checked_sub(len)?;
        region(bytes, at, len)?;
        Some(())
    }
    if bytes.get(..4)? != b"MDMP" {
        return None;
    }
    region(bytes, 0, 32)?;
    let streams = u32_at(bytes, 8)?;
    if streams > 128 {
        return None;
    }
    let directory = u32_at(bytes, 12)?;
    region(bytes, directory, streams.checked_mul(12)?)?;
    let mut decoded_budget = 1024 * 1024;
    for index in 0..streams {
        let entry = directory + index * 12;
        let kind = u32_at(bytes, entry)?;
        let length = u32_at(bytes, entry + 4)?;
        let rva = u32_at(bytes, entry + 8)?;
        region(bytes, rva, length)?;
        match kind {
            4 => {
                // MINIDUMP_MODULE_LIST / 108-byte MINIDUMP_MODULE descriptors.
                let count = u32_at(bytes, rva)?;
                if count > 4096 || count.checked_mul(108)?.checked_add(4)? > length {
                    return None;
                }
                for index in 0..count {
                    let module = rva + 4 + index * 108;
                    let name = u32_at(bytes, module + 20)?;
                    let length = u32_at(bytes, name)?;
                    if !length.is_multiple_of(2) {
                        return None;
                    }
                    charge(bytes, name + 4, length, &mut decoded_budget)?;
                    // CodeView may carry a PDB filename; bound it before decoding too.
                    for descriptor in [module + 76, module + 84] {
                        let length = u32_at(bytes, descriptor)?;
                        let rva = u32_at(bytes, descriptor + 4)?;
                        charge(bytes, rva, length, &mut decoded_budget)?;
                    }
                }
            }
            3 => {
                // MINIDUMP_THREAD_LIST: bound raw-vector allocations too.
                let count = u32_at(bytes, rva)?;
                if count > 1024 || count.checked_mul(48)?.checked_add(4)? > length {
                    return None;
                }
            }
            7 => {
                // MINIDUMP_SYSTEM_INFO's optional CSD UTF-16 string.
                if length < 56 {
                    return None;
                }
                let name = u32_at(bytes, rva + 24)?;
                if name != 0 {
                    let length = u32_at(bytes, name)?;
                    charge(bytes, name + 4, length, &mut decoded_budget)?;
                }
            }
            _ => {}
        }
    }
    Some(())
}
pub fn extract(bytes: &[u8], manifest: &Manifest, build: BuildContext, component: Component) -> Option<SafeCrash> {
    let directory = std::env::current_exe().ok()?.parent()?.to_path_buf();
    extract_at(bytes, manifest, build, component, &directory)
}
pub fn extract_at(
    bytes: &[u8],
    manifest: &Manifest,
    build: BuildContext,
    component: Component,
    binaries: &Path,
) -> Option<SafeCrash> {
    if bytes.len() as u64 > MAX_DUMP || build.app_version != manifest.version {
        return None;
    }
    bounded_layout(bytes)?;
    let dump = Minidump::read(bytes).ok()?;
    let exception = dump.get_stream::<MinidumpException<'_>>().ok()?;
    let code = exception.raw.exception_record.exception_code;
    let class = exception_class(code);
    let mut crash = SafeCrash::empty(build, component, CrashKind::NativeException, class);
    crash.exception_code = Some(code);
    let Ok(system) = dump.get_stream::<MinidumpSystemInfo>() else {
        return Some(crash);
    };
    // The debuginfo provider supports these CPUs; unknown layouts never guess.
    if !matches!(
        system.cpu,
        minidump::system_info::Cpu::X86_64 | minidump::system_info::Cpu::Arm64
    ) {
        return Some(crash);
    }
    let context = exception.context(&system, None)?.into_owned();
    let modules = dump.get_stream::<MinidumpModuleList>().ok()?;
    // Do not open dump-supplied paths (including UNC shares). Only allow local
    // fixed binary directories, and verify the on-disk GUID+age against the dump.
    #[allow(unused_mut)]
    let mut allowed = vec![binaries.to_path_buf()];
    // Cargo integration children live in deps beside the collector; packaged
    // installs have no such directory. The GUID+age check applies in either case.
    allowed.push(binaries.join("deps"));
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        allowed.push(Path::new(&root).join("System32"));
    }
    let mut binary_budget = 256 * 1024 * 1024u64;
    let local = MinidumpModuleList::from_modules(
        modules
            .iter()
            .filter_map(|m| {
                let path = Path::new(&m.name);
                #[cfg(windows)]
                if !crate::local::local_path(path) {
                    return None;
                }
                if !allowed
                    .iter()
                    .any(|root| path.parent().is_some_and(|parent| same_directory(parent, root)))
                {
                    return None;
                }
                let size = std::fs::metadata(path).ok()?.len();
                binary_budget = binary_budget.checked_sub(size)?;
                let disk = executable_image(path, preferred(component))?;
                let id = m.debug_identifier()?;
                if disk.debug_id != *id.uuid().as_bytes() || disk.debug_age != id.appendix() {
                    return None;
                }
                let mut module = m.clone();
                // GUID/age was validated above. Strip embedded debug-file paths so
                // the offline provider can only read this already-approved binary.
                module.codeview_info = None;
                module.misc_info = None;
                Some(module)
            })
            .take(128)
            .collect(),
    );
    let provider = futures_executor::block_on(minidump_unwind::symbols::debuginfo::DebugInfoSymbolProvider::new(
        &system, &local,
    ));
    let threads = dump.get_stream::<MinidumpThreadList<'_>>().ok()?;
    let thread = threads
        .threads
        .iter()
        .find(|t| t.raw.thread_id == exception.raw.thread_id)?;
    let memory = dump.get_memory().unwrap_or_default();
    let stack_memory = thread.stack_memory(&memory);
    let mut stack = minidump_unwind::CallStack::with_context(context);
    let info = minidump_unwind::SystemInfo {
        os: system.os,
        cpu: system.cpu,
        os_version: None,
        os_build: None,
        cpu_info: None,
        cpu_microcode_version: None,
        cpu_count: 0,
    };
    // Processing is isolated in the collector with a wall-clock watchdog. Cap
    // pathological stacks too; retain the partial walk on this private sentinel.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        futures_executor::block_on(minidump_unwind::walk_stack(
            0,
            |index, _: &minidump_unwind::StackFrame| {
                if index >= 256 {
                    std::panic::panic_any(());
                }
            },
            &mut stack,
            stack_memory,
            &modules,
            &info,
            &provider,
        ))
    }));
    for (slot, frame) in crash
        .frames
        .iter_mut()
        .flatten()
        .zip(stack.frames.iter().filter_map(|frame| {
            let module = modules.module_at_address(frame.instruction)?;
            let id = module.debug_identifier()?;
            let image = manifest.image(*id.uuid().as_bytes(), preferred(component))?;
            let offset = frame.instruction.checked_sub(module.base_address())?;
            if image.debug_age != id.appendix() || offset >= image.text_size {
                return None;
            }
            Some(Frame::Native {
                image: image.slot,
                offset,
                debug_id: image.debug_id,
                debug_age: image.debug_age,
                code_id: image.code_id,
                image_size: image.text_size,
            })
        }))
    {
        *slot = Some(frame);
    }
    Some(crash)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_limits_precede_any_module_name_decoding() {
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
            images: vec![],
        };
        // Repeated RVAs must count repeatedly against decoded allocation budgets.
        for (count, name_length, cv_length) in [(4097, 0, 0u32), (32, 65536, 0), (32, 0, 65536)] {
            let names = 300 + count * 108;
            let mut raw = vec![0u8; names + 65540];
            raw[..4].copy_from_slice(b"MDMP");
            for (at, value) in [
                (4, 0xa793u32),
                (8, 2),
                (12, 32),
                (32, 6),
                (36, 168),
                (40, 64),
                (44, 4),
                (48, (4 + count * 108) as u32),
                (52, 256),
                (72, 0xc0000005),
                (256, count as u32),
                (names, name_length),
            ] {
                raw[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
            for index in 0..count {
                let record = 260 + index * 108;
                raw[record + 20..record + 24].copy_from_slice(&(names as u32).to_le_bytes());
                raw[record + 76..record + 80].copy_from_slice(&cv_length.to_le_bytes());
                raw[record + 80..record + 84].copy_from_slice(&(names as u32).to_le_bytes());
            }
            assert!(
                extract(&raw, &manifest, build, Component::Agent).is_none(),
                "count={count} name={name_length} cv={cv_length}"
            );
        }
    }
    #[test]
    fn raw_names_memory_registers_and_unshipped_modules_never_cross_projection() {
        let mut raw = vec![0u8; 1024];
        raw[..4].copy_from_slice(b"MDMP");
        raw[4..8].copy_from_slice(&0xa793u32.to_le_bytes());
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
        put(&mut raw, 624, 960);
        put(&mut raw, 960, 20);
        for (index, word) in "poison.dll".encode_utf16().enumerate() {
            raw[964 + index * 2..966 + index * 2].copy_from_slice(&word.to_le_bytes());
        }
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
                debug_age: 0,
                code_id: None,
                text_size: 4096,
            }],
        };
        let crash = extract(&raw, &manifest, build, Component::Agent).unwrap();
        assert_eq!(crash.frames().count(), 0);
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
