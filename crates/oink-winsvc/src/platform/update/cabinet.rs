//! Data-only cabinet extraction. No installer, INF processing or caller paths.
use oink_update::{Error, feed::MAX_UNPACKED};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::c_void,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows_sys::Win32::{Devices::DeviceAndDriverInstallation::*, Foundation::*};

struct Extraction<'a> {
    expected: &'a BTreeMap<String, u64>,
    destination: &'a Path,
    seen: &'a mut BTreeSet<String>,
    total: &'a mut u64,
    files: Vec<(String, PathBuf)>,
    error: Option<Error>,
    started: Instant,
}

// SAFETY: SetupAPI supplies a valid NUL-terminated notification string. Bound
// its accepted length independently of the MSI's bounded string reader.
unsafe fn name(pointer: *const u16) -> Result<String, Error> {
    if pointer.is_null() {
        return Err(Error::Metadata);
    }
    for length in 0..4096 {
        // SAFETY: the notification string remains live throughout the callback.
        if unsafe { *pointer.add(length) } == 0 {
            // SAFETY: the preceding scan bounded a live UTF-16 string.
            return String::from_utf16(unsafe { std::slice::from_raw_parts(pointer, length) })
                .map_err(|_| Error::Metadata);
        }
    }
    Err(Error::Limit)
}

fn member(context: &mut Extraction<'_>, info: &mut FILE_IN_CABINET_INFO_W) -> Result<u32, Error> {
    // SAFETY: NameInCabinet is the notification's documented live UTF-16 string.
    let id = unsafe { name(info.NameInCabinet) }?;
    let expected = context.expected.get(&id).ok_or(Error::Metadata)?;
    if u64::from(info.FileSize) != *expected || !context.seen.insert(id.clone()) {
        return Err(Error::Metadata);
    }
    *context.total = context.total.checked_add(*expected).ok_or(Error::Limit)?;
    if *context.total > MAX_UNPACKED {
        return Err(Error::Limit);
    }
    // Cabinet names are lookup keys only. Native extraction always receives a
    // fresh numeric filename under our protected directory, never archive paths.
    let path = context.destination.join(format!("member-{}.bin", context.files.len()));
    let target = super::super::wide(path.to_str().ok_or(Error::Storage)?);
    if target.len() > info.FullTargetName.len() {
        return Err(Error::Limit);
    }
    info.FullTargetName[..target.len()].copy_from_slice(&target);
    context.files.push((id, path));
    Ok(FILEOP_DOIT)
}

