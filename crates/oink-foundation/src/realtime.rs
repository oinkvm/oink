//! Scheduling of the realtime threads: input, injection and network threads get
//! their scheduling tier here.
//! Audio threads keep the implementation in `oink-audio`.
//!
//! A realtime thread never waits for an ordinary thread: no lock shared with one, no file I/O, no log formatting.
//!
//! Detection only: after setting, the effective policy is read back and recorded; later
//! read-backs that differ count as "demoted". Nothing restores or tunes automatically.

/// Policy monitoring is diagnostic; queue/input events never wait for this timer.
pub const READBACK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadRole {
    /// Capture, the router, motion pacing. Always `Tier::Realtime`.
    Input,
    /// Native injection (and, in the Windows input service helper, its injection thread).
    /// Always `Tier::Realtime`.
    Inject,
    /// The QUIC endpoint. Either tier; measured per platform, the lower one that passes.
    Network,
}

/// The two tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// macOS: QoS class user-interactive. Windows: `THREAD_PRIORITY_HIGHEST`, process
    /// priority class untouched.
    Elevated,
    /// macOS: time-constraint policy, non-periodic, with the role's [`Budget`].
    /// Windows: MMCSS (task "Games" for input and injection, "Distribution" for network).
    Realtime,
}

/// Declared use for the macOS time-constraint policy: what the thread itself executes per
/// activation, with headroom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub computation: Duration,
    pub constraint: Duration,
}

impl ThreadRole {
    pub const fn budget(self) -> Budget {
        match self {
            Self::Network => Budget {
                computation: Duration::from_micros(500),
                constraint: Duration::from_millis(1),
            },
            Self::Input | Self::Inject => Budget {
                computation: Duration::from_micros(250),
                constraint: Duration::from_millis(1),
            },
        }
    }
}

/// What the OS reports for the calling thread after a set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    pub tier: Tier,
    /// Platform value read back: macOS the policy flavour, Windows the thread priority.
    pub native: i32,
    #[cfg(windows)]
    pub windows: WindowsScheduling,
}

/// Read on the measured owner thread, without allocating or changing its policy.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsScheduling {
    pub process_class: u32,
    pub relative_priority: i32,
    /// Raw ThreadBasicInformation.BasePriority; `Applied::native` is the current scheduler priority.
    pub base_priority: i32,
    pub mmcss_role: Option<ThreadRole>,
    /// AVRT_PRIORITY_NORMAL (0), the registration default.
    pub mmcss_priority: Option<i32>,
}

/// Apply `tier` to the calling thread and read it back. Call once, first thing on the
/// thread. An error is reported by the caller (the thread keeps running at its default).
pub fn apply(role: ThreadRole, tier: Tier) -> std::io::Result<Applied> {
    platform::apply(role, tier)
}

