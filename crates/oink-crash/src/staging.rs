//! Owned staging: two files, 32 MiB each. Bounds apply BEFORE every write.
use oink_telemetry::{capture::Manifest, *};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub generation: u64,
    pub collected_at: u64,
    pub build: BuildContext,
    pub component: Component,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Projected {
    lease: Lease,
    crash: SafeCrash,
}
#[derive(Debug)]
pub struct Writer {
    file: File,
    path: PathBuf,
    lease_path: PathBuf,
    high_water: u64,
    failed: bool,
}
impl Writer {
    pub fn reserve(root: &Path, lease: Lease) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(|_| Error::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(|_| Error::Io)?;
        }
        // Cleanup only the two owned slots. Never discover/delete arbitrary files.
        for slot in 0..2 {
            let path = root.join(format!("{slot}.dmp"));
            let lease_path = root.join(format!("{slot}.lease"));
            if let Some(old) = read_lease(&lease_path)
                && now().saturating_sub(old.collected_at) >= 86400
            {
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(&lease_path);
            }
            let file = match OpenOptions::new().create_new(true).write(true).read(true).open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            oink_telemetry::store::atomic_write(&lease_path, &serde_json::to_vec(&lease).map_err(|_| Error::Invalid)?)?;
            return Ok(Self {
                file,
                path,
                lease_path,
                high_water: 0,
                failed: false,
            });
        }
        Err(Error::Limit)
    }
    pub fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        let end = offset.checked_add(bytes.len() as u64).ok_or(Error::Limit)?;
        if self.failed || end > crate::dump::MAX_DUMP {
            self.failed = true;
            return Err(Error::Limit);
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(bytes))
            .map_err(|_| {
                self.failed = true;
                Error::Io
            })?;
        self.high_water = self.high_water.max(end);
        Ok(())
    }
    pub fn project(self, manifest: &Manifest) -> Result<()> {
        let Self {
            mut file,
            path,
            lease_path,
            failed,
            ..
        } = self;
        let projected = (|| {
            if failed {
                return Err(Error::Invalid);
            }
            file.flush().map_err(|_| Error::Io)?;
            let lease = read_lease(&lease_path).ok_or(Error::Invalid)?;
            file.seek(SeekFrom::Start(0)).map_err(|_| Error::Io)?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(crate::dump::MAX_DUMP + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::Io)?;
            let crash = crate::dump::extract(&bytes, manifest, lease.build, lease.component).ok_or(Error::Invalid)?;
            Ok(Projected { lease, crash })
        })();
        // Delete raw material whether projection succeeds or fails. The only
        // persistent output is a strict numeric typed record; no raw-upload API.
        // Windows cannot delete a file held without delete sharing. Close the
        // capture handle before deletion, including every projection error path.
        drop(file);
        std::fs::remove_file(&path).map_err(|_| Error::Io)?;
        std::fs::remove_file(&lease_path).map_err(|_| Error::Io)?;
        let bytes = serde_json::to_vec(&projected?).map_err(|_| Error::Invalid)?;
        if bytes.len() > 32768 {
            return Err(Error::Limit);
        }
        oink_telemetry::store::atomic_write(&path.with_extension("safe"), &bytes)
    }
}
fn read_lease(path: &Path) -> Option<Lease> {
    let mut b = Vec::new();
    File::open(path).ok()?.take(4097).read_to_end(&mut b).ok()?;
    if b.len() > 4096 {
        return None;
    }
    serde_json::from_slice(&b).ok()
}
pub fn recover(root: &Path, consent: ReportConsent, manifest: &Manifest) -> Vec<(SafeCrash, u64)> {
    let mut out = Vec::new();
    for slot in 0..2 {
        let dump = root.join(format!("{slot}.dmp"));
        let lease_path = root.join(format!("{slot}.lease"));
        if dump.exists() {
            if let Some(lease) = read_lease(&lease_path).filter(|l| {
                consent.granted() && l.generation == consent.generation && now().saturating_sub(l.collected_at) < 86400
            }) {
                if let Ok(file) = OpenOptions::new().read(true).write(true).open(&dump) {
                    let _ = Writer {
                        file,
                        path: dump.clone(),
                        lease_path: lease_path.clone(),
                        high_water: 0,
                        failed: false,
                    }
                    .project(manifest);
                }
                let _ = lease;
            }
            let _ = std::fs::remove_file(&dump);
            let _ = std::fs::remove_file(&lease_path);
        }
        let safe = root.join(format!("{slot}.safe"));
        if let Ok(file) = File::open(&safe) {
            let mut bytes = Vec::new();
            let _ = file.take(32769).read_to_end(&mut bytes);
            if bytes.len() <= 32768
                && let Ok(mut projected) = serde_json::from_slice::<Projected>(&bytes)
                && consent.granted()
                && projected.lease.generation == consent.generation
                && now().saturating_sub(projected.lease.collected_at) < 7 * 86400
                && manifest.project(&mut projected.crash).is_ok()
            {
                out.push((projected.crash, projected.lease.collected_at));
            }
            let _ = std::fs::remove_file(safe);
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_projection_deletes_raw_staging_but_never_other_storage_classes() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "manual.zip",
            "emergency.zip",
            "0.dseg",
            "oink.2026-01-01.log",
            "checkpoint.json",
        ] {
            std::fs::write(dir.path().join(name), b"keep").unwrap();
        }
        let build = BuildContext {
            app_version: AppVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 1,
            },
            os_version_class: OsVersionClass::Unknown,
        };
        let lease = Lease {
            generation: 1,
            collected_at: now(),
            build,
            component: Component::Agent,
        };
        let mut writer = Writer::reserve(dir.path(), lease).unwrap();
        writer.write_at(0, b"private invalid dump").unwrap();
        assert_eq!(
            writer.project(&Manifest {
                version: build.app_version,
                images: vec![]
            }),
            Err(Error::Invalid)
        );
        assert!(!dir.path().join("0.dmp").exists());
        assert!(!dir.path().join("0.lease").exists());
        assert!(!dir.path().join("0.safe").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 5);
    }
    #[test]
    fn write_limit_is_enforced_before_disk_and_slots_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let lease = Lease {
            generation: 1,
            collected_at: now(),
            build: BuildContext {
                app_version: AppVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    build: 1,
                },
                os_version_class: OsVersionClass::Unknown,
            },
            component: Component::Agent,
        };
        let mut first = Writer::reserve(dir.path(), lease).unwrap();
        let _second = Writer::reserve(dir.path(), lease).unwrap();
        assert_eq!(Writer::reserve(dir.path(), lease).unwrap_err(), Error::Limit);
        assert_eq!(first.write_at(crate::dump::MAX_DUMP, &[1]), Err(Error::Limit));
        assert_eq!(first.file.metadata().unwrap().len(), 0);
        assert_eq!(first.write_at(0, &[1]), Err(Error::Limit));
    }
}
