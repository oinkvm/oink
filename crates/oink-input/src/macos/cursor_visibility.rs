//! Read-only distinction between hide-count hiding and hide-until-move obscure.
//!
//! CGCursorIsVisible merges both states. A CGEvent move can leave typing obscure
//! set indefinitely. A readable cause excludes typing-only hiding; when unavailable,
//! detection falls back to visibility plus the existing movement evidence. Ordinary
//! remote HID motion can clear the server's cursor state while the shared obscure
//! byte remains set on macOS 26.6.2. CGCursorIsVisible reads these same bytes: their
//! agreement validates the reader, not rendered visibility. In that stale state
//! hide-count remains zero, so the precise path excludes typing; the visibility-only
//! fallback needs the separate cursor_refresh reconciliation before accepting
//! ambiguous movement evidence. This module itself remains read-only.
//! No suitable public/exported query was found on the validated systems. On arm64 we
//! recognize the exact visibility reader and cached-scoreboard load used by
//! macOS 14.7.8 / 26.6.2, then copy only its two Boolean bytes from our own address
//! space. We never call an unexported function, dereference a guessed pointer,
//! write shared state, post input, or change another application's hide count.
//! Unknown code/layout, an unreadable address, or an inconsistent sample is None:
//! Auto uses its visibility fallback; manual On and absolute input are unchanged.
//!
//! Native calibration covered obscure, NSCursor/CGDisplay hide, both together,
//! and decoupling, including an independent background reader on both systems.
//! Chromium 149 Pointer Lock retained the hide bit through release/recapture.
//! This is an undocumented layout dependency, not proof of pointer lock: hide-only
//! applications remain ambiguous. Other releases are unverified;
//! x86_64 returns unknown without attempting the shared-memory read.
//! The OS version is diagnostic metadata, never an allowlist: eligibility is
//! determined by the loaded code structure. Matching structure on another arm64
//! release is read through the same path; a mismatch is unknown before data reads.

use std::sync::{Mutex, OnceLock};

/// None means the cause is unavailable; detectors then use visibility/motion evidence.
pub(super) fn application_hidden() -> Option<bool> {
    #[cfg(target_arch = "aarch64")]
    let result = arm64::read();
    #[cfg(not(target_arch = "aarch64"))]
    let result: Result<bool, &str> = Err("unsupported_architecture");
    // Report once at startup or when the failure category changes, never at the
    // 50-ms sampling frequency. Query even when visible, so an unsupported system
    // reports its capability without waiting for the user to enter a game.
    static LAST: Mutex<Option<Option<&'static str>>> = Mutex::new(None);
    let failure = result.as_ref().err().copied();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last != Some(failure) {
        let previous = last.replace(failure);
        if let Some(reason) = failure {
            tracing::warn!(
                os_version = os_version(),
                reason,
                "cursor hiding cause unavailable; using visibility-only automatic pointer capture"
            );
        } else if previous.is_some() {
            tracing::info!(
                os_version = os_version(),
                reason = "hide_reason_available",
                "cursor hiding cause available again"
            );
        }
    }
    result.ok()
}

