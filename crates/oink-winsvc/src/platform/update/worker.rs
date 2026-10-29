use super::super::{Handle, check, security, token, wide};
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use oink_update::{
    feed,
    transaction::{self, InstallOutcome, Journal, Platform, Stage},
    trust::{LocalRoots, Roots},
};
use sha2::{Digest, Sha256};
use std::{
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::*,
    System::{
        Diagnostics::ToolHelp::*, Registry::*, RemoteDesktop::*, Services::*, SystemInformation::*, Threading::*,
    },
};

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    serde_json::to_vec(value).map_err(|_| Error::Metadata)
}
fn write_plan(plan: &Plan) -> Result<(), Error> {
    protected::write(&transaction_dir(&plan.transaction)?.join("plan.json"), &json(plan)?)
}
fn system_dir() -> Result<PathBuf, Error> {
    let mut buf = [0u16; 32768];
    // SAFETY: correctly sized writable buffer for the OS-owned system path.
    let length = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if length == 0 || length >= buf.len() {
        return Err(Error::Storage);
    }
    Ok(PathBuf::from(
        String::from_utf16(&buf[..length]).map_err(|_| Error::Storage)?,
    ))
}
fn command(binary: &'static str) -> Result<Command, Error> {
    let directory = system_dir()?;
    let mut command = Command::new(directory.join(binary));
    command
        .env_clear()
        .envs(system_environment()?)
        .current_dir(protected::maintenance().map_err(|_| Error::Storage)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}
fn msi_arguments(operation: &str, target: &std::ffi::OsStr, log: &Path) -> Vec<std::ffi::OsString> {
    [
        std::ffi::OsStr::new(operation),
        target,
        std::ffi::OsStr::new("/qn"),
        std::ffi::OsStr::new("/norestart"),
        std::ffi::OsStr::new("/L*v!"),
        log.as_os_str(),
    ]
    .into_iter()
    .map(std::ffi::OsStr::to_owned)
    .collect()
}
fn repair_source_path(directory: &Path, name: &str) -> Result<PathBuf, Error> {
    // Windows Installer resolves repairs with the original registered basename,
    // even when /i names another file. Treat that OS metadata as one leaf only.
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .replace('¹', "1")
        .replace('²', "2")
        .replace('³', "3")
        .to_ascii_uppercase();
    if name.encode_utf16().count() > 255
        || !name.to_ascii_lowercase().ends_with(".msi")
        || name.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
        || name.ends_with(['.', ' '])
        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return Err(Error::Metadata);
    }
    Ok(directory.join("repair-source").join(name))
}
fn registered_package_name(product: &str) -> Result<String, Error> {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::*;
    let mut buffer = [0u16; 256];
    let mut length = buffer.len() as u32;
    // SAFETY: query only, exact independently inspected machine ProductCode;
    // no source-list, security policy or Windows Installer registry mutation.
    let code = unsafe {
        MsiSourceListGetInfoW(
            wide(product).as_ptr(),
            ptr::null(),
            MSIINSTALLCONTEXT_MACHINE,
            MSICODE_PRODUCT as u32,
            wide("PackageName").as_ptr(),
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if code != 0 || length as usize >= buffer.len() {
        return Err(Error::Metadata);
    }
    String::from_utf16(&buffer[..length as usize]).map_err(|_| Error::Metadata)
}
#[derive(Debug, PartialEq, Eq)]
enum RestorePackage {
    RepairInPlace,
    ReplaceProduct,
}
fn restore_package(previous: &str, candidate: &str, available: bool) -> Result<RestorePackage, Error> {
    if !available {
        return Err(Error::RepairRequired);
    }
    Ok(if previous == candidate {
        RestorePackage::RepairInPlace
    } else {
        RestorePackage::ReplaceProduct
    })
}
fn run_command(mut command: Command) -> Result<u32, Error> {
    // Keep the package and ancestor locks on our caller's stack until the MSI
    // engine exits. Timing out and dropping them leaves an in-flight install
    // using an unpinned path. A stalled engine stays pending; never kill it.
    command
        .status()
        .map_err(|_| Error::Transaction)?
        .code()
        .map(|n| n as u32)
        .ok_or(Error::Transaction)
}
pub(super) fn windows_build() -> Result<u32, Error> {
    let mut value = [0u16; 32];
    let mut bytes = size_of_val(&value) as u32;
    // SAFETY: fixed OS version value, read-only REG_SZ query into bounded buffer.
    let code = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion").as_ptr(),
            wide("CurrentBuildNumber").as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if code != 0 {
        return Err(Error::OsUnsupported);
    }
    let n = value.iter().position(|c| *c == 0).ok_or(Error::Metadata)?;
    String::from_utf16(&value[..n])
        .map_err(|_| Error::Metadata)?
        .parse()
        .map_err(|_| Error::Metadata)
}
fn installed_product_code() -> Result<String, Error> {
    let mut value = [0u16; 64];
    let mut bytes = size_of_val(&value) as u32;
    // SAFETY: fixed, read-only product registry record written by MSI.
    let code = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide(r"Software\Oink").as_ptr(),
            wide("ProductCode").as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if code != 0 {
        return Err(Error::Storage);
    }
    let length = value.iter().position(|c| *c == 0).ok_or(Error::Metadata)?;
    String::from_utf16(&value[..length]).map_err(|_| Error::Metadata)
}
fn cache(build: u64) -> Result<PathBuf, Error> {
    Ok(protected::maintenance()
        .map_err(|_| Error::Storage)?
        .join("cache")
        .join(build.to_string()))
}
// A protected local plan is authorized by the elevated installed CLI, not by
// the feed enablement gate. Keep distinct types so its pins cannot authenticate
// a feed manifest. In-house packages intentionally disable automatic updates.
#[derive(Debug)]
pub(super) enum PackageTrust {
    Feed(Roots),
    Local(LocalRoots),
}
impl PackageTrust {
    fn compiled(local: bool) -> Result<Self, Error> {
        if local {
            oink_update::trust::local_packages().map(Self::Local)
        } else {
            oink_update::trust::compiled().map(Self::Feed)
        }
    }
    fn pins(&self) -> &[String] {
        match self {
            Self::Feed(roots) => &roots.signer_spki_sha256,
            Self::Local(roots) => &roots.signer_spki_sha256,
        }
    }
    fn feed(&self) -> Result<feed::Trust<'_>, Error> {
        match self {
            Self::Feed(roots) => Ok(roots.feed()),
            Self::Local(_) => Err(Error::Policy),
        }
    }
}
pub(super) fn prepare_package(
    candidate: feed::Release,
    previous: Option<feed::Release>,
    envelope: &[u8],
    roots: &PackageTrust,
    local: bool,
) -> Result<PreparedPackage, Error> {
    if local != matches!(roots, PackageTrust::Local(_)) {
        return Err(Error::Policy);
    }
    protected::system().map_err(|_| Error::Policy)?;
    let _lock = protected::lock()?;
    if pending() {
        return Err(Error::Busy);
    }
    let receipt = installed()?;
    let machine = policy()?;
    let floor = if local {
        receipt.build
    } else {
        receipt.build.max(machine.successful_build)
    };
    if candidate.build < floor || (!local && candidate.build == floor) {
        return Err(Error::Replay);
    }
    if local {
        msi::local_metadata(
            &protected::maintenance()
                .map_err(|_| Error::Storage)?
                .join("downloads/candidate.msi"),
            candidate.package.length,
            candidate.package.sha256.clone(),
        )?
        .validate(&receipt.version, receipt.build, "x64")?;
    } else {
        feed::verify_version(&candidate.version, &receipt.version)?;
    }
    let maintenance = protected::maintenance().map_err(|_| Error::Storage)?;
    for name in ["transactions", "cache"] {
        protected::directory(&maintenance.join(name)).map_err(|_| Error::Storage)?;
    }
    let previous_directory = cache(receipt.build)?;
    protected::directory(&previous_directory).map_err(|_| Error::Storage)?;
    let previous = match previous {
        Some(previous) => previous,
        None if previous_directory.join("release.json").exists() => {
            feed::strict_json(&protected::read(&previous_directory.join("release.json"), 65536)?)?
        }
        None if local => oink_update::local::Package {
            product: oink_update::PRODUCT.into(),
            version: receipt.version.clone(),
            build: receipt.build,
            architecture: "x64".into(),
            source_tree_sha256: receipt.source_tree_sha256.clone(),
            minimum_windows_build: receipt.minimum_windows_build,
            length: 0,
            sha256: "0".repeat(64),
            notes: None,
        }
        .release(),
        None => return Err(Error::Storage),
    };
    if previous.build != receipt.build {
        return Err(Error::Policy);
    }
    let previous_msi = previous_directory.join("package.msi");
    if !local && !previous_msi.exists() && !previous.package.url.is_empty() {
        let bytes = service::http(&previous.package.url, previous.package.length, true)?;
        feed::verify_package(&bytes, &previous.package, &roots.feed()?)?;
        protected::write(&previous_msi, &bytes)?;
        protected::write(&previous_directory.join("release.json"), &json(&previous)?)?;
    }
    let rollback_package = previous_msi.exists();
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| Error::Storage)?;
    let tx = feed::hex(&random);
    let directory = transaction_dir(&tx)?;
    protected::directory(&directory).map_err(|_| Error::Storage)?;
    let candidate_msi = directory.join("candidate.msi");
    let candidate_bytes = protected::read(&maintenance.join("downloads/candidate.msi"), feed::MAX_PACKAGE)?;
    protected::write(&candidate_msi, &candidate_bytes)?;
    protected::write(&directory.join("manifest.json"), envelope)?;
    let candidate_inventory = msi::inspect(
        &candidate_msi,
        &directory.join("candidate-check"),
        roots.pins(),
        &candidate,
    )?;
    let previous_inventory = cached_inventory(&receipt)?;
    if candidate_inventory.product_code == previous_inventory.product_code {
        // Refuse unusable source metadata before the original session drains.
        repair_source_path(&directory, &registered_package_name(&candidate_inventory.product_code)?)?;
    }
    let root = protected::root().map_err(|_| Error::Storage)?;
    let source = root.join("oink-update-worker.exe");
    signature::verify(&source, roots.pins())?;
    let worker = directory.join("worker.exe");
    let bytes = protected::read(&source, 64 * 1024 * 1024)?;
    protected::write(&worker, &bytes)?;
    signature::verify(&worker, roots.pins())?;
    let mut free = 0;
    // SAFETY: fixed protected installation volume; query-only space check before stopping.
    check(unsafe {
        GetDiskFreeSpaceExW(
            wide(root.to_str().ok_or(Error::Storage)?).as_ptr(),
            &mut free,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    })
    .map_err(|_| Error::Storage)?;
    if free < feed::MAX_UNPACKED * 2 + candidate.package.length + previous.package.length {
        return Err(Error::Storage);
    }
    let candidate_pin = protected::pin(&candidate_msi).map_err(|_| Error::Storage)?;
    Ok(PreparedPackage {
        transaction: tx,
        local,
        rollback_package,
        candidate,
        previous,
        candidate_inventory,
        previous_inventory,
        worker_sha256: feed::hex(&Sha256::digest(&bytes)),
        _candidate_pin: candidate_pin,
    })
}
pub(super) struct PreparedPackage {
    pub transaction: String,
    local: bool,
    rollback_package: bool,
    candidate: feed::Release,
    previous: feed::Release,
    candidate_inventory: msi::Inventory,
    previous_inventory: msi::Inventory,
    worker_sha256: String,
    _candidate_pin: protected::PinnedFile,
}
fn cached_inventory(receipt: &msi::Receipt) -> Result<msi::Inventory, Error> {
    let destination = cache(receipt.build)?;
    protected::directory(&destination).map_err(|_| Error::Storage)?;
    let path = destination.join("inventory.json");
    let inventory = if path.exists() {
        feed::strict_json(&protected::read(&path, feed::MAX_FEED as u64)?)?
    } else {
        // A manual MSI install has no previous updater commit. Capture the
        // protected installed payload as its rollback-health baseline once;
        // do not open/extract its MSI or compare installed files with that MSI.
        let inventory = msi::installed_inventory(receipt, installed_product_code()?)?;
        protected::write(&path, &json(&inventory)?)?;
        inventory
    };
    if inventory.build != receipt.build || inventory.product_code != installed_product_code()? {
        return Err(Error::Metadata);
    }
    Ok(inventory)
}
pub(super) fn stage(prepared: &PreparedPackage, sessions: Vec<Session>) -> Result<String, Error> {
    protected::system().map_err(|_| Error::Policy)?;
    let lock = protected::lock()?;
    if pending() {
        return Err(Error::Busy);
    }
    let receipt = installed()?;
    let machine = policy()?;
    if receipt.build != prepared.previous.build
        || prepared.candidate.build < receipt.build
        || (!prepared.local && prepared.candidate.build <= receipt.build.max(machine.successful_build))
        || (!prepared.local && machine.quarantine.contains(&prepared.candidate.build))
    {
        return Err(Error::Replay);
    }
    let tx = prepared.transaction.clone();
    let directory = transaction_dir(&tx)?;
    let worker = directory.join("worker.exe");
    let mut plan = Plan {
        schema: 1,
        local: prepared.local,
        rollback_package: prepared.rollback_package,
        transaction: tx.clone(),
        candidate: prepared.candidate.clone(),
        previous: prepared.previous.clone(),
        candidate_inventory: prepared.candidate_inventory.clone(),
        previous_inventory: prepared.previous_inventory.clone(),
        sessions,
        reboot_marker_armed: false,
        lock_handle: format!("{:016x}", lock.0 as usize),
        worker_pid: 0,
        worker_created_filetime: "0000000000000000".into(),
        worker_sha256: prepared.worker_sha256.clone(),
    };
    write_plan(&plan)?;
    let maintenance = protected::maintenance().map_err(|_| Error::Storage)?;
    let journal = Journal::new(
        tx.clone(),
        plan.previous.build,
        plan.candidate.build,
        plan.candidate.package.sha256.clone(),
    )?;
    protected::write(&maintenance.join("pending.json"), &json(&journal)?)?;
    let (process, thread) = spawn_worker(&worker, &tx, &lock)?;
    // SAFETY: returned live child process handle pins its identity before it executes.
    plan.worker_pid = unsafe { GetProcessId(process.0) };
    plan.worker_created_filetime = format!("{:016x}", protected::created(process.0)?);
    write_plan(&plan)?;
    // SAFETY: child was created suspended, after plan and pending became durable.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(Error::Transaction);
    }
    Ok(tx)
}
fn spawn_worker(worker: &Path, tx: &str, lock: &Handle) -> Result<(Handle, Handle), Error> {
    let mut length = 0usize;
    // SAFETY: first call obtains required attribute-list size; no output buffer yet.
    unsafe {
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut length);
    }
    let mut storage = vec![0usize; length.div_ceil(size_of::<usize>())];
    let list = storage.as_mut_ptr().cast();
    // SAFETY: aligned storage of returned size; initialized list lives through process creation.
    check(unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut length) }).map_err(|_| Error::Storage)?;
    let mut only_handle = lock.0;
    // SAFETY: the one inherited handle is the exclusive maintenance lock, never a user token.
    let result = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            ptr::addr_of_mut!(only_handle).cast(),
            size_of::<HANDLE>(),
            ptr::null_mut(),
            ptr::null(),
        )
    };
    if result == 0 {
        // SAFETY: release the successfully initialized attribute list.
        unsafe {
            DeleteProcThreadAttributeList(list);
        }
        return Err(Error::Storage);
    }
    let app = wide(worker.to_str().ok_or(Error::Storage)?);
    let mut args = wide(&format!("\"{}\" {tx}", worker.display()));
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = list;
    let mut process = PROCESS_INFORMATION::default();
    let mut environment = environment()?;
    let directory = wide(worker.parent().and_then(Path::to_str).ok_or(Error::Storage)?);
    // SAFETY: fixed signed old worker, validated hex argument, controlled environment,
    // explicit handle allowlist. The SYSTEM creator's token is used unchanged.
    let created = unsafe {
        CreateProcessW(
            app.as_ptr(),
            args.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_mut_ptr().cast(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    };
    // SAFETY: no process creation call retains this attribute buffer.
    unsafe {
        DeleteProcThreadAttributeList(list);
    }
    check(created).map_err(|_| Error::Transaction)?;
    Ok((
        Handle::new(process.hProcess).map_err(|_| Error::Transaction)?,
        Handle::new(process.hThread).map_err(|_| Error::Transaction)?,
    ))
}
fn environment() -> Result<Vec<u16>, Error> {
    use std::os::windows::ffi::OsStrExt;
    let mut value = Vec::new();
    for (key, path) in system_environment()? {
        value.extend(key.encode_utf16());
        value.push(u16::from(b'='));
        value.extend(path.as_os_str().encode_wide());
        value.push(0);
    }
    value.push(0);
    Ok(value)
}

fn system_environment() -> Result<[(&'static str, PathBuf); 3], Error> {
    let system = system_dir()?;
    let root = system.parent().ok_or(Error::Storage)?.to_owned();
    let Some(std::path::Component::Prefix(prefix)) = system.components().next() else {
        return Err(Error::Storage);
    };
    let drive = PathBuf::from(prefix.as_os_str());
    // The worker later calls CreateEnvironmentBlock(user, FALSE) to restart the
    // backend/UI. On Windows 11 26200 userenv still takes SystemDrive from its
    // caller: omitting it leaves ProfileList's %SystemDrive% paths unexpanded,
    // and SHGetKnownFolderPath then fails its default existence check. Merely
    // rebuilding the user's environment did not repair that missing seed.
    // Supply the OS-reported drive before userenv runs, also for MSI children;
    // keep the SYSTEM environment allow-listed, without inheriting user input.
    // https://learn.microsoft.com/windows/win32/api/userenv/nf-userenv-createenvironmentblock
    // https://learn.microsoft.com/windows/win32/api/shlobj_core/ne-shlobj_core-known_folder_flag
    Ok([("PATH", system), ("SystemDrive", drive), ("SystemRoot", root)])
}
struct Worker {
    plan: Plan,
    directory: PathBuf,
    roots: PackageTrust,
    lock: Option<Handle>,
    failure: Option<FailurePolicy>,
    canceled: bool,
}
pub(super) fn run() -> Result<(), Error> {
    protected::system().map_err(|_| Error::Policy)?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [tx] = args.as_slice() else {
        return Err(Error::Policy);
    };
    let directory = transaction_dir(tx)?;
    // SAFETY: query this process's kernel image, never argv[0] or cwd.
    let image = security::image(unsafe { GetCurrentProcess() }).map_err(|_| Error::Policy)?;
    if !image
        .as_os_str()
        .eq_ignore_ascii_case(directory.join("worker.exe").as_os_str())
    {
        return Err(Error::Policy);
    }
    let plan: Plan = feed::strict_json(&protected::read(&directory.join("plan.json"), feed::MAX_FEED as u64)?)?;
    if plan.schema != 1 || plan.transaction != *tx || protected::hash(&image)? != plan.worker_sha256 {
        return Err(Error::Signature);
    }
    let roots = PackageTrust::compiled(plan.local)?;
    signature::verify(&image, roots.pins())?;
    let mut journal = Journal::parse(&protected::read(
        &protected::maintenance()
            .map_err(|_| Error::Storage)?
            .join("pending.json"),
        65536,
    )?)?;
    if journal.transaction != *tx {
        return Err(Error::Transaction);
    }
    let mut worker = Worker {
        plan,
        directory,
        roots,
        lock: None,
        failure: None,
        canceled: false,
    };
    transaction::execute(&mut worker, &mut journal)
}

impl Worker {
    fn verify_candidate_manifest(&self) -> Result<(), Error> {
        let machine = policy()?;
        if self.plan.local {
            let path = self.directory.join("candidate.msi");
            let _pin = protected::pin(&path).map_err(|_| Error::Storage)?;
            if protected::hash(&path)? != self.plan.candidate.package.sha256 {
                return Err(Error::Signature);
            }
            let package = msi::local_metadata(
                &path,
                self.plan.candidate.package.length,
                self.plan.candidate.package.sha256.clone(),
            )?;
            package.validate(&self.plan.previous.version, self.plan.previous.build, "x64")?;
            if package.release() != self.plan.candidate {
                return Err(Error::Metadata);
            }
            return Ok(());
        }
        let manifest = feed::verify_manifest(
            &protected::read(&self.directory.join("manifest.json"), feed::MAX_FEED as u64)?,
            &self.roots.feed()?,
            &machine.watermarks.get("stable").cloned().unwrap_or_default(),
            now(),
        )?;
        feed::verify_version(&self.plan.candidate.version, &self.plan.previous.version)?;
        if !manifest.manifest().releases.contains(&self.plan.candidate)
            || self.plan.candidate.architecture != "x64"
            || self.plan.candidate.build <= self.plan.previous.build.max(machine.successful_build)
            || machine.quarantine.contains(&self.plan.candidate.build)
        {
            return Err(Error::Policy);
        }
        Ok(())
    }
    fn save(&self, journal: &Journal) -> Result<(), Error> {
        protected::write(
            &protected::maintenance()
                .map_err(|_| Error::Storage)?
                .join("pending.json"),
            &json(journal)?,
        )
    }
    fn authorize(
        &self,
        target: &msi::Inventory,
        package: &feed::Package,
        remove: Option<(&msi::Inventory, &feed::Package)>,
    ) -> Result<(), Error> {
        #[derive(Serialize)]
        struct Authorization<'a> {
            schema: u32,
            product_code: &'a str,
            package_sha256: &'a str,
            remove_product_code: &'a str,
            remove_package_sha256: &'a str,
        }
        let maintenance = protected::maintenance().map_err(|_| Error::Storage)?;
        self.lock.as_ref().ok_or(Error::Transaction)?;
        let auth = Authorization {
            schema: 1,
            product_code: &target.product_code,
            package_sha256: &package.sha256,
            remove_product_code: remove.map(|(i, _)| i.product_code.as_str()).unwrap_or(""),
            remove_package_sha256: remove.map(|(_, p)| p.sha256.as_str()).unwrap_or(""),
        };
        protected::write(&maintenance.join("msi-authorization.json"), &json(&auth)?)
    }
    fn install(&self, rollback: bool) -> Result<InstallOutcome, Error> {
        if !rollback {
            // Recheck manifest freshness immediately before the candidate MSI.
            // Expired metadata must never authorize a new invocation;
            // restoring the previously verified version remains possible offline.
            self.verify_candidate_manifest()?;
        }
        let (inventory, release, path, removal_inventory, removal_release) = if rollback {
            (
                &self.plan.previous_inventory,
                &self.plan.previous,
                cache(self.plan.previous.build)?.join("package.msi"),
                &self.plan.candidate_inventory,
                &self.plan.candidate,
            )
        } else {
            (
                &self.plan.candidate_inventory,
                &self.plan.candidate,
                self.directory.join("candidate.msi"),
                &self.plan.previous_inventory,
                &self.plan.previous,
            )
        };
        let mut removal_package = removal_release.package.clone();
        let removal = if inventory.product_code != removal_inventory.product_code
            && product_installed(&removal_inventory.product_code)
        {
            removal_package.sha256 = cached_package_hash(&removal_inventory.product_code)?;
            Some((removal_inventory, &removal_package))
        } else {
            None
        };
        // Package bytes were verified before staging. The protected copy is
        // immutable to users; hold its read-only handle while msiexec opens it.
        let _package = protected::pin(&path).map_err(|_| Error::Storage)?;
        let repair = product_installed(&inventory.product_code);
        let path = if repair {
            let source = repair_source_path(&self.directory, &registered_package_name(&inventory.product_code)?)?;
            protected::directory(source.parent().ok_or(Error::Storage)?).map_err(|_| Error::Storage)?;
            let bytes = protected::read(&path, feed::MAX_PACKAGE)?;
            if feed::hex(&Sha256::digest(&bytes)) != release.package.sha256 {
                return Err(Error::Signature);
            }
            protected::write(&source, &bytes)?;
            source
        } else {
            path
        };
        let _source = protected::pin(&path).map_err(|_| Error::Storage)?;
        self.authorize(inventory, &release.package, removal)?;
        let mut install = command("msiexec.exe")?;
        install.args(msi_arguments(
            "/i",
            path.as_os_str(),
            &self
                .directory
                .join(if rollback { "rollback.log" } else { "install.log" }),
        ));
        if repair {
            // A same-ProductCode repair must recache this explicitly approved
            // MSI rather than silently repairing from the previous cached one.
            install.args(["REINSTALL=ALL", "REINSTALLMODE=vamus"]);
        }
        self.installer_outcome(run_command(install)?)
    }
    fn installer_outcome(&self, code: u32) -> Result<InstallOutcome, Error> {
        let result = outcome(code)?;
        if result == InstallOutcome::RebootRequired {
            // Every 3010 needs a subsequent reboot, including rollback after a
            // candidate reboot. Re-arm the volatile marker for this boot.
            mark_boot(&self.plan.transaction)?;
        }
        Ok(result)
    }
    fn remove_candidate(&self) -> Result<InstallOutcome, Error> {
        // SAFETY: read-only product state query for the independently inspected ProductCode.
        let state = unsafe {
            windows_sys::Win32::System::ApplicationInstallationAndServicing::MsiQueryProductStateW(
                wide(&self.plan.candidate_inventory.product_code).as_ptr(),
            )
        };
        if state == windows_sys::Win32::System::ApplicationInstallationAndServicing::INSTALLSTATE_UNKNOWN {
            return Ok(InstallOutcome::Installed);
        }
        let cached_hash = cached_package_hash(&self.plan.candidate_inventory.product_code)?;
        let mut cached_package = self.plan.candidate.package.clone();
        cached_package.sha256 = cached_hash;
        self.authorize(&self.plan.candidate_inventory, &cached_package, None)?;
        let mut uninstall = command("msiexec.exe")?;
        uninstall.args(msi_arguments(
            "/x",
            self.plan.candidate_inventory.product_code.as_ref(),
            &self.directory.join("remove-candidate.log"),
        ));
        self.installer_outcome(run_command(uninstall)?)
    }
    fn health(&self, previous: bool) -> Result<(), Error> {
        let inventory = if previous {
            &self.plan.previous_inventory
        } else {
            &self.plan.candidate_inventory
        };
        let root = protected::root().map_err(|_| Error::Storage)?;
        for (name, hash) in &inventory.hashes {
            if protected::hash(&root.join(name))? != *hash {
                return Err(Error::Signature);
            }
            if name.ends_with(".exe") {
                signature::verify(&root.join(name), self.roots.pins())?;
            }
        }
        let receipt = installed()?;
        if receipt.build != inventory.build {
            return Err(Error::Transaction);
        }
        protected::write(&self.directory.join("health-authorized.json"), b"{\"schema\":1}")?;
        scm_start()?;
        let live = super::pipe::sessions().map_err(|_| Error::Transaction)?;
        // Restart only the same still-logged-on SID/session identities. No token,
        // environment, data directory or privileged user is supplied by IPC.
        for original in &self.plan.sessions {
            if !live.contains(&original.id) {
                continue;
            }
            if STANDARD.encode(security::user_sid(original.id).map_err(|_| Error::Policy)?) != original.sid {
                return Err(Error::Policy);
            }
            if process_current(original)? {
                continue;
            }
            super::super::backend::spawn_update(
                original.id,
                &STANDARD.decode(&original.sid).map_err(|_| Error::Metadata)?,
            )
            .map_err(|_| Error::Transaction)?;
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let mut complete = true;
            for original in &self.plan.sessions {
                if !live.contains(&original.id) {
                    continue;
                }
                let path = self.directory.join(format!("health-{}.json", original.id));
                let proof: HealthProof = match protected::read(&path, 65536).and_then(|b| feed::strict_json(&b)) {
                    Ok(health) => health,
                    Err(_) => {
                        complete = false;
                        continue;
                    }
                };
                let health = proof.session;
                if now().saturating_sub(proof.observed_unix) > 3
                    || proof.observed_unix > now() + 1
                    || health.id != original.id
                    || health.sid != original.sid
                    || health.build != inventory.build
                    || !process_current(&health)?
                {
                    complete = false;
                }
            }
            if complete {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Transaction);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}
fn outcome(code: u32) -> Result<InstallOutcome, Error> {
    match code {
        0 => Ok(InstallOutcome::Installed),
        3010 => Ok(InstallOutcome::RebootRequired),
        1618 => Ok(InstallOutcome::Busy),
        _ => Err(Error::Transaction),
    }
}
impl Platform for Worker {
    fn verify_and_lock(&mut self, journal: &Journal) -> Result<(), Error> {
        if journal.transaction != self.plan.transaction
            || journal.new_build != self.plan.candidate.build
            || journal.old_build != self.plan.previous.build
            || journal.package_sha256 != self.plan.candidate.package.sha256
        {
            return Err(Error::Transaction);
        }
        self.lock = Some(match inherited_lock(&self.plan)? {
            Some(lock) => lock,
            None => protected::lock()?,
        });
        // Refresh the exact executing identity for boot recovery, after exclusive lock.
        // SAFETY: own process pseudo handle and PID are immutable for this lifetime.
        self.plan.worker_pid = unsafe { GetCurrentProcessId() };
        // SAFETY: query-only process pseudo handle.
        self.plan.worker_created_filetime = format!("{:016x}", protected::created(unsafe { GetCurrentProcess() })?);
        self.plan.lock_handle = format!("{:016x}", self.lock.as_ref().unwrap().0 as usize);
        write_plan(&self.plan)?;
        if self.directory.join("failure-policy.json").exists() {
            self.failure = Some(feed::strict_json(&protected::read(
                &self.directory.join("failure-policy.json"),
                65536,
            )?)?);
        }
        self.canceled = journal.stage == Stage::Prepared && self.directory.join("abort.json").exists();
        // The service verified and inspected each package once before persisting
        // this SYSTEM-owned plan. Recovery trusts that protected state.
        Ok(())
    }
    fn persist(&mut self, journal: &Journal) -> Result<(), Error> {
        self.save(journal)
    }
    fn arm_recovery(&mut self) -> Result<(), Error> {
        if self.canceled {
            return Ok(());
        }
        arm_task(&self.plan, &self.directory)?;
        if !self.plan.reboot_marker_armed {
            mark_boot(&self.plan.transaction)?;
            self.plan.reboot_marker_armed = true;
            write_plan(&self.plan)?;
        }
        Ok(())
    }
    fn confirm_drain_and_handover(&mut self) -> Result<(), Error> {
        if self.canceled || self.directory.join("abort.json").exists() {
            return Err(Error::Busy);
        }
        protected::write(&self.directory.join("worker-ready.json"), &json(&self.plan.worker_pid)?)?;
        let deadline = Instant::now() + Duration::from_secs(25);
        while !self.directory.join("drained.json").exists() {
            if self.directory.join("abort.json").exists() || Instant::now() >= deadline {
                return Err(Error::Busy);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let sessions: Vec<Session> = feed::strict_json(&protected::read(&self.directory.join("drained.json"), 65536)?)?;
        if json(&sessions)? != json(&self.plan.sessions)? {
            return Err(Error::Policy);
        }
        for session in &sessions {
            if STANDARD.encode(security::user_sid(session.id).map_err(|_| Error::Policy)?) != session.sid {
                return Err(Error::Policy);
            }
        }
        Ok(())
    }
    fn stop_owned_processes(&mut self) -> Result<(), Error> {
        // Original users already accepted shutdown through their live drain token.
        // Wait for real process exit; never kill by executable name or force a busy backend.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let alive = self
                .plan
                .sessions
                .iter()
                .map(process_current)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .any(|v| v);
            if !alive {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Error::Busy);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.failure.is_none() {
            let policy = scm_failure_policy()?;
            protected::write(&self.directory.join("failure-policy.json"), &json(&policy)?)?;
            self.failure = Some(policy);
        }
        set_failure_policy(&FailurePolicy::default())?;
        // A failed candidate's newly restarted backend must also ACK the local
        // drain and actually exit before its helper/service may be stopped.
        for process in product_processes(false)? {
            // SAFETY: pinned user product process, wait only; never terminate.
            if unsafe { WaitForSingleObject(process.0, 20000) } != WAIT_OBJECT_0 {
                return Err(Error::Busy);
            }
        }
        let processes = product_processes(true)?;
        scm_stop()?;
        for process in processes {
            // SAFETY: pinned product-process handle, bounded wait; no termination privilege.
            if unsafe { WaitForSingleObject(process.0, 15000) } != WAIT_OBJECT_0 {
                return Err(Error::Busy);
            }
        }
        Ok(())
    }
    fn replace_components_and_metadata(&mut self) -> Result<InstallOutcome, Error> {
        self.install(false)
    }
    fn reboot_observed(&mut self) -> Result<bool, Error> {
        boot_changed(&self.plan)
    }
    fn start_service_under_maintenance(&mut self) -> Result<(), Error> {
        protected::write(&self.directory.join("health-authorized.json"), b"{\"schema\":1}")?;
        scm_start()
    }
    fn health_check_original_sessions(&mut self) -> Result<(), Error> {
        self.health(false)
    }
    fn commit_receipt(&mut self) -> Result<(), Error> {
        let destination = cache(self.plan.candidate.build)?;
        protected::directory(&destination).map_err(|_| Error::Storage)?;
        cache_verified_package(
            &destination,
            &self.plan.candidate,
            &self.plan.candidate_inventory,
            &protected::read(&self.directory.join("candidate.msi"), feed::MAX_PACKAGE)?,
            protected::write,
        )?;
        let mut machine = policy()?;
        machine.record_success(self.plan.candidate.build);
        save_policy(&machine)
    }
    fn restore_previous_components_and_metadata(&mut self) -> Result<InstallOutcome, Error> {
        // A prior /i may have committed before a crash or 3010. Verify health
        // instead of reinstalling it on every boot (which may request reboot again).
        let same_product = self.plan.previous_inventory.product_code == self.plan.candidate_inventory.product_code;
        let previous_intact = same_product && installed()?.build == self.plan.previous_inventory.build && {
            let root = protected::root().map_err(|_| Error::Storage)?;
            self.plan
                .previous_inventory
                .hashes
                .iter()
                .all(|(name, hash)| protected::hash(&root.join(name)).is_ok_and(|actual| actual == *hash))
        };
        if product_installed(&self.plan.previous_inventory.product_code)
            && (!product_installed(&self.plan.candidate_inventory.product_code) || previous_intact)
        {
            return Ok(InstallOutcome::Installed);
        }
        // Older manual/in-house installations may have no complete prior MSI.
        // Never uninstall a working candidate without an authenticated restore
        // package: that would destroy user data. Preserve it for explicit repair.
        if restore_package(
            &self.plan.previous_inventory.product_code,
            &self.plan.candidate_inventory.product_code,
            self.plan.rollback_package,
        )? == RestorePackage::RepairInPlace
        {
            return self.install(true);
        }
        // Authorize cached /x and verified prior /i separately under one lock.
        match self.remove_candidate()? {
            InstallOutcome::Installed => self.install(true),
            other => Ok(other),
        }
    }
    fn resume_before_replacement(&mut self) -> Result<(), Error> {
        protected::write(&self.directory.join("abort.json"), b"{\"schema\":1}")?;
        self.health(true)
    }
    fn health_check_previous(&mut self) -> Result<(), Error> {
        self.health(true)
    }
    fn quarantine_candidate(&mut self) -> Result<(), Error> {
        let mut machine = policy()?;
        machine.quarantine.insert(self.plan.candidate.build);
        save_policy(&machine)
    }
    fn restore_failure_policy_and_cleanup(&mut self) -> Result<(), Error> {
        if let Some(policy) = &self.failure {
            set_failure_policy(policy)?;
        }
        super::task::remove(&self.plan.transaction)?;
        let maintenance = protected::maintenance().map_err(|_| Error::Storage)?;
        let journal = protected::read(&maintenance.join("pending.json"), 65536)?;
        protected::write(&maintenance.join("last-result.json"), &journal)?;
        clear_boot_markers(HKEY_LOCAL_MACHINE, r"Software\Oink")?;
        remove_completed_files(&maintenance, protected::open)?;
        // Only after health commit/rollback and pending removal: ordinary UI
        // activation follows the same entry as the installer's Launch Oink.
        if self.directory.join("drained.json").exists() {
            let reopen = || -> Result<(), Error> {
                let live = super::pipe::sessions().map_err(|_| Error::Transaction)?;
                for original in &self.plan.sessions {
                    if live.contains(&original.id)
                        && STANDARD.encode(security::user_sid(original.id).map_err(|_| Error::Policy)?) == original.sid
                    {
                        super::super::backend::spawn_update_ui(
                            original.id,
                            &STANDARD.decode(&original.sid).map_err(|_| Error::Metadata)?,
                        )
                        .map_err(|_| Error::Transaction)?;
                    }
                }
                Ok(())
            };
            if let Err(error) = reopen() {
                // UI activation is best effort after the durable product result;
                // it must not turn an already completed rollback into RepairRequired.
                tracing::warn!(%error, "could not reopen Oink after update");
            }
        }
        Ok(())
    }
}
fn cache_verified_package(
    directory: &Path,
    release: &feed::Release,
    inventory: &msi::Inventory,
    bytes: &[u8],
    mut write: impl FnMut(&Path, &[u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    write(&directory.join("package.msi"), bytes)?;
    write(&directory.join("release.json"), &json(release)?)?;
    write(&directory.join("inventory.json"), &json(inventory)?)
}
fn remove_completed_files(
    maintenance: &Path,
    mut open: impl FnMut(&Path) -> std::io::Result<std::fs::File>,
) -> Result<(), Error> {
    // Keep pending until all other cleanup is durable. A crash before its
    // removal resumes the terminal cleanup, without repeating installation.
    for name in [
        "downloads/candidate.msi",
        "downloads/candidate.new",
        "msi-authorization.json",
        "pending.json",
    ] {
        let path = maintenance.join(name);
        if path.exists() {
            let verified = open(&path).map_err(|_| Error::Storage)?;
            drop(verified);
            std::fs::remove_file(path).map_err(|_| Error::Storage)?;
        }
    }
    Ok(())
}
pub(super) fn cleanup_completed() -> Result<(), Error> {
    let _lock = protected::lock()?;
    if pending() {
        return Ok(());
    }
    let receipt = installed()?;
    let _inventory = cached_inventory(&receipt)?;
    if protected::prune_completed(receipt.build)? {
        clear_boot_markers(HKEY_LOCAL_MACHINE, r"Software\Oink")?;
    }
    Ok(())
}
fn clear_boot_markers(hive: HKEY, parent: &str) -> Result<(), Error> {
    let mut key = ptr::null_mut();
    // SAFETY: fixed updater-owned registry parent in production; the native
    // regression supplies only a private HKCU fixture. Caller holds the lock.
    let code = unsafe {
        RegOpenKeyExW(
            hive,
            wide(parent).as_ptr(),
            0,
            KEY_READ | KEY_WRITE | DELETE | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if code != 0 {
        return Err(Error::Storage);
    }
    // SAFETY: owned handle with the delete/enumerate/query rights required by
    // RegDeleteTreeW. No pending transaction needs any marker at this point.
    let code = unsafe {
        let code = RegDeleteTreeW(key, wide("UpdateBoot").as_ptr());
        RegCloseKey(key);
        code
    };
    if code != 0 && code != ERROR_FILE_NOT_FOUND {
        return Err(Error::Storage);
    }
    Ok(())
}
pub(super) fn process_current(session: &Session) -> Result<bool, Error> {
    // SAFETY: query-only process open; no exit/termination privilege requested.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, session.pid) };
    if raw.is_null() {
        // SAFETY: immediate last-error read for failed OpenProcess.
        return if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
            Ok(false)
        } else {
            Err(Error::Policy)
        };
    }
    let process = Handle::new(raw).map_err(|_| Error::Policy)?;
    // SAFETY: owned process handle, nonblocking state query.
    if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0
        || protected::created(process.0)? != session.created
    {
        return Ok(false);
    }
    if STANDARD.encode(security::sid_bytes(token(process.0).map_err(|_| Error::Policy)?.0).map_err(|_| Error::Policy)?)
        != session.sid
    {
        return Err(Error::Policy);
    }
    let mut id = 0;
    // SAFETY: process remains pinned and output DWORD valid.
    check(unsafe { ProcessIdToSessionId(session.pid, &mut id) }).map_err(|_| Error::Policy)?;
    if id != session.id {
        return Err(Error::Policy);
    }
    Ok(true)
}
fn product_processes(include_service: bool) -> Result<Vec<Handle>, Error> {
    // SAFETY: read-only system process snapshot.
    let snapshot =
        Handle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }).map_err(|_| Error::Storage)?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let root = protected::root().map_err(|_| Error::Storage)?;
    let mut handles = Vec::new();
    // SAFETY: live snapshot and correctly sized writable entry.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while more != 0 {
        let n = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..n]);
        if (include_service || !name.eq_ignore_ascii_case("oink-service.exe"))
            && [
                "oink.exe",
                "oink-agent.exe",
                "oink-service.exe",
                "oink-crash-collector.exe",
            ]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
        {
            // SAFETY: pin only candidate product processes; actual image path is checked next.
            let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, entry.th32ProcessID) };
            if raw.is_null() {
                return Err(Error::Busy);
            }
            let process = Handle::new(raw).map_err(|_| Error::Busy)?;
            let image = security::image(process.0).map_err(|_| Error::Policy)?;
            if image
                .parent()
                .is_some_and(|parent| parent.as_os_str().eq_ignore_ascii_case(root.as_os_str()))
            {
                handles.push(process);
            }
        }
        // SAFETY: same live snapshot and output entry.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    Ok(handles)
}
fn inherited_lock(plan: &Plan) -> Result<Option<Handle>, Error> {
    // A boot-recovery process must not trust a stale numeric handle. Only accept
    // the original executing PID+creation identity and its exact protected file.
    // SAFETY: own PID and process pseudo-handle are valid.
    let (pid, process) = unsafe { (GetCurrentProcessId(), GetCurrentProcess()) };
    if plan.worker_pid != pid || plan.worker_created_filetime != format!("{:016x}", protected::created(process)?) {
        return Ok(None);
    }
    let raw = usize::from_str_radix(&plan.lock_handle, 16).map_err(|_| Error::Metadata)? as HANDLE;
    let mut name = [0u16; 32768];
    // SAFETY: potential inherited handle is queried before being adopted; bounded output.
    let n =
        unsafe { GetFinalPathNameByHandleW(raw, name.as_mut_ptr(), name.len() as u32, FILE_NAME_NORMALIZED) } as usize;
    if n == 0 || n >= name.len() {
        return Err(Error::Policy);
    }
    let actual = String::from_utf16(&name[..n]).map_err(|_| Error::Policy)?;
    let expected = format!(
        r"\\?\{}",
        protected::maintenance()
            .map_err(|_| Error::Storage)?
            .join("maintenance.lock")
            .display()
    );
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(Error::Policy);
    }
    Handle::new(raw).map(Some).map_err(|_| Error::Policy)
}
fn mark_boot(transaction: &str) -> Result<(), Error> {
    let mut key = ptr::null_mut();
    let descriptor = security::system_security().map_err(|_| Error::Storage)?;
    let attrs = descriptor.attrs();
    // SAFETY: fixed validated transaction child of HKLM; volatile keys disappear
    // at reboot without relying on wall-clock changes to identify a new boot.
    let code = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(&format!(r"Software\Oink\UpdateBoot\{transaction}")).as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_VOLATILE,
            KEY_READ | KEY_WRITE | KEY_WOW64_64KEY,
            &attrs,
            &mut key,
            ptr::null_mut(),
        )
    };
    if code != 0 {
        return Err(Error::Storage);
    }
    // SAFETY: RegCreateKeyEx returned an owned registry handle.
    unsafe {
        RegCloseKey(key);
    }
    Ok(())
}
fn boot_changed(plan: &Plan) -> Result<bool, Error> {
    if !plan.reboot_marker_armed {
        return Err(Error::Transaction);
    }
    let mut key = ptr::null_mut();
    // SAFETY: read-only fixed volatile marker query.
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(&format!(r"Software\Oink\UpdateBoot\{}", plan.transaction)).as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(true);
    }
    if code != 0 {
        return Err(Error::Storage);
    }
    // SAFETY: valid query-only registry handle.
    unsafe {
        RegCloseKey(key);
    }
    Ok(false)
}
fn recovery_task_xml(transaction: &str, directory: &Path) -> Result<String, Error> {
    let escape = |s: String| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    // The COM API receives UTF-16 BSTR, while the diagnostic file is UTF-8:
    // leave encoding unspecified. ServiceAccount is a registration flag, not
    // a valid XML LogonType; task::register supplies TASK_LOGON_SERVICE_ACCOUNT.
    Ok(format!(
        r#"<?xml version="1.0"?><Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><RegistrationInfo><SecurityDescriptor>O:SYG:SYD:P(A;;GA;;;SY)</SecurityDescriptor></RegistrationInfo><Triggers><BootTrigger><Enabled>true</Enabled><Delay>PT30S</Delay></BootTrigger><TimeTrigger><Repetition><Interval>PT1M</Interval></Repetition><StartBoundary>{}</StartBoundary><Enabled>true</Enabled></TimeTrigger></Triggers><Principals><Principal id="System"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT30M</ExecutionTimeLimit></Settings><Actions Context="System"><Exec><Command>{}</Command><Arguments>{}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions></Task>"#,
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| Error::Clock)?,
        escape(directory.join("worker.exe").display().to_string()),
        transaction,
        escape(directory.display().to_string())
    ))
}
fn arm_task(plan: &Plan, directory: &Path) -> Result<(), Error> {
    let task = recovery_task_xml(&plan.transaction, directory)?;
    let path = directory.join("task.xml");
    protected::write(&path, task.as_bytes())?;
    super::task::register(&plan.transaction, &task)
}
struct ScHandle(SC_HANDLE);
impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: owned SCM handle has the dedicated SCM close function.
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
fn service(access: u32) -> Result<ScHandle, Error> {
    // SAFETY: local SCM, minimum connect rights.
    let raw = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if raw.is_null() {
        return Err(Error::Storage);
    }
    let manager = ScHandle(raw);
    // SAFETY: fixed installed service name, access chosen by trusted worker code only.
    let raw = unsafe { OpenServiceW(manager.0, wide("OinkInput").as_ptr(), access | SERVICE_QUERY_CONFIG) };
    if raw.is_null() {
        return Err(Error::Storage);
    }
    let service = ScHandle(raw);
    let mut storage = [0usize; 8192 / size_of::<usize>()];
    let mut needed = 0;
    let config = storage.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    // SAFETY: aligned bounded output; all config pointers stay inside its lifetime.
    unsafe {
        check(QueryServiceConfigW(
            service.0,
            config,
            size_of_val(&storage) as u32,
            &mut needed,
        ))
        .map_err(|_| Error::Policy)?;
        let equals = |raw: *const u16, expected: &[u16]| -> bool {
            !raw.is_null()
                && expected
                    .iter()
                    .enumerate()
                    .all(|(index, value)| *raw.add(index) == *value)
        };
        let expected = wide(&format!(
            "\"{}\"",
            protected::root()
                .map_err(|_| Error::Storage)?
                .join("oink-service.exe")
                .display()
        ));
        if (*config).dwServiceType != SERVICE_WIN32_OWN_PROCESS
            || !equals((*config).lpServiceStartName, &wide("LocalSystem"))
            || !equals((*config).lpBinaryPathName, &expected)
        {
            return Err(Error::Policy);
        }
    }
    Ok(service)
}
fn wait_service(service: &ScHandle, expected: u32) -> Result<(), Error> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut status = SERVICE_STATUS_PROCESS::default();
        let mut length = 0;
        // SAFETY: owned query-status handle and correctly sized output struct.
        check(unsafe {
            QueryServiceStatusEx(
                service.0,
                SC_STATUS_PROCESS_INFO,
                ptr::addr_of_mut!(status).cast(),
                size_of_val(&status) as u32,
                &mut length,
            )
        })
        .map_err(|_| Error::Transaction)?;
        if status.dwCurrentState == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Busy);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn scm_stop() -> Result<(), Error> {
    let service = service(SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    let mut status = SERVICE_STATUS::default();
    // SAFETY: fixed service normal STOP, SCM owns helper cleanup; no process kill.
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        // SAFETY: immediate last error from ControlService.
        if unsafe { GetLastError() } != ERROR_SERVICE_NOT_ACTIVE {
            return Err(Error::Transaction);
        }
    }
    wait_service(&service, SERVICE_STOPPED)
}
fn scm_start() -> Result<(), Error> {
    let service = service(SERVICE_START | SERVICE_QUERY_STATUS)?;
    // SAFETY: fixed service starts with no arbitrary arguments.
    if unsafe { StartServiceW(service.0, 0, ptr::null()) } == 0 {
        // SAFETY: immediate last error from StartServiceW.
        if unsafe { GetLastError() } != ERROR_SERVICE_ALREADY_RUNNING {
            return Err(Error::Transaction);
        }
    }
    wait_service(&service, SERVICE_RUNNING)
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailurePolicy {
    reset: u32,
    reboot: String,
    command: String,
    actions: Vec<(i32, u32)>,
}
fn scm_failure_policy() -> Result<FailurePolicy, Error> {
    let service = service(SERVICE_QUERY_CONFIG)?;
    let mut buffer = vec![0usize; 8192 / size_of::<usize>()];
    let mut needed = 0;
    // SAFETY: aligned output storage, documented max service config size.
    check(unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            buffer.as_mut_ptr().cast(),
            size_of_val(buffer.as_slice()) as u32,
            &mut needed,
        )
    })
    .map_err(|_| Error::Storage)?;
    // SAFETY: successful config query owns all interior pointers until buffer drops.
    unsafe {
        let policy = &*buffer.as_ptr().cast::<SERVICE_FAILURE_ACTIONSW>();
        if policy.cActions > 32 {
            return Err(Error::Metadata);
        }
        let string = |raw: *const u16| -> Result<String, Error> {
            if raw.is_null() {
                return Ok(String::new());
            }
            let mut length = 0;
            while length < 4096 && *raw.add(length) != 0 {
                length += 1;
            }
            if length == 4096 {
                return Err(Error::Metadata);
            }
            String::from_utf16(std::slice::from_raw_parts(raw, length)).map_err(|_| Error::Metadata)
        };
        Ok(FailurePolicy {
            reset: policy.dwResetPeriod,
            reboot: string(policy.lpRebootMsg)?,
            command: string(policy.lpCommand)?,
            actions: if policy.cActions == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(policy.lpsaActions, policy.cActions as usize)
                    .iter()
                    .map(|a| (a.Type, a.Delay))
                    .collect()
            },
        })
    }
}
fn set_failure_policy(policy: &FailurePolicy) -> Result<(), Error> {
    if policy.actions.len() > 32 {
        return Err(Error::Metadata);
    }
    let service = service(SERVICE_CHANGE_CONFIG)?;
    let mut actions: Vec<_> = policy
        .actions
        .iter()
        .map(|(kind, delay)| SC_ACTION {
            Type: *kind,
            Delay: *delay,
        })
        .collect();
    let (mut reboot, mut command) = (wide(&policy.reboot), wide(&policy.command));
    let mut native = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: policy.reset,
        lpRebootMsg: reboot.as_mut_ptr(),
        lpCommand: command.as_mut_ptr(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: fixed service and owned backing arrays/strings; restores only saved original policy.
    check(unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            ptr::addr_of_mut!(native).cast(),
        )
    })
    .map_err(|_| Error::Storage)
}

