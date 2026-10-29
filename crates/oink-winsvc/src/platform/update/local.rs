//! Local MSI admission requires a real elevated installed CLI, after UAC. A path,
//! hash, expected build or medium integrity Oink process is never authorization.
use super::{msi, protected};
use oink_update::{Error, local::Package};
use sha2::{Digest, Sha256};
use std::{io::Read, os::windows::fs::OpenOptionsExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{Com::*, Threading::*},
    UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
};

pub fn inspect(path: &Path) -> Result<Package, Error> {
    let (_, package) = read(path)?;
    Ok(package)
}
/// Run only in the receiving user session. Following a Cloud Files placeholder
/// here is user-authorized filesystem I/O; SYSTEM never follows the source.
pub fn spool(path: &Path, destination: &Path) -> Result<Package, Error> {
    use std::io::Write;
    let (bytes, _) = read_mode(path, true)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .open(destination)
        .map_err(|_| Error::Storage)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| Error::Storage)?;
    // Display metadata from the copied bytes, not from a user-controlled source
    // path that could be renamed while the original read handle stays open.
    drop(file);
    drop(bytes);
    inspect(destination)
}
pub(super) fn read(path: &Path) -> Result<(Vec<u8>, Package), Error> {
    read_mode(path, false)
}
fn read_mode(path: &Path, receiving_user: bool) -> Result<(Vec<u8>, Package), Error> {
    if !path.is_absolute() || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("msi")) {
        return Err(Error::WrongProduct);
    }
    let name = path.to_str().ok_or(Error::Storage)?;
    // No network paths, device namespaces, alternate streams, or argument text.
    if name.starts_with(r"\\") || name.contains('"') || name.contains('\0') || name[2..].contains(':') {
        return Err(Error::WrongProduct);
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(if receiving_user {
            0
        } else {
            FILE_FLAG_OPEN_REPARSE_POINT
        })
        .open(path)
        .map_err(|_| Error::Storage)?;
    let metadata = file.metadata().map_err(|_| Error::Storage)?;
    use std::os::windows::fs::MetadataExt;
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
        || (!receiving_user && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    {
        return Err(Error::CorruptPackage);
    }
    if metadata.len() == 0 || metadata.len() > oink_update::feed::MAX_PACKAGE {
        return Err(Error::Limit);
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(oink_update::feed::MAX_PACKAGE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Storage)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(Error::CorruptPackage);
    }
    let package = msi::local_metadata(
        path,
        bytes.len() as u64,
        oink_update::feed::hex(&Sha256::digest(&bytes)),
    )?;
    // Deny write/delete for the full metadata read, so displayed identity and hash
    // describe the same file. The elevated service later verifies its own copy.
    Ok((bytes, package))
}
pub fn elevate(path: &Path, hash: &str) -> Result<(), Error> {
    if !oink_update::local::hex(hash) {
        return Err(Error::Signature);
    }
    let name = path.to_str().ok_or(Error::Storage)?;
    if name.contains(['"', '\0']) {
        return Err(Error::Storage);
    }
    let agent = protected::root().map_err(|_| Error::Storage)?.join("oink-agent.exe");
    let _pin = protected::pin(&agent).map_err(|_| Error::Storage)?;
    let executable = super::super::wide(agent.to_str().ok_or(Error::Storage)?);
    let arguments = super::super::wide(&format!("approve-local-update \"{}\" {}", name, hash));
    let verb = super::super::wide("runas");
    // Shell elevation is requested before any maintenance gate, session drain,
    // UI close or service stop. The SYSTEM input helper remains live for UAC.
    // SAFETY: all NUL-terminated inputs and output structures remain live; the
    // returned process handle is owned and closed, and COM is balanced on success.
    unsafe {
        let com = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: verb.as_ptr(),
            lpFile: executable.as_ptr(),
            lpParameters: arguments.as_ptr(),
            nShow: SW_HIDE,
            ..Default::default()
        };
        let launched = ShellExecuteExW(&mut info);
        if com >= 0 {
            CoUninitialize();
        }
        if launched == 0 || info.hProcess.is_null() {
            return Err(Error::Elevation);
        }
        let process = super::super::Handle::new(info.hProcess).map_err(|_| Error::Elevation)?;
        if WaitForSingleObject(process.0, 120_000) != WAIT_OBJECT_0 {
            return Err(Error::Elevation);
        }
        let mut exit = 1;
        if GetExitCodeProcess(process.0, &mut exit) == 0 || exit != 0 {
            return Err(Error::Elevation);
        }
    }
    Ok(())
}