unsafe extern "system" fn callback(raw: *const c_void, notification: u32, param1: usize, _: usize) -> u32 {
    // SAFETY: extract passes this exclusively borrowed stack context to a
    // synchronous SetupIterateCabinet call; it outlives every notification.
    let context = unsafe { &mut *raw.cast_mut().cast::<Extraction<'_>>() };
    if context.error.is_some() {
        return if notification == SPFILENOTIFY_FILEINCABINET {
            FILEOP_ABORT
        } else {
            ERROR_INVALID_DATA
        };
    }
    let result = if context.started.elapsed() > Duration::from_secs(30) {
        Err(Error::Limit)
    } else {
        match notification {
            SPFILENOTIFY_FILEINCABINET if param1 != 0 => {
                // SAFETY: this notification documents a writable FILE_IN_CABINET_INFO_W.
                member(context, unsafe { &mut *(param1 as *mut FILE_IN_CABINET_INFO_W) })
            }
            SPFILENOTIFY_FILEEXTRACTED if param1 != 0 => {
                // SAFETY: this notification documents a live FILEPATHS_W.
                let error = unsafe { (*(param1 as *const FILEPATHS_W)).Win32Error };
                if error == ERROR_SUCCESS {
                    Ok(ERROR_SUCCESS)
                } else {
                    Err(Error::Storage)
                }
            }
            SPFILENOTIFY_CABINETINFO => Ok(ERROR_SUCCESS),
            // Never search another path, prompt, or fetch a spanned cabinet.
            _ => Err(Error::Metadata),
        }
    };
    match result {
        Ok(value) => value,
        Err(error) => {
            context.error = Some(error);
            if notification == SPFILENOTIFY_FILEINCABINET {
                FILEOP_ABORT
            } else {
                ERROR_INVALID_DATA
            }
        }
    }
}

pub(super) fn extract(
    cabinet: &Path,
    destination: &Path,
    expected: &BTreeMap<String, u64>,
    seen: &mut BTreeSet<String>,
    total: &mut u64,
) -> Result<Vec<(String, PathBuf)>, Error> {
    let mut context = Extraction {
        expected,
        destination,
        seen,
        total,
        files: Vec::new(),
        error: None,
        started: Instant::now(),
    };
    let source = super::super::wide(cabinet.to_str().ok_or(Error::Storage)?);
    // cab::read_file restarts decompression at the folder prefix for every file.
    // A real 443-file MSI took minutes per inspection. SetupAPI iterates once,
    // with our explicit filename/size/budget callback; it never executes MSI CAs.
    // https://learn.microsoft.com/windows/win32/setupapi/creating-a-cabinet-callback-routine
    // SAFETY: fixed cabinet and owned context remain live for this synchronous
    // data-only API. The callback supplies only numeric destinations we created.
    let success = unsafe {
        SetupIterateCabinetW(
            source.as_ptr(),
            0,
            Some(callback),
            (&mut context as *mut Extraction<'_>).cast(),
        )
    };
    if let Some(error) = context.error {
        return Err(error);
    }
    if success == 0 {
        return Err(Error::Metadata);
    }
    Ok(context.files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, io::Write};

    fn fixture(path: &Path, files: &[(String, Vec<u8>)]) {
        let mut builder = cab::CabinetBuilder::new();
        let folder = builder.add_folder(cab::CompressionType::MsZip);
        for (name, _) in files {
            folder.add_file(name);
        }
        let mut writer = builder.build(File::create(path).unwrap()).unwrap();
        let mut at = 0;
        while let Some(mut output) = writer.next_file().unwrap() {
            output.write_all(&files[at].1).unwrap();
            at += 1;
        }
        writer.finish().unwrap();
    }

    #[test]
    fn wide_cabinet_extracts_every_file_once_and_checks_all_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let cabinet = temporary.path().join("input.cab");
        let output = temporary.path().join("output");
        std::fs::create_dir(&output).unwrap();
        let mut files: Vec<_> = (0..443).map(|n| (format!("id{n}"), vec![n as u8; 64])).collect();
        files[0].1 = vec![42; 2 * 1024 * 1024];
        fixture(&cabinet, &files);
        let expected = files.iter().map(|(id, data)| (id.clone(), data.len() as u64)).collect();
        let (mut seen, mut total) = (BTreeSet::new(), 0);
        let started = Instant::now();
        let extracted = extract(&cabinet, &output, &expected, &mut seen, &mut total).unwrap();
        assert_eq!(extracted.len(), files.len());
        assert_eq!(seen.len(), files.len());
        assert_eq!(total, files.iter().map(|(_, data)| data.len() as u64).sum::<u64>());
        for ((id, path), (original, data)) in extracted.iter().zip(&files) {
            assert_eq!(id, original);
            assert_eq!(std::fs::read(path).unwrap(), *data);
        }
        eprintln!(
            "native wide cabinet: {} members in {:?}",
            extracted.len(),
            started.elapsed()
        );
    }

    #[test]
    fn wrong_size_duplicate_or_unknown_path_never_becomes_an_extraction_destination() {
        for case in 0..3 {
            let temporary = tempfile::tempdir().unwrap();
            let cabinet = temporary.path().join("input.cab");
            let output = temporary.path().join("output");
            std::fs::create_dir(&output).unwrap();
            let files = match case {
                0 => vec![("known".into(), vec![1; 10])],
                1 => vec![("known".into(), vec![1; 10]), ("known".into(), vec![2; 10])],
                _ => vec![("../outside".into(), vec![1; 10])],
            };
            fixture(&cabinet, &files);
            let expected = BTreeMap::from([("known".into(), if case == 0 { 11 } else { 10 })]);
            assert_eq!(
                extract(&cabinet, &output, &expected, &mut BTreeSet::new(), &mut 0).unwrap_err(),
                Error::Metadata
            );
            assert!(!temporary.path().join("outside").exists());
            assert!(
                std::fs::read_dir(output).unwrap().all(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("member-"))
            );
        }
    }
}