fn cached_package_hash(product: &str) -> Result<String, Error> {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::*;
    let mut buffer = [0u16; 32768];
    let mut length = buffer.len() as u32;
    // SAFETY: exact verified ProductCode, read-only Windows Installer cache lookup.
    let code = unsafe {
        MsiGetProductInfoW(
            wide(product).as_ptr(),
            wide("LocalPackage").as_ptr(),
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if code != 0 || length as usize >= buffer.len() {
        return Err(Error::Metadata);
    }
    let path = PathBuf::from(String::from_utf16(&buffer[..length as usize]).map_err(|_| Error::Metadata)?);
    let installer = system_dir()?.parent().ok_or(Error::Storage)?.join("Installer");
    let mut file = protected::cached_msi(&path, &installer).map_err(|_| Error::Policy)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|_| Error::Storage)?;
    Ok(feed::hex(&hasher.finalize()))
}

fn product_installed(product: &str) -> bool {
    use windows_sys::Win32::System::ApplicationInstallationAndServicing::*;
    // SAFETY: only a ProductCode validated from the read-only signed MSI database.
    unsafe { MsiQueryProductStateW(wide(product).as_ptr()) != INSTALLSTATE_UNKNOWN }
}

/// Recovery entry while SCM is still alive handles a crash before task arming.
/// Once the task exists it independently covers service death and reboot.
pub(super) fn resume_pending() -> Result<(), Error> {
    let maintenance = protected::maintenance().map_err(|_| Error::Storage)?;
    let journal = Journal::parse(&protected::read(&maintenance.join("pending.json"), 65536)?)?;
    let directory = transaction_dir(&journal.transaction)?;
    let mut plan: Plan = feed::strict_json(&protected::read(&directory.join("plan.json"), feed::MAX_FEED as u64)?)?;
    if plan.worker_pid != 0 {
        // SAFETY: query-only pinned process, no terminate/start capability.
        let opened = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, plan.worker_pid) };
        if let Ok(process) = Handle::new(opened) {
            // SAFETY: owned process, nonblocking liveness query.
            if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_TIMEOUT
                && plan.worker_created_filetime == format!("{:016x}", protected::created(process.0)?)
            {
                return Ok(());
            }
        }
    }
    let roots = PackageTrust::compiled(plan.local)?;
    let lock = protected::lock()?;
    let worker = directory.join("worker.exe");
    signature::verify(&worker, roots.pins())?;
    if protected::hash(&worker)? != plan.worker_sha256 {
        return Err(Error::Signature);
    }
    plan.lock_handle = format!("{:016x}", lock.0 as usize);
    let (process, thread) = spawn_worker(&worker, &plan.transaction, &lock)?;
    // SAFETY: suspended child handles returned by CreateProcessW.
    plan.worker_pid = unsafe { GetProcessId(process.0) };
    plan.worker_created_filetime = format!("{:016x}", protected::created(process.0)?);
    write_plan(&plan)?;
    // SAFETY: plan persisted before the owned suspended worker is allowed to run.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(Error::Transaction);
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/worker_environment.rs"]
mod environment_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_package_pins_do_not_confer_manifest_authority() {
        let local = PackageTrust::Local(LocalRoots {
            signer_spki_sha256: vec!["a".repeat(64)],
        });
        assert_eq!(local.pins(), ["a".repeat(64)]);
        assert!(matches!(local.feed(), Err(Error::Policy)));
    }
    #[test]
    fn committed_candidate_inventory_survives_for_rollback_without_another_msi_inspection() {
        let fixture = tempfile::tempdir().unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(include_bytes!("notes-loopback-test.json")).unwrap();
        let decoded = STANDARD.decode(manifest["payload"].as_str().unwrap()).unwrap();
        let feed: feed::Manifest = serde_json::from_slice(&decoded).unwrap();
        let release = &feed.releases[0];
        let inventory = msi::Inventory {
            product_code: "{01234567-89AB-CDEF-0123-456789ABCDEF}".into(),
            build: release.build,
            hashes: BTreeMap::from([("oink.exe".into(), "a".repeat(64))]),
        };
        cache_verified_package(
            fixture.path(),
            release,
            &inventory,
            b"fixture MSI, never installed",
            |path, bytes| std::fs::write(path, bytes).map_err(|_| Error::Storage),
        )
        .unwrap();
        let saved: msi::Inventory =
            serde_json::from_slice(&std::fs::read(fixture.path().join("inventory.json")).unwrap()).unwrap();
        assert_eq!(saved.build, inventory.build);
        assert_eq!(saved.product_code, inventory.product_code);
        assert_eq!(saved.hashes, inventory.hashes);
    }
    #[test]
    fn completed_cleanup_reclaims_all_boot_markers_without_touching_other_registry_state() {
        let fixture = tempfile::tempdir().unwrap();
        let parent = format!(
            r"Software\OinkUpdateRetentionTest-{}",
            fixture.path().file_name().unwrap().to_str().unwrap()
        );
        let create = |suffix: &str| {
            let mut key = ptr::null_mut();
            // SAFETY: private HKCU test fixture only; no product/system keys.
            let code = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&format!(r"{parent}\{suffix}")).as_ptr(),
                    0,
                    ptr::null(),
                    REG_OPTION_VOLATILE,
                    KEY_ALL_ACCESS,
                    ptr::null(),
                    &mut key,
                    ptr::null_mut(),
                )
            };
            assert_eq!(code, 0);
            // SAFETY: owned fixture handle.
            unsafe {
                RegCloseKey(key);
            }
        };
        create("Unrelated");
        create(r"UpdateBoot\0123456789abcdef0123456789abcdef");
        create(r"UpdateBoot\fedcba9876543210fedcba9876543210");
        clear_boot_markers(HKEY_CURRENT_USER, &parent).unwrap();
        clear_boot_markers(HKEY_CURRENT_USER, &parent).unwrap();
        let mut key = ptr::null_mut();
        // SAFETY: read-only queries of this test's own registry keys.
        unsafe {
            assert_eq!(
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&format!(r"{parent}\UpdateBoot")).as_ptr(),
                    0,
                    KEY_READ,
                    &mut key
                ),
                ERROR_FILE_NOT_FOUND
            );
            assert_eq!(
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    wide(&format!(r"{parent}\Unrelated")).as_ptr(),
                    0,
                    KEY_READ,
                    &mut key
                ),
                0
            );
            RegCloseKey(key);
            assert_eq!(RegDeleteTreeW(HKEY_CURRENT_USER, wide(&parent).as_ptr()), 0);
        }
    }
    #[test]
    fn completed_cleanup_removes_download_duplicate_and_preserves_cached_rollback_and_result() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        for directory in ["downloads", "cache/31", "cache/32"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for file in [
            "downloads/candidate.msi",
            "downloads/candidate.new",
            "cache/31/package.msi",
            "cache/32/package.msi",
            "last-result.json",
            "policy.json",
            "msi-authorization.json",
            "pending.json",
        ] {
            std::fs::write(root.join(file), b"isolated fixture, never installed").unwrap();
        }
        // Exercise production deletion, replacing only its fixed-root/ACL open
        // with an ordinary fixture read. No installed files are accessed.
        remove_completed_files(root, |path| std::fs::File::open(path)).unwrap();
        assert!(!root.join("downloads/candidate.msi").exists());
        assert!(!root.join("downloads/candidate.new").exists());
        assert!(!root.join("pending.json").exists());
        assert!(!root.join("msi-authorization.json").exists());
        for file in [
            "cache/31/package.msi",
            "cache/32/package.msi",
            "last-result.json",
            "policy.json",
        ] {
            assert!(root.join(file).exists(), "{file}");
        }
        remove_completed_files(root, |path| std::fs::File::open(path)).unwrap();
    }
    #[test]
    fn cleanup_keeps_pending_recovery_if_the_download_cannot_be_removed() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        std::fs::create_dir(root.join("downloads")).unwrap();
        std::fs::write(root.join("downloads/candidate.msi"), b"download").unwrap();
        std::fs::write(root.join("pending.json"), b"terminal recovery").unwrap();
        assert_eq!(
            remove_completed_files(root, |_| Err(std::io::ErrorKind::PermissionDenied.into())),
            Err(Error::Storage)
        );
        assert!(root.join("pending.json").exists());
    }
    #[test]
    fn recovery_task_definition_is_accepted_by_native_scheduler() {
        let directory = Path::new(r"C:\Program Files\Oink\maintenance\transactions\0123456789abcdef0123456789abcdef");
        let xml = recovery_task_xml("0123456789abcdef0123456789abcdef", directory).unwrap();
        super::super::task::validate_definition(&xml).unwrap();
    }
    #[test]
    fn same_product_recovery_never_uninstalls_user_data() {
        assert_eq!(restore_package("old", "old", true), Ok(RestorePackage::RepairInPlace));
        assert_eq!(restore_package("old", "new", true), Ok(RestorePackage::ReplaceProduct));
        assert_eq!(restore_package("old", "old", false), Err(Error::RepairRequired));
        assert_eq!(restore_package("old", "new", false), Err(Error::RepairRequired));
    }
    #[test]
    fn same_product_repair_preserves_the_registered_source_filename() {
        let directory = Path::new(r"C:\Program Files\Oink\maintenance\transactions\test");
        for name in ["baseline.msi", "Oink installer.msi", "Oink-更新.MSI"] {
            assert_eq!(
                repair_source_path(directory, name).unwrap(),
                directory.join("repair-source").join(name)
            );
        }
        for name in [
            "",
            "../baseline.msi",
            r"..\baseline.msi",
            r"C:\baseline.msi",
            "file.msi:stream",
            "file.msi.",
            "file.msi ",
            "file?.msi",
            "NUL.msi",
            "COM1.msi",
            "NUL .msi",
            "COM¹.msi",
            "file.exe",
            "a\0.msi",
        ] {
            assert_eq!(repair_source_path(directory, name), Err(Error::Metadata), "{name:?}");
        }
    }
    #[test]
    fn installer_has_only_quiet_no_restart_and_protected_verbose_log_arguments() {
        let package = Path::new(r"C:\Program Files\Oink\maintenance\transactions\test\candidate.msi");
        let log = package.with_file_name("install.log");
        let arguments = msi_arguments("/i", package.as_os_str(), &log);
        assert_eq!(
            arguments,
            [
                "/i".as_ref(),
                package.as_os_str(),
                "/qn".as_ref(),
                "/norestart".as_ref(),
                "/L*v!".as_ref(),
                log.as_os_str()
            ]
        );
        assert_eq!(outcome(0), Ok(InstallOutcome::Installed));
        assert_eq!(outcome(3010), Ok(InstallOutcome::RebootRequired));
        assert_eq!(outcome(1618), Ok(InstallOutcome::Busy));
        assert_eq!(outcome(1603), Err(Error::Transaction));
    }
}
