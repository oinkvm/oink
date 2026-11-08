//! Separate local and grant-armed throw capture. Raw local exception text has
//! no ZIP/network reader; the report snapshot excludes reason and arbitrary names.
use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::IntoRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
unsafe extern "C" {
    fn oink_objc_exception_output(fd: libc::c_int);
}

/// One owner per process. Dropping stops observation but keeps local evidence for
/// the next launch; explicit diagnostic disable must call `erase` after dropping.
#[derive(Debug)]
pub struct Capture {
    _private: (),
}
impl Capture {
    pub fn start(directory: &Path) -> io::Result<Self> {
        if ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        match prepare(directory) {
            Ok(file) => {
                // SAFETY: transfers this unique descriptor to native code, which
                // closes it only after any in-flight recording has completed.
                unsafe { oink_objc_exception_output(file.into_raw_fd()) };
                Ok(Self { _private: () })
            }
            Err(error) => {
                ACTIVE.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: -1 disarms the process-local descriptor; no Rust callbacks.
        unsafe { oink_objc_exception_output(-1) };
        ACTIVE.store(false, Ordering::Release);
    }
}
fn prepare(directory: &Path) -> io::Result<File> {
    std::fs::create_dir_all(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let current = directory.join("objc-exceptions.jsonl");
    // Keep the last nonempty process's evidence across crash/restart loops with
    // no subsequent throw. At most two 128 KiB files, each with eight records.
    if std::fs::symlink_metadata(&current).is_ok_and(|m| m.is_file() && m.len() > 0) {
        std::fs::rename(&current, directory.join("objc-exceptions.previous.jsonl"))?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(current)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
/// The caller has already dropped the capture, fencing in-flight writes.
pub fn erase(directory: &Path) -> io::Result<()> {
    for name in ["objc-exceptions.jsonl", "objc-exceptions.previous.jsonl"] {
        match std::fs::remove_file(directory.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

unsafe extern "C" {
    fn oink_objc_exception_report_output(fd: libc::c_int);
}
/// Grant-armed numeric throw snapshot, independent of local diagnostic recording.
/// Contains no reason, raw PC, path or arbitrary name; recovery requires an
/// attributable SIGABRT .ips from this lifetime before it can enter a report.
#[derive(Debug)]
pub struct ReportCapture {
    _private: (),
}
impl ReportCapture {
    pub fn start(path: &Path) -> io::Result<Self> {
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        // SAFETY: transfers the unique descriptor; native configuration fences writes.
        unsafe { oink_objc_exception_report_output(file.into_raw_fd()) };
        Ok(Self { _private: () })
    }
}
impl Drop for ReportCapture {
    fn drop(&mut self) {
        // SAFETY: disarms only the grant-armed snapshot; local capture is separate.
        unsafe { oink_objc_exception_report_output(-1) };
    }
}