fn os_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut bytes = [0u8; 64];
        let mut size = bytes.len();
        // SAFETY: read-only sysctl into the supplied buffer; its size is in/out.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.osproductversion".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0
            && let Ok(value) = std::ffi::CStr::from_bytes_until_nul(&bytes)
        {
            value.to_string_lossy().into_owned()
        } else {
            "unknown".into()
        }
    })
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    use super::OnceLock;

    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_read_overwrite(task: u32, address: u64, size: u64, data: u64, out_size: *mut u64) -> i32;
    }

    fn bytes<const N: usize>(address: u64) -> Option<[u8; N]> {
        let mut data = [0; N];
        let mut copied = 0;
        // SAFETY: the kernel validates the source range in our own task. The
        // destination is an initialized N-byte buffer; no foreign task access.
        let rc = unsafe {
            mach_vm_read_overwrite(
                mach_task_self_,
                address,
                N as u64,
                data.as_mut_ptr() as u64,
                &mut copied,
            )
        };
        (rc == 0 && copied == N as u64).then_some(data)
    }

    fn pointer(address: u64) -> Option<u64> {
        let value = u64::from_le_bytes(bytes(address)?);
        (value != 0).then_some(value)
    }

    fn words<const N: usize, const B: usize>(address: u64) -> Option<[u32; N]> {
        let data = bytes::<B>(address)?;
        let mut words = [0; N];
        for (word, chunk) in words.iter_mut().zip(data.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*chunk);
        }
        (B == N * 4).then_some(words)
    }

    fn resolve() -> Result<u64, &'static str> {
        let visible = super::super::system::find_symbol(c"CGCursorIsVisible") as u64;
        if visible == 0 {
            return Err("visibility_symbol_missing");
        }
        // The caller already used CGCursorIsVisible, which initializes Apple's
        // cached scoreboard. Resolve its storage; reload the pointers per sample
        // so a WindowServer/session change cannot leave a cached stale mapping.
        let code = words::<19, 76>(visible).ok_or("memory_read_failed")?;
        let getter = visibility_getter(visible, &code).ok_or("visibility_pattern_changed")?;
        let load = words::<24, 96>(getter).ok_or("memory_read_failed")?;
        scoreboard_storage(getter, &load).ok_or("scoreboard_pattern_changed")
    }

    pub(super) fn read() -> Result<bool, &'static str> {
        static STORAGE: OnceLock<Result<u64, &'static str>> = OnceLock::new();
        let storage = (*STORAGE.get_or_init(resolve))?;
        let scoreboard = pointer(storage).ok_or("scoreboard_unavailable")?;
        let shared = pointer(scoreboard).ok_or("scoreboard_unavailable")?;
        let address = shared.checked_add(0x2f).ok_or("scoreboard_unavailable")?;
        let before = bytes::<2>(address).ok_or("memory_read_failed")?;
        #[allow(deprecated)]
        let visible = objc2_core_graphics::CGCursorIsVisible();
        let after = bytes::<2>(address).ok_or("memory_read_failed")?;
        if before != after {
            return Err("inconsistent_visibility");
        }
        classify(after, visible).ok_or("inconsistent_visibility")
    }

    fn classify([hidden, obscured]: [u8; 2], visible: bool) -> Option<bool> {
        if hidden > 1 || obscured > 1 || visible != (hidden == 0 && obscured == 0) {
            return None;
        }
        // A game can obscure while holding hide, e.g. on a key press. Only the
        // independent hide bit is the qualifier; "recent key" is not evidence.
        Some(hidden == 1)
    }

    fn branch(pc: u64, op: u32) -> Option<u64> {
        if op & 0xfc00_0000 != 0x9400_0000 {
            return None;
        }
        let offset = (i64::from(op & 0x03ff_ffff) << 38) >> 36;
        pc.checked_add_signed(offset)
    }

    fn visibility_getter(base: u64, code: &[u32; 19]) -> Option<u64> {
        // Complete implementation, including both byte offsets and all branches.
        // 14.7 uses cbz w8; 26 uses tbz w8,#0. Both are equivalent for these Booleans.
        let expected = [
            0xd503_237f,
            0xa9bf_7bfd,
            0x9100_03fd,
            0,
            0xb400_00c0,
            0xf940_0008,
            0x3940_bd08,
            0x3600_0068,
            0x5280_0000,
            0x1400_0008,
            0,
            0xb400_00a0,
            0xf940_0008,
            0x3940_c108,
            0x5200_0100,
            0x1400_0002,
            0x5280_0020,
            0xa8c1_7bfd,
            0xd65f_0fff,
        ];
        for (i, (&actual, &expected)) in code.iter().zip(&expected).enumerate() {
            if i == 3 || i == 10 || (i == 7 && actual == 0x3400_0068) {
                continue;
            }
            if actual != expected {
                return None;
            }
        }
        let getter = branch(base.checked_add(12)?, code[3])?;
        (getter == branch(base.checked_add(40)?, code[10])?).then_some(getter)
    }

    fn scoreboard_storage(base: u64, code: &[u32; 24]) -> Option<u64> {
        // Validate the whole cached fast path through its return. Address
        // immediates vary with the system image; register/data flow must match.
        let pattern = [
            (0xd503_237f, u32::MAX),
            (0xd101_43ff, u32::MAX),
            (0xa902_57f6, u32::MAX),
            (0xa903_4ff4, u32::MAX),
            (0xa904_7bfd, u32::MAX),
            (0x9101_03fd, u32::MAX),
            (0x9000_0008, 0x9f00_001f),
            (0xf940_0108, 0xffc0_03ff),
            (0xf940_0108, u32::MAX),
            (0xf900_0fe8, u32::MAX),
            (0x9000_0014, 0x9f00_001f),
            (0xf940_0280, 0xffc0_03ff),
            (0xb400_0180, u32::MAX),
            (0xf940_0fe8, u32::MAX),
            (0x9000_0009, 0x9f00_001f),
            (0xf940_0129, 0xffc0_03ff),
            (0xf940_0129, u32::MAX),
            (0xeb08_013f, u32::MAX),
            (0x5400_03c1, u32::MAX),
            (0xa944_7bfd, u32::MAX),
            (0xa943_4ff4, u32::MAX),
            (0xa942_57f6, u32::MAX),
            (0x9101_43ff, u32::MAX),
            (0xd65f_0fff, u32::MAX),
        ];
        if code.iter().zip(pattern).any(|(&op, (value, mask))| op & mask != value) {
            return None;
        }
        let op = code[10]; // adrp x20, page; ldr x0,[x20,#offset]
        let imm = i64::from(((op >> 29) & 3) | (((op >> 5) & 0x7ffff) << 2));
        let page_delta = ((imm << 43) >> 43) << 12;
        (base.checked_add(40)? & !0xfff)
            .checked_add_signed(page_delta)?
            .checked_add(u64::from((code[11] >> 10) & 0xfff) * 8)
    }

    #[cfg(all(test, debug_assertions))]
    mod tests {
        use super::*;

        #[test]
        fn hide_is_independent_of_typing_obscure_and_invalid_reads_are_unknown() {
            assert_eq!(classify([0, 0], true), Some(false));
            assert_eq!(classify([0, 1], false), Some(false));
            assert_eq!(classify([1, 0], false), Some(true));
            assert_eq!(classify([1, 1], false), Some(true));
            for bytes in [[2, 0], [0, 2], [255, 255], [0, 0]] {
                assert_eq!(classify(bytes, false), None);
            }
            assert_eq!(classify([1, 0], true), None);
            assert_eq!(pointer(0), None);
            assert_eq!(bytes::<2>(u64::MAX), None);
        }

        #[test]
        fn visibility_decoder_rejects_changed_offsets_registers_branches_and_getters() {
            let code = [
                0xd503237f, 0xa9bf7bfd, 0x910003fd, 0x97f37fd4, 0xb40000c0, 0xf9400008, 0x3940bd08, 0x36000068,
                0x52800000, 0x14000008, 0x97f37fcd, 0xb40000a0, 0xf9400008, 0x3940c108, 0x52000100, 0x14000002,
                0x52800020, 0xa8c17bfd, 0xd65f0fff,
            ];
            let base = 0x1893ffc68;
            assert_eq!(visibility_getter(base, &code), Some(0x1890dfbc4));
            let mut old = code;
            old[7] = 0x34000068;
            assert_eq!(visibility_getter(base, &old), Some(0x1890dfbc4));
            for i in 0..code.len() {
                let mut changed = code;
                changed[i] ^= 1;
                assert_eq!(visibility_getter(base, &changed), None, "instruction {i}");
            }
            assert_eq!(branch(4, 0x97ffffff), Some(0));
            assert_eq!(branch(0, 0x97ffffff), None);
            assert_eq!(branch(0, 0), None);
        }

        #[test]
        fn scoreboard_decoder_requires_the_cached_load_data_flow() {
            let code = [
                0xd503237f, 0xd10143ff, 0xa90257f6, 0xa9034ff4, 0xa9047bfd, 0x910103fd, 0xf02fa668, 0xf9472108,
                0xf9400108, 0xf9000fe8, 0xd03315f4, 0xf946a280, 0xb4000180, 0xf9400fe8, 0xf02fa669, 0xf9472129,
                0xf9400129, 0xeb08013f, 0x540003c1, 0xa9447bfd, 0xa9434ff4, 0xa94257f6, 0x910143ff, 0xd65f0fff,
            ];
            assert_eq!(scoreboard_storage(0x1890dfbc4, &code), Some(0x1ef39dd40));
            for i in 0..code.len() {
                let mut changed = code;
                changed[i] ^= 1; // Changes the register or fixed operation, not a relocated address.
                assert_eq!(scoreboard_storage(0x1890dfbc4, &changed), None, "instruction {i}");
            }
        }
    }
}