/// Read back the calling thread's effective policy. Cheap enough for a periodic check;
/// must not allocate or block.
pub fn read_back() -> std::io::Result<Applied> {
    platform::read_back()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::io;
    const TIME_CONSTRAINT: i32 = 2;
    const USER_INTERACTIVE: u32 = 0x21;
    #[repr(C)]
    #[derive(Default)]
    struct TimeConstraint {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn pthread_self() -> *mut std::ffi::c_void;
        fn pthread_mach_thread_np(thread: *mut std::ffi::c_void) -> u32;
        fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32;
        fn pthread_get_qos_class_np(thread: *mut std::ffi::c_void, class: *mut u32, priority: *mut i32) -> i32;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
        fn thread_policy_set(thread: u32, flavor: i32, policy: *const i32, count: u32) -> i32;
        fn thread_policy_get(thread: u32, flavor: i32, policy: *mut i32, count: *mut u32, default: *mut i32) -> i32;
    }
    fn check(code: i32) -> io::Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code))
        }
    }
    pub(super) fn apply(role: ThreadRole, tier: Tier) -> io::Result<Applied> {
        // SAFETY: operates only on this thread; every policy buffer has the ABI's four words.
        unsafe {
            match tier {
                Tier::Elevated => check(pthread_set_qos_class_self_np(USER_INTERACTIVE, 0))?,
                Tier::Realtime => {
                    let mut timebase = Timebase::default();
                    check(mach_timebase_info(&mut timebase))?;
                    let ticks =
                        |d: Duration| (d.as_nanos() * u128::from(timebase.denom) / u128::from(timebase.numer)) as u32;
                    let budget = role.budget();
                    // Non-periodic capture/network work must not claim an audio period.
                    let policy = TimeConstraint {
                        period: 0,
                        computation: ticks(budget.computation),
                        constraint: ticks(budget.constraint),
                        preemptible: 1,
                    };
                    check(thread_policy_set(
                        pthread_mach_thread_np(pthread_self()),
                        TIME_CONSTRAINT,
                        (&policy as *const TimeConstraint).cast(),
                        4,
                    ))?;
                }
            }
        }
        read_back()
    }
    pub(super) fn read_back() -> io::Result<Applied> {
        let mut policy = TimeConstraint::default();
        let mut count = 4;
        let mut default = 0;
        // SAFETY: this thread's borrowed Mach port, correctly sized writable buffers.
        unsafe {
            check(thread_policy_get(
                pthread_mach_thread_np(pthread_self()),
                TIME_CONSTRAINT,
                (&mut policy as *mut TimeConstraint).cast(),
                &mut count,
                &mut default,
            ))?;
            if default == 0 {
                return Ok(Applied {
                    tier: Tier::Realtime,
                    native: TIME_CONSTRAINT,
                });
            }
            let mut qos = 0;
            let mut priority = 0;
            check(pthread_get_qos_class_np(pthread_self(), &mut qos, &mut priority))?;
            Ok(Applied {
                tier: Tier::Elevated,
                native: qos as i32,
            })
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{cell::RefCell, io};
    use windows_sys::Win32::{Foundation::HANDLE, System::Threading::*};
    struct Registration(HANDLE, ThreadRole);
    impl Drop for Registration {
        fn drop(&mut self) {
            // SAFETY: TLS drops this registration on the same thread that joined MMCSS.
            unsafe {
                AvRevertMmThreadCharacteristics(self.0);
            }
        }
    }
    thread_local! { static REGISTRATION: RefCell<Option<Registration>> = const { RefCell::new(None) }; }
    pub(super) fn apply(role: ThreadRole, tier: Tier) -> io::Result<Applied> {
        REGISTRATION.with(|slot| -> io::Result<()> {
            slot.borrow_mut().take();
            // SAFETY: current thread only; static NUL-terminated task names, valid index output.
            unsafe {
                match tier {
                    Tier::Elevated => {
                        if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) == 0 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    Tier::Realtime => {
                        let name: &[u16] = match role {
                            ThreadRole::Network => &[68, 105, 115, 116, 114, 105, 98, 117, 116, 105, 111, 110, 0],
                            ThreadRole::Input | ThreadRole::Inject => &[71, 97, 109, 101, 115, 0],
                        };
                        let mut index = 0;
                        let handle = AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut index);
                        if handle.is_null() {
                            return Err(io::Error::last_os_error());
                        }
                        *slot.borrow_mut() = Some(Registration(handle, role));
                    }
                }
            }
            Ok(())
        })?;
        read_back()
    }
    pub(super) fn read_back() -> io::Result<Applied> {
        // GetThreadPriority reports the relative Win32 base priority, not the scheduler
        // priority MMCSS applies. ThreadBasicInformation reads the latter directly.
        #[repr(C)]
        #[derive(Default)]
        struct Basic {
            exit_status: i32,
            teb: usize,
            client: [usize; 2],
            affinity: usize,
            priority: i32,
            base_priority: i32,
        }
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtQueryInformationThread(
                thread: HANDLE,
                class: i32,
                info: *mut Basic,
                size: u32,
                returned: *mut u32,
            ) -> i32;
            fn RtlNtStatusToDosError(status: i32) -> u32;
        }
        let mut basic = Basic::default();
        // SAFETY: current thread pseudo-handle, ABI-sized writable basic-info record.
        let status = unsafe {
            NtQueryInformationThread(
                GetCurrentThread(),
                0,
                &mut basic,
                std::mem::size_of::<Basic>() as u32,
                std::ptr::null_mut(),
            )
        };
        if status < 0 {
            // SAFETY: converts a returned NT status; no pointer arguments.
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            ));
        }
        let native = basic.priority;
        let tier = if native >= 16 { Tier::Realtime } else { Tier::Elevated };
        // SAFETY: read-only pseudo-handles for this process and calling thread.
        let (process_class, relative_priority) = unsafe {
            (
                GetPriorityClass(GetCurrentProcess()),
                GetThreadPriority(GetCurrentThread()),
            )
        };
        let mmcss_role = REGISTRATION.with(|slot| slot.borrow().as_ref().map(|r| r.1));
        Ok(Applied {
            tier,
            native,
            windows: WindowsScheduling {
                process_class,
                relative_priority,
                base_priority: basic.base_priority,
                mmcss_role,
                mmcss_priority: mmcss_role.map(|_| 0),
            },
        })
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;
    pub(super) fn apply(_: ThreadRole, _: Tier) -> std::io::Result<Applied> {
        read_back()
    }
    pub(super) fn read_back() -> std::io::Result<Applied> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "realtime scheduling unavailable",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budgets_fit_the_constraint() {
        for role in [ThreadRole::Input, ThreadRole::Inject, ThreadRole::Network] {
            let b = role.budget();
            assert!(b.computation > Duration::ZERO && b.computation < b.constraint);
        }
    }
    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn elevated_thread_policy_can_be_read_back() {
        std::thread::spawn(|| {
            let applied = apply(ThreadRole::Network, Tier::Elevated).unwrap();
            assert_eq!(applied, read_back().unwrap());
            assert_eq!(applied.tier, Tier::Elevated);
        })
        .join()
        .unwrap();
    }
    #[test]
    #[cfg(windows)]
    fn helper_mmcss_priority_is_read_from_the_scheduler() {
        std::thread::spawn(|| {
            let applied = apply(ThreadRole::Inject, Tier::Realtime).unwrap();
            let read = read_back().unwrap();
            assert_eq!(applied, read);
            assert_eq!(read.tier, Tier::Realtime);
            assert!((16..=31).contains(&read.native));
        })
        .join()
        .unwrap();
    }
}
