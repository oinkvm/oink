//! Read-only MSI database/cabinet inspection. Never uses administrative install.
use super::super::wide;
use super::{protected, signature};
use oink_update::{
    Error,
    feed::{MAX_PACKAGE, MAX_UNPACKED, hex},
    trust::Roots,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
    path::Path,
    ptr,
};
use windows_sys::Win32::{Foundation::ERROR_NO_MORE_ITEMS, System::ApplicationInstallationAndServicing::*};
const UPGRADE: &str = "4D39E4BE-6792-4CD6-B344-A5E95A270EA6";
const FILES: &[&str] = &[
    "oink.exe",
    "oink-agent.exe",
    "oink-service.exe",
    "oink-update-worker.exe",
    "installed.json",
];
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub schema: u32,
    pub product: String,
    pub source_tree_sha256: String,
    pub version: String,
    pub build: u64,
    pub architecture: String,
    pub minimum_os: String,
    #[serde(rename = "minimumWindowsBuild")]
    pub minimum_windows_build: u32,
    pub updater_included: bool,
    pub collector_included: bool,
    pub protocol_baseline: u32,
    pub upgrade_code: String,
    pub components: Vec<String>,
    pub signature: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Inventory {
    pub product_code: String,
    pub build: u64,
    pub hashes: BTreeMap<String, String>,
}
struct Msi(u32);
impl Drop for Msi {
    fn drop(&mut self) {
        // SAFETY: wrapper owns a valid MSI database, view or record handle.
        unsafe {
            MsiCloseHandle(self.0);
        }
    }
}
fn ok(code: u32) -> Result<(), Error> {
    if code == 0 { Ok(()) } else { Err(Error::Metadata) }
}
fn rows(database: &Msi, query: &str) -> Result<Vec<Msi>, Error> {
    let mut view = 0;
    // SAFETY: fixed SELECT only; database opened read-only, output handle is owned below.
    ok(unsafe { MsiDatabaseOpenViewW(database.0, wide(query).as_ptr(), &mut view) })?;
    let view = Msi(view);
    // SAFETY: live query view; no parameters or side effects.
    ok(unsafe { MsiViewExecute(view.0, 0) })?;
    let mut rows = Vec::new();
    loop {
        let mut row = 0;
        // SAFETY: live query and output handle.
        let code = unsafe { MsiViewFetch(view.0, &mut row) };
        if code == ERROR_NO_MORE_ITEMS {
            break;
        }
        ok(code)?;
        rows.push(Msi(row));
        if rows.len() > 1024 {
            return Err(Error::Limit);
        }
    }
    Ok(rows)
}
fn string(row: &Msi, field: u32) -> Result<String, Error> {
    let mut buf = [0u16; 4096];
    let mut length = buf.len() as u32;
    // SAFETY: record field is copied into bounded UTF-16 storage; larger values fail.
    ok(unsafe { MsiRecordGetStringW(row.0, field, buf.as_mut_ptr(), &mut length) })?;
    String::from_utf16(&buf[..length as usize]).map_err(|_| Error::Metadata)
}
fn stream(row: &Msi, field: u32, max: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    loop {
        let mut buf = [0u8; 65536];
        let mut length = buf.len() as u32;
        // SAFETY: live record and writable bounded byte buffer.
        ok(unsafe { MsiRecordReadStream(row.0, field, buf.as_mut_ptr(), &mut length) })?;
        if length == 0 {
            return Ok(bytes);
        }
        if bytes.len() + length as usize > max {
            return Err(Error::Limit);
        }
        bytes.extend_from_slice(&buf[..length as usize]);
    }
}
pub(super) fn inspect(
    path: &Path,
    stage: &Path,
    roots: &Roots,
    release: &oink_update::feed::Release,
) -> Result<Inventory, Error> {
    let expected_build = release.build;
    let _locked = protected::open(path).map_err(|_| Error::Storage)?;
    signature::verify(path, roots)?;
    if expected_build == 0 || expected_build >= (1u64 << 31) {
        return Err(Error::Metadata);
    }
    let mut database = 0;
    // SAFETY: null persist mode is MSIDBOPEN_READONLY; no installation sequences run.
    ok(unsafe {
        MsiOpenDatabaseW(
            wide(path.to_str().ok_or(Error::Storage)?).as_ptr(),
            ptr::null(),
            &mut database,
        )
    })?;
    let database = Msi(database);
    let properties: BTreeMap<_, _> = rows(&database, "SELECT `Property`, `Value` FROM `Property`")?
        .into_iter()
        .map(|row| Ok((string(&row, 1)?, string(&row, 2)?)))
        .collect::<Result<_, Error>>()?;
    let product_code = properties.get("ProductCode").ok_or(Error::Metadata)?.clone();
    if properties.get("ProductName").map(String::as_str) != Some("Oink")
        || properties.get("UpgradeCode").map(|s| s.trim_matches(['{', '}'])) != Some(UPGRADE)
        || properties.get("ProductVersion")
            != Some(&format!(
                "{}.{}.{}",
                expected_build >> 24,
                (expected_build >> 16) & 255,
                expected_build & 65535
            ))
        || !valid_product_code(&product_code)
    {
        return Err(Error::Metadata);
    }
    let components: BTreeMap<_, _> = rows(&database, "SELECT `Component`, `Directory_` FROM `Component`")?
        .into_iter()
        .map(|row| Ok((string(&row, 1)?, string(&row, 2)?)))
        .collect::<Result<_, Error>>()?;
    let mut files = BTreeMap::new();
    let mut names = BTreeSet::new();
    for row in rows(
        &database,
        "SELECT `File`, `Component_`, `FileName`, `FileSize` FROM `File`",
    )? {
        let id = string(&row, 1)?;
        let filename = string(&row, 3)?;
        let name = filename.rsplit('|').next().ok_or(Error::Metadata)?.to_owned();
        // MSI file identifiers are used only as cabinet lookup keys, never paths.
        if !(FILES.contains(&name.as_str()) || name == "oink-crash-collector.exe")
            || !names.insert(name.clone())
            || components.get(&string(&row, 2)?).map(String::as_str) != Some("INSTALLFOLDER")
        {
            return Err(Error::Metadata);
        }
        let length: u64 = string(&row, 4)?.parse().map_err(|_| Error::Metadata)?;
        if length == 0 || length > MAX_UNPACKED {
            return Err(Error::Limit);
        }
        if files.insert(id, (name, length)).is_some() {
            return Err(Error::Metadata);
        }
    }
    if !FILES.iter().all(|name| names.contains(*name)) {
        return Err(Error::Metadata);
    }
    let cabinets: BTreeSet<_> = rows(&database, "SELECT `Cabinet` FROM `Media`")?
        .into_iter()
        .map(|r| string(&r, 1))
        .collect::<Result<_, _>>()?;
    if cabinets.is_empty() || cabinets.iter().any(|s| !s.starts_with('#')) {
        return Err(Error::Metadata);
    }
    protected::directory(stage).map_err(|_| Error::Storage)?;
    let mut hashes = BTreeMap::new();
    let mut total = 0u64;
    let mut seen = BTreeSet::new();
    for row in rows(&database, "SELECT `Name`, `Data` FROM `_Streams`")? {
        let name = string(&row, 1)?;
        if !cabinets.contains(&format!("#{name}")) {
            continue;
        }
        let bytes = stream(&row, 2, MAX_PACKAGE as usize)?;
        let mut cabinet = cab::Cabinet::new(Cursor::new(bytes)).map_err(|_| Error::Metadata)?;
        let entries: Vec<_> = cabinet
            .folder_entries()
            .flat_map(|f| f.file_entries())
            .map(|f| (f.name().to_owned(), u64::from(f.uncompressed_size())))
            .collect();
        for (id, length) in entries {
            let (name, expected) = files.get(&id).ok_or(Error::Metadata)?;
            total = total.checked_add(length).ok_or(Error::Limit)?;
            if total > MAX_UNPACKED || length != *expected || !seen.insert(id.clone()) {
                return Err(Error::Limit);
            }
            let mut bytes = Vec::new();
            cabinet
                .read_file(&id)
                .map_err(|_| Error::Metadata)?
                .take(length + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::Metadata)?;
            if bytes.len() as u64 != length {
                return Err(Error::Metadata);
            }
            let output = stage.join(name);
            protected::write(&output, &bytes)?;
            hashes.insert(name.clone(), hex(&Sha256::digest(&bytes)));
            if name.ends_with(".exe") {
                signature::verify(&output, roots)?;
            } else {
                let receipt: Receipt = oink_update::feed::strict_json(&bytes)?;
                if receipt.schema != 1
                    || receipt.product != "Oink"
                    || receipt.build != expected_build
                    || receipt.source_tree_sha256 != release.source_tree_sha256
                    || receipt.version != release.version
                    || receipt.minimum_windows_build != release.minimum_windows_build
                    || !receipt.updater_included
                    || receipt.collector_included != names.contains("oink-crash-collector.exe")
                    || receipt.components.iter().cloned().collect::<BTreeSet<_>>()
                        != names.iter().filter(|n| n.ends_with(".exe")).cloned().collect()
                    || receipt.architecture != "x64"
                    || receipt.protocol_baseline != 1
                    || receipt.upgrade_code != UPGRADE
                {
                    return Err(Error::Metadata);
                }
            }
        }
    }
    if seen.len() != files.len() {
        return Err(Error::Metadata);
    }
    // Native custom action code executes as SYSTEM too; verify the embedded DLL.
    let binaries = rows(&database, "SELECT `Name`, `Data` FROM `Binary`")?;
    if binaries.len() != 1 || string(&binaries[0], 1)? != "InstallerCA" {
        return Err(Error::Metadata);
    }
    let ca = stage.join("InstallerCA.dll");
    protected::write(&ca, &stream(&binaries[0], 2, 16 * 1024 * 1024)?)?;
    signature::verify(&ca, roots)?;
    Ok(Inventory {
        product_code,
        build: expected_build,
        hashes,
    })
}
fn valid_product_code(code: &str) -> bool {
    let code = code.trim_matches(['{', '}']);
    code.len() == 36
        && code.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
