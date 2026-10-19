//! Process startup policy shared by Oink executables.

/// Set Windows process scheduling to Normal; other platforms are unchanged.
///
/// Call as the first action in `main`, before argument dispatch (including
/// `--autostart`, `--service` and same-executable helpers), logging or application
/// threads. A failure must be reported by the caller before continuing startup.
/// This does not change relative thread priority settings, priority boosts or
/// audio MMCSS.
pub fn initialize_priority() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, NORMAL_PRIORITY_CLASS, SetPriorityClass};
        // Task Scheduler defaults to Priority 7 (BelowNormal); Windows inherits
        // Idle/BelowNormal from a launcher unless explicitly overridden. Setting a
        // child's class after spawning is too late: it may
        // already have spawned its own host. Normalize at every entry point,
        // including helper dispatch, so unrelated launchers cannot set our policy.
        // Reproduced with Task Scheduler on Windows 11. Normal restores ordinary
        // scheduling; High/Realtime would let unrelated UI/network/logging work
        // compete with other apps and system work. Per-thread/MMCSS scheduling
        // remains the subsystem's decision.
        // SAFETY: the pseudo-handle refers to this process and needs no closing;
        // NORMAL_PRIORITY_CLASS is a documented, non-realtime scheduling class.
        if unsafe { SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
