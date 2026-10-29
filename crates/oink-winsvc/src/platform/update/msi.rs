//! Read-only MSI database/cabinet inspection. Never uses administrative install.
use super::super::wide;
use super::{cabinet, protected, signature};
use oink_update::{
    Error,
    feed::{MAX_PACKAGE, MAX_UNPACKED, hex},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    "oink-build.json",
    "report-manifest.json",
];
const AUDIO_DRIVER_ARCHIVE: &str = "AudioPlugin/VBCABLE_Driver_Pack45.zip";
const NOTICES: &[&str] = &[
    "third-party-licenses.json",
    "THIRD_PARTY_LICENSES.md",
    "NOTICE",
    "LICENSE",
    "LICENSE-ADDITIONAL-TERMS.md",
];
fn relative_file(
    directory: &str,
    name: &str,
    directories: &BTreeMap<String, (String, String)>,
) -> Result<String, Error> {
    if name.contains('/') {
        return Err(Error::Metadata);
    }
    let mut parts = vec![name.to_owned()];
    let mut current = directory;
    let mut seen = BTreeSet::new();
    while current != "INSTALLFOLDER" {
        if !seen.insert(current) {
            return Err(Error::Metadata);
        }
        let (parent, name) = directories.get(current).ok_or(Error::Metadata)?;
        if name.contains('/') {
            return Err(Error::Metadata);
        }
        parts.push(name.clone());
        current = parent;
    }
    parts.reverse();
    let path = parts.join("/");
    // The installed (older) service inspects the next package. Constrain its
    // extraction to product directories, rather than freezing future filenames.
    // Package authorization authenticates the whole MSI, including executable
    // custom actions; this check protects our extraction/inventory namespace.
    // Fold reserved paths because the destination is case-insensitive on Windows.
    let folded = path.to_ascii_lowercase();
    if folded == "maintenance" || folded.starts_with("maintenance/") {
        return Err(Error::Metadata);
    }
    if path != "installed.json" {
        oink_update::package::validate_path(&folded)?;
    }
    Ok(path)
}
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
/// First manual installation has no updater commit to reuse. Capture its
/// trusted installed payload once, outside the explicit Install path. This is
/// a rollback-health baseline, not a comparison with or extraction of its MSI.
pub(super) fn installed_inventory(receipt: &Receipt, product_code: String) -> Result<Inventory, Error> {
    let root = protected::root().map_err(|_| Error::Storage)?;
    let mut paths: BTreeMap<String, std::path::PathBuf> = FILES
        .iter()
        .chain(NOTICES)
        .map(|name| (name.to_string(), root.join(name)))
        .collect();
    if receipt.collector_included {
        paths.insert("oink-crash-collector.exe".into(), root.join("oink-crash-collector.exe"));
    }
    // Older installations may predate the bundled optional driver. Capture its
    // installed hash when present for this first-install rollback baseline.
    // Incoming packages use their complete MSI File-table inventory instead.
    if root.join(AUDIO_DRIVER_ARCHIVE).exists() {
        paths.insert(AUDIO_DRIVER_ARCHIVE.into(), root.join(AUDIO_DRIVER_ARCHIVE));
    }
    let mut directories = vec![root.join("licenses")];
    while let Some(directory) = directories.pop() {
        for path in protected::entries(&directory).map_err(|_| Error::Storage)? {
            if path.is_dir() {
                directories.push(path);
            } else {
                let name = path
                    .strip_prefix(&root)
                    .map_err(|_| Error::Storage)?
                    .to_str()
                    .ok_or(Error::Metadata)?
                    .replace('\\', "/");
                oink_update::package::validate_path(&name)?;
                paths.insert(name, path);
            }
        }
    }
    if !valid_product_code(&product_code) {
        return Err(Error::Metadata);
    }
    let hashes = paths
        .into_iter()
        .map(|(name, path)| Ok((name, protected::hash(&path)?)))
        .collect::<Result<_, Error>>()?;
    Ok(Inventory {
        product_code,
        build: receipt.build,
        hashes,
    })
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
        if rows.len() > 4096 {
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
type FileTable = (BTreeMap<String, (String, u64)>, BTreeSet<String>);

fn file_table(database: &Msi) -> Result<FileTable, Error> {
    let components: BTreeMap<_, _> = rows(database, "SELECT `Component`, `Directory_` FROM `Component`")?
        .into_iter()
        .map(|row| Ok((string(&row, 1)?, string(&row, 2)?)))
        .collect::<Result<_, Error>>()?;
    let directories = rows(
        database,
        "SELECT `Directory`, `Directory_Parent`, `DefaultDir` FROM `Directory`",
    )?
    .into_iter()
    .map(|row| {
        Ok((
            string(&row, 1)?,
            (
                string(&row, 2)?,
                string(&row, 3)?.rsplit('|').next().ok_or(Error::Metadata)?.to_owned(),
            ),
        ))
    })
    .collect::<Result<BTreeMap<_, _>, Error>>()?;
    let mut files = BTreeMap::new();
    let mut names = BTreeSet::new();
    for row in rows(
        database,
        "SELECT `File`, `Component_`, `FileName`, `FileSize` FROM `File`",
    )? {
        let id = string(&row, 1)?;
        let filename = string(&row, 3)?;
        let name = relative_file(
            components.get(&string(&row, 2)?).ok_or(Error::Metadata)?,
            filename.rsplit('|').next().ok_or(Error::Metadata)?,
            &directories,
        )?;
        // MSI file identifiers are used only as cabinet lookup keys, never paths.
        if !names.insert(name.to_ascii_lowercase()) {
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
    if !FILES.iter().all(|name| names.contains(&name.to_ascii_lowercase())) {
        return Err(Error::Metadata);
    }
    Ok((files, names))
}

fn local_notes(database: &Msi) -> Result<Option<oink_update::feed::ReleaseNotes>, Error> {
    // Older packages and the minimal database fixture need not have this table.
    if rows(database, "SELECT `Name` FROM `_Tables` WHERE `Name` = 'Binary'")?.is_empty() {
        return Ok(None);
    }
    let notes = rows(
        database,
        "SELECT `Data` FROM `Binary` WHERE `Name` = 'OinkLocalUpdateNotes'",
    )?;
    match notes.as_slice() {
        [] => Ok(None),
        [row] => oink_update::local::embedded_notes(&stream(row, 1, oink_update::local::MAX_EMBEDDED_NOTES)?).map(Some),
        _ => Err(Error::CorruptPackage),
    }
}

pub(super) fn local_metadata(path: &Path, length: u64, sha256: String) -> Result<oink_update::local::Package, Error> {
    let mut database = 0;
    // SAFETY: bounded UTF-16 path, read-only database mode, owned output handle.
    ok(unsafe {
        MsiOpenDatabaseW(
            wide(path.to_str().ok_or(Error::Storage)?).as_ptr(),
            ptr::null(),
            &mut database,
        )
    })
    .map_err(|_| Error::CorruptPackage)?;
    let database = Msi(database);
    let properties: BTreeMap<_, _> = rows(&database, "SELECT `Property`, `Value` FROM `Property`")?
        .into_iter()
        .map(|row| Ok((string(&row, 1)?, string(&row, 2)?)))
        .collect::<Result<_, Error>>()?;
    if properties.get("ProductName").map(String::as_str) != Some("Oink")
        || properties.get("UpgradeCode").map(|v| v.trim_matches(['{', '}'])) != Some(UPGRADE)
    {
        return Err(Error::WrongProduct);
    }
    let mut summary = 0;
    // SAFETY: read-only live database; returned summary handle is owned below.
    ok(unsafe { MsiGetSummaryInformationW(database.0, ptr::null(), 0, &mut summary) })?;
    let summary = Msi(summary);
    let (mut kind, mut number, mut time) = (0, 0, windows_sys::Win32::Foundation::FILETIME::default());
    let mut text = [0u16; 256];
    let mut size = text.len() as u32;
    // SAFETY: live summary and correctly sized writable outputs; property 7 is the template.
    ok(unsafe {
        MsiSummaryInfoGetPropertyW(
            summary.0,
            7,
            &mut kind,
            &mut number,
            &mut time,
            text.as_mut_ptr(),
            &mut size,
        )
    })?;
    let template = String::from_utf16(&text[..size as usize]).map_err(|_| Error::CorruptPackage)?;
    if template.split(';').next() != Some("x64") {
        return Err(Error::WrongArchitecture);
    }
    let parts = properties
        .get("ProductVersion")
        .ok_or(Error::CorruptPackage)?
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::CorruptPackage)?;
    if parts.len() != 3 || parts[0] > 127 || parts[1] > 255 || parts[2] > 65535 {
        return Err(Error::CorruptPackage);
    }
    let build = (parts[0] << 24) | (parts[1] << 16) | parts[2];
    Ok(oink_update::local::Package {
        product: oink_update::PRODUCT.into(),
        build,
        version: properties.get("OINK_VERSION").ok_or(Error::CorruptPackage)?.clone(),
        source_tree_sha256: properties.get("OINK_SOURCE_TREE").ok_or(Error::CorruptPackage)?.clone(),
        minimum_windows_build: properties
            .get("OINK_MINIMUM_WINDOWS_BUILD")
            .ok_or(Error::CorruptPackage)?
            .parse()
            .map_err(|_| Error::CorruptPackage)?,
        architecture: "x64".into(),
        length,
        sha256,
        notes: local_notes(&database)?,
    })
}

pub(super) fn inspect(
    path: &Path,
    stage: &Path,
    pins: &[String],
    release: &oink_update::feed::Release,
) -> Result<Inventory, Error> {
    let expected_build = release.build;
    let _locked = protected::pin(path).map_err(|_| Error::Storage)?;
    signature::verify(path, pins)?;
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
    let (files, names) = file_table(&database)?;
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
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Storage)?;
        let native = stage.join(format!("cab-{}", hex(&random)));
        protected::directory(&native).map_err(|_| Error::Storage)?;
        let cabinet = native.join("input.cab");
        protected::write(&cabinet, &bytes)?;
        let _cabinet = protected::open(&cabinet).map_err(|_| Error::Storage)?;
        let expected = files.iter().map(|(id, (_, length))| (id.clone(), *length)).collect();
        for (id, extracted) in cabinet::extract(&cabinet, &native, &expected, &mut seen, &mut total)? {
            let (name, expected) = files.get(&id).ok_or(Error::Metadata)?;
            let bytes = protected::read(&extracted, *expected)?;
            if bytes.len() as u64 != *expected {
                return Err(Error::Metadata);
            }
            let output = stage.join(name);
            let mut parent = stage.to_owned();
            for part in Path::new(name).parent().ok_or(Error::Metadata)?.components() {
                parent.push(part);
                protected::directory(&parent).map_err(|_| Error::Storage)?;
            }
            // Native extraction already wrote these exact protected bytes.
            // A same-volume move preserves them and avoids a second full write
            // and a second payload tree to delete after the update.
            retain_member(&extracted, &output)?;
            hashes.insert(name.clone(), hex(&Sha256::digest(&bytes)));
            if name.to_ascii_lowercase().ends_with(".exe") {
                signature::verify(&output, pins)?;
            } else if name == "installed.json" {
                let receipt: Receipt = oink_update::feed::strict_json(&bytes)?;
                if receipt.schema != 1
                    || receipt.product != "Oink"
                    || receipt.build != expected_build
                    || receipt.source_tree_sha256 != release.source_tree_sha256
                    || receipt.version != release.version
                    || receipt.minimum_windows_build != release.minimum_windows_build
                    || !receipt.updater_included
                    || receipt.collector_included != names.contains("oink-crash-collector.exe")
                    || receipt
                        .components
                        .iter()
                        .map(|n| n.to_ascii_lowercase())
                        .collect::<BTreeSet<_>>()
                        != names
                            .iter()
                            .filter(|n| n.ends_with(".exe") && !n.contains('/'))
                            .cloned()
                            .collect()
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
    // WiX also embeds firewall actions and UI bitmaps. All are covered by the
    // manifest's whole-MSI hash (and optional MSI Authenticode pin). InstallerCA
    // is not separately Authenticode-signed by the SignPath MSI configuration.
    let binaries = rows(&database, "SELECT `Data` FROM `Binary` WHERE `Name` = 'InstallerCA'")?;
    if binaries.len() != 1 {
        return Err(Error::Metadata);
    }
    let ca = stage.join("InstallerCA.dll");
    protected::write(&ca, &stream(&binaries[0], 1, 16 * 1024 * 1024)?)?;
    Ok(Inventory {
        product_code,
        build: expected_build,
        hashes,
    })
}
fn retain_member(extracted: &Path, output: &Path) -> Result<(), Error> {
    std::fs::rename(extracted, output).map_err(|_| Error::Storage)
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

#[cfg(test)]
mod tests {
    use super::*;
    fn local_fixture(path: &Path, name: &str, template: &str) {
        let mut database = 0;
        // SAFETY: isolated test MSI, CREATE mode; never an installed product.
        ok(unsafe { MsiOpenDatabaseW(wide(path.to_str().unwrap()).as_ptr(), MSIDBOPEN_CREATE, &mut database) })
            .unwrap();
        let database = Msi(database);
        let queries=[
            "CREATE TABLE `Property` (`Property` CHAR(72) NOT NULL, `Value` CHAR(0) LOCALIZABLE PRIMARY KEY `Property`)".to_string(),
            format!("INSERT INTO `Property` (`Property`, `Value`) VALUES ('ProductName', '{name}')"),
            format!("INSERT INTO `Property` (`Property`, `Value`) VALUES ('UpgradeCode', '{{{UPGRADE}}}')"),
            "INSERT INTO `Property` (`Property`, `Value`) VALUES ('ProductVersion', '0.0.30')".into(),
            "INSERT INTO `Property` (`Property`, `Value`) VALUES ('OINK_VERSION', '0.3.0')".into(),
            format!("INSERT INTO `Property` (`Property`, `Value`) VALUES ('OINK_SOURCE_TREE', '{}')","a".repeat(64)),
            "INSERT INTO `Property` (`Property`, `Value`) VALUES ('OINK_MINIMUM_WINDOWS_BUILD', '19045')".into(),
        ];
        for query in queries {
            let mut view = 0;
            // SAFETY: fixed fixture-only SQL, isolated owned database and view.
            ok(unsafe { MsiDatabaseOpenViewW(database.0, wide(&query).as_ptr(), &mut view) }).unwrap();
            let view = Msi(view);
            // SAFETY: live fixture view; no MSI install sequence is ever run.
            ok(unsafe { MsiViewExecute(view.0, 0) }).unwrap();
        }
        let mut summary = 0;
        // SAFETY: isolated summary information with one writable property.
        ok(unsafe { MsiGetSummaryInformationW(database.0, ptr::null(), 1, &mut summary) }).unwrap();
        let summary = Msi(summary);
        // SAFETY: property 7 (template), VT_LPSTR; live NUL-terminated string.
        ok(unsafe { MsiSummaryInfoSetPropertyW(summary.0, 7, 30, 0, ptr::null_mut(), wide(template).as_ptr()) })
            .unwrap();
        // SAFETY: persist only the disposable fixture's table and summary bytes.
        unsafe {
            ok(MsiSummaryInfoPersist(summary.0)).unwrap();
            ok(MsiDatabaseCommit(database.0)).unwrap();
        }
    }
    #[test]
    fn local_preflight_extracts_optional_binary_notes_without_installing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.msi");
        local_fixture(&path, "Oink", "x64;1033");
        assert!(local_metadata(&path, 123, "b".repeat(64)).unwrap().notes.is_none());
        let notes = root.path().join("notes.json");
        std::fs::write(
            &notes,
            r#"{"schema":1,"notes":{"en":"Fixed file drops","ja":"ドラッグを修正"}}"#.as_bytes(),
        )
        .unwrap();
        let mut database = 0;
        // SAFETY: transaction writes only a disposable MSI fixture, never installs it.
        ok(unsafe { MsiOpenDatabaseW(wide(path.to_str().unwrap()).as_ptr(), MSIDBOPEN_TRANSACT, &mut database) })
            .unwrap();
        let database = Msi(database);
        for query in [
            "CREATE TABLE `Binary` (`Name` CHAR(72) NOT NULL, `Data` OBJECT PRIMARY KEY `Name`)",
            "INSERT INTO `Binary` (`Name`, `Data`) VALUES ('OinkLocalUpdateNotes', ?)",
        ] {
            let mut view = 0;
            // SAFETY: fixed SQL against isolated owned database and view.
            ok(unsafe { MsiDatabaseOpenViewW(database.0, wide(query).as_ptr(), &mut view) }).unwrap();
            let view = Msi(view);
            // SAFETY: fixture record contains only a local JSON stream parameter.
            let record = Msi(unsafe { MsiCreateRecord(1) });
            if query.starts_with("INSERT") {
                // SAFETY: live fixture record and NUL-terminated file path.
                ok(unsafe { MsiRecordSetStreamW(record.0, 1, wide(notes.to_str().unwrap()).as_ptr()) }).unwrap();
            }
            // SAFETY: live fixture view, no install sequence is run.
            ok(unsafe { MsiViewExecute(view.0, if query.starts_with("INSERT") { record.0 } else { 0 }) }).unwrap();
        }
        // SAFETY: persist only this temporary fixture's Binary-table bytes.
        ok(unsafe { MsiDatabaseCommit(database.0) }).unwrap();
        drop(database);
        let extracted = local_metadata(&path, 123, "b".repeat(64)).unwrap().notes.unwrap();
        assert_eq!(extracted.for_language("ja-JP"), "ドラッグを修正");
        assert_eq!(extracted.for_language("ko"), "Fixed file drops");
    }
    #[test]
    fn local_msi_preflight_reads_real_database_and_rejects_other_product_architecture_or_damage() {
        let root = tempfile::tempdir().unwrap();
        for (name, template, error) in [
            ("Oink", "x64;1033", None),
            ("Other", "x64;1033", Some(Error::WrongProduct)),
            ("Oink", "Arm64;1033", Some(Error::WrongArchitecture)),
        ] {
            let path = root.path().join(format!("{name}-{template}.msi"));
            local_fixture(&path, name, template);
            let result = local_metadata(&path, 123, "b".repeat(64));
            if let Some(error) = error {
                assert_eq!(result, Err(error));
            } else {
                let package = result.unwrap();
                assert_eq!(package.build, 30);
                let spool = root.path().join("spool.msi");
                let original = std::fs::read(&path).unwrap();
                let copied = super::super::local::spool(&path, &spool).unwrap();
                assert_eq!(copied.sha256, hex(&Sha256::digest(&original)));
                std::fs::write(&path, b"changed after preflight").unwrap();
                assert_eq!(std::fs::read(&spool).unwrap(), original);
                assert!(copied.candidate().local_package);
                assert_eq!(package.validate("0.3.0", 30, "x64"), Ok(()));
                assert_eq!(package.validate("0.3.0", 31, "x64"), Err(Error::Downgrade));
            }
        }
        let path = root.path().join("broken.msi");
        std::fs::write(&path, b"damaged MSI").unwrap();
        assert_eq!(local_metadata(&path, 11, "b".repeat(64)), Err(Error::CorruptPackage));
    }
    #[test]
    fn inspected_member_is_retained_without_a_second_payload_copy() {
        let directory = tempfile::tempdir().unwrap();
        let extracted = directory.path().join("cabinet-member");
        let output = directory.path().join("product.exe");
        std::fs::write(&extracted, b"verified payload").unwrap();
        retain_member(&extracted, &output).unwrap();
        assert!(!extracted.exists());
        assert_eq!(std::fs::read(&output).unwrap(), b"verified payload");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    #[ignore = "explicit external TEST MSI and previously verified inventory; no installation"]
    fn external_msi_native_cabinet_matches_every_previously_verified_file() {
        let path = std::env::var("OINK_UPDATE_TEST_MSI").expect("external TEST MSI path required");
        let inventory = std::env::var("OINK_UPDATE_TEST_INVENTORY").expect("external verified inventory required");
        let inventory: Inventory = serde_json::from_slice(&std::fs::read(inventory).unwrap()).unwrap();
        let started = std::time::Instant::now();
        let mut database = 0;
        // SAFETY: explicit fixture opened read-only, never an administrative installation.
        ok(unsafe { MsiOpenDatabaseW(wide(&path).as_ptr(), ptr::null(), &mut database) }).unwrap();
        let database = Msi(database);
        let (files, _) = file_table(&database).unwrap();
        let cabinets: BTreeSet<_> = rows(&database, "SELECT `Cabinet` FROM `Media`")
            .unwrap()
            .iter()
            .map(|row| string(row, 1).unwrap())
            .collect();
        let temporary = tempfile::tempdir().unwrap();
        let expected = files.iter().map(|(id, (_, length))| (id.clone(), *length)).collect();
        let (mut seen, mut total, mut hashes) = (BTreeSet::new(), 0, BTreeMap::new());
        for (index, row) in rows(&database, "SELECT `Name`, `Data` FROM `_Streams`")
            .unwrap()
            .iter()
            .enumerate()
        {
            if !cabinets.contains(&format!("#{}", string(row, 1).unwrap())) {
                continue;
            }
            let output = temporary.path().join(index.to_string());
            std::fs::create_dir(&output).unwrap();
            let path = output.join("input.cab");
            std::fs::write(&path, stream(row, 2, MAX_PACKAGE as usize).unwrap()).unwrap();
            for (id, path) in cabinet::extract(&path, &output, &expected, &mut seen, &mut total).unwrap() {
                let bytes = std::fs::read(path).unwrap();
                let (name, length) = &files[&id];
                assert_eq!(bytes.len() as u64, *length);
                hashes.insert(name.clone(), hex(&Sha256::digest(&bytes)));
            }
        }
        assert_eq!(seen.len(), files.len());
        assert_eq!(hashes, inventory.hashes);
        eprintln!(
            "external MSI: {} complete file hashes matched in {:?}",
            hashes.len(),
            started.elapsed()
        );
    }
    #[test]
    fn product_file_table_accepts_build_49_and_future_payloads() {
        let directories = BTreeMap::from([
            ("AudioPlugin".into(), ("INSTALLFOLDER".into(), "AudioPlugin".into())),
            (
                "FutureResources".into(),
                ("INSTALLFOLDER".into(), "future-resources".into()),
            ),
        ]);
        // The build-49 table is checked by the older receiving service. New
        // product files cannot require an update to that service first.
        for (directory, name, expected) in [
            ("AudioPlugin", "Microphone.ps1", "AudioPlugin/Microphone.ps1"),
            (
                "AudioPlugin",
                "VBCABLE_Driver_Pack45.zip",
                "AudioPlugin/VBCABLE_Driver_Pack45.zip",
            ),
            ("AudioPlugin", "future-helper.dll", "AudioPlugin/future-helper.dll"),
            (
                "FutureResources",
                "future-data.json",
                "future-resources/future-data.json",
            ),
            ("INSTALLFOLDER", "oink-future-helper.exe", "oink-future-helper.exe"),
        ] {
            assert_eq!(
                relative_file(directory, name, &directories),
                Ok(expected.into()),
                "{expected}"
            );
        }
    }
    #[test]
    fn feed_and_local_inspection_share_forward_compatible_file_table() {
        let root = tempfile::tempdir().unwrap();
        let mut database = 0;
        // SAFETY: CREATE a disposable database only; never run an installer.
        ok(unsafe {
            MsiOpenDatabaseW(
                wide(root.path().join("payload.msi").to_str().unwrap()).as_ptr(),
                MSIDBOPEN_CREATE,
                &mut database,
            )
        })
        .unwrap();
        let database = Msi(database);
        let mut queries = vec![
            "CREATE TABLE `Directory` (`Directory` CHAR(72) NOT NULL, `Directory_Parent` CHAR(72), `DefaultDir` CHAR(255) NOT NULL PRIMARY KEY `Directory`)".to_owned(),
            "CREATE TABLE `Component` (`Component` CHAR(72) NOT NULL, `Directory_` CHAR(72) NOT NULL PRIMARY KEY `Component`)".into(),
            "CREATE TABLE `File` (`File` CHAR(72) NOT NULL, `Component_` CHAR(72) NOT NULL, `FileName` CHAR(255) NOT NULL, `FileSize` LONG NOT NULL PRIMARY KEY `File`)".into(),
            "INSERT INTO `Directory` (`Directory`, `Directory_Parent`, `DefaultDir`) VALUES ('AudioPlugin', 'INSTALLFOLDER', 'AudioPlugin')".into(),
            "INSERT INTO `Component` (`Component`, `Directory_`) VALUES ('Root', 'INSTALLFOLDER')".into(),
            "INSERT INTO `Component` (`Component`, `Directory_`) VALUES ('Audio', 'AudioPlugin')".into(),
        ];
        for (index, name) in FILES
            .iter()
            .chain([&"Microphone.ps1", &"future-helper.dll"])
            .enumerate()
        {
            let component = if index < FILES.len() { "Root" } else { "Audio" };
            queries.push(format!("INSERT INTO `File` (`File`, `Component_`, `FileName`, `FileSize`) VALUES ('F{index}', '{component}', '{name}', 1)"));
        }
        for query in queries {
            let mut view = 0;
            // SAFETY: fixture-only SQL against the owned disposable database.
            ok(unsafe { MsiDatabaseOpenViewW(database.0, wide(&query).as_ptr(), &mut view) }).unwrap();
            let view = Msi(view);
            // SAFETY: table queries only, no install sequence or product changes.
            ok(unsafe { MsiViewExecute(view.0, 0) }).unwrap();
        }
        // Feed and locally approved packages both enter inspect() and this
        // exact File/Component/Directory-table reader, before any installation.
        let (files, _) = file_table(&database).unwrap();
        assert_eq!(files.len(), FILES.len() + 2);
        assert_eq!(files[&format!("F{}", FILES.len())].0, "AudioPlugin/Microphone.ps1");
        assert_eq!(
            files[&format!("F{}", FILES.len() + 1)].0,
            "AudioPlugin/future-helper.dll"
        );
        let mut view = 0;
        // SAFETY: change only this fixture's directory ancestry to escape root.
        ok(unsafe { MsiDatabaseOpenViewW(database.0, wide("UPDATE `Directory` SET `Directory_Parent` = 'ProgramFiles64Folder' WHERE `Directory` = 'AudioPlugin'").as_ptr(), &mut view) }).unwrap();
        let view = Msi(view);
        // SAFETY: live fixture-only update view, never an installer action.
        ok(unsafe { MsiViewExecute(view.0, 0) }).unwrap();
        assert_eq!(file_table(&database), Err(Error::Metadata));
    }
    #[test]
    fn product_file_table_rejects_escape_aliases_and_maintenance_state() {
        let directories = BTreeMap::from([
            ("AudioPlugin".into(), ("INSTALLFOLDER".into(), "AudioPlugin".into())),
            ("Outside".into(), ("ProgramFiles64Folder".into(), "OtherProduct".into())),
            ("Cycle".into(), ("Cycle".into(), "cycle".into())),
            ("Maintenance".into(), ("INSTALLFOLDER".into(), "MaInTeNaNcE".into())),
            ("Nested".into(), ("Maintenance".into(), "nested".into())),
            ("Dot".into(), ("INSTALLFOLDER".into(), "..".into())),
            ("Separator".into(), ("INSTALLFOLDER".into(), "extra/level".into())),
        ]);
        for (directory, name) in [
            ("Outside", "escape.exe"),
            ("Missing", "escape.exe"),
            ("Cycle", "x"),
            ("AudioPlugin", "../escape.ps1"),
            ("AudioPlugin", "C:\u{005c}escape.ps1"),
            ("AudioPlugin", "/escape.ps1"),
            ("AudioPlugin", "safe/embedded.ps1"),
            ("AudioPlugin", "name:stream"),
            ("AudioPlugin", "NUL.ps1"),
            ("AudioPlugin", "trailing."),
            ("AudioPlugin", "trailing "),
            ("Maintenance", "pending.json"),
            ("Nested", "x"),
            ("INSTALLFOLDER", "maintenance"),
            ("INSTALLFOLDER", "MaInTeNaNcE"),
            ("INSTALLFOLDER", "Files.json"),
            ("INSTALLFOLDER", "Installed.json"),
            ("Dot", "x"),
            ("Separator", "x"),
        ] {
            assert!(
                relative_file(directory, name, &directories).is_err(),
                "{directory}/{name}"
            );
        }
    }
    #[test]
    fn current_package_metadata_and_licenses_are_inspected_without_path_escape() {
        let directories = BTreeMap::from([
            ("Licenses".into(), ("INSTALLFOLDER".into(), "licenses".into())),
            ("Dependency".into(), ("Licenses".into(), "dependency".into())),
            ("Cycle".into(), ("Cycle".into(), "cycle".into())),
        ]);
        for name in [
            "oink-build.json",
            "report-manifest.json",
            "installed.json",
            "THIRD_PARTY_LICENSES.md",
            "oink.exe",
        ] {
            assert_eq!(relative_file("INSTALLFOLDER", name, &directories).unwrap(), name);
        }
        assert_eq!(
            relative_file("Dependency", "LICENSE.txt", &directories).unwrap(),
            "licenses/dependency/LICENSE.txt"
        );
        for (directory, name) in [
            ("Dependency", "../escape"),
            ("Dependency", "NUL"),
            ("Cycle", "x"),
            ("ProgramFiles64Folder", "x"),
        ] {
            assert!(relative_file(directory, name, &directories).is_err());
        }
    }
}
