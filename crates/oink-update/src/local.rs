//! Metadata is never authority: Apple signing or an elevated Windows caller is required.
use crate::{Error, PRODUCT, feed, policy::Candidate};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub product: String,
    pub version: String,
    pub build: u64,
    pub architecture: String,
    pub source_tree_sha256: String,
    pub minimum_windows_build: u32,
    pub length: u64,
    pub sha256: String,
    pub notes: Option<feed::ReleaseNotes>,
}
impl Package {
    pub fn validate(&self, version: &str, build: u64, architecture: &str) -> Result<(), Error> {
        self.validate_inner(version, build, architecture, true)
    }
    /// Local DMGs have already passed codesign verification in the macOS stager.
    /// Their unused package hash is omitted (user decision, 2026-10-29). Every
    /// Windows/local MSI and signed feed package retains its hash validation.
    #[cfg(target_os = "macos")]
    pub fn validate_codesigned_dmg(&self, version: &str, build: u64, architecture: &str) -> Result<(), Error> {
        self.validate_inner(version, build, architecture, false)
    }
    fn validate_inner(&self, version: &str, build: u64, architecture: &str, require_hash: bool) -> Result<(), Error> {
        if self.product != PRODUCT {
            return Err(Error::WrongProduct);
        }
        if self.architecture != architecture {
            return Err(Error::WrongArchitecture);
        }
        if self.build == 0
            || self.build >= (1 << 31)
            || self.length == 0
            || self.length > feed::MAX_PACKAGE
            || ((require_hash || !self.sha256.is_empty()) && !hex(&self.sha256))
            || !hex(&self.source_tree_sha256)
        {
            return Err(Error::CorruptPackage);
        }
        // In-house labels use build numbers; order their base release and numeric build.
        let base = |text: &str| -> Result<semver::Version, Error> {
            let canonical = if let Some((base, suffix)) = text.rsplit_once("-inhouse.") {
                let build_label = !suffix.starts_with('0')
                    && suffix.bytes().all(|b| b.is_ascii_digit())
                    && suffix.parse::<u64>().is_ok_and(|n| n > 0 && n < (1 << 31));
                // Installed in-house builds 41/42 have eight-digit commit labels,
                // including numeric hashes with leading zeroes that are not SemVer.
                let commit_label =
                    suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if !build_label && !commit_label {
                    return Err(Error::CorruptPackage);
                }
                base
            } else {
                text
            };
            let mut v = semver::Version::parse(canonical).map_err(|_| Error::CorruptPackage)?;
            v.pre = semver::Prerelease::EMPTY;
            v.build = semver::BuildMetadata::EMPTY;
            Ok(v)
        };
        if self.build < build || base(&self.version)? < base(version)? {
            return Err(Error::Downgrade);
        }
        Ok(())
    }
    pub fn candidate(&self) -> Candidate {
        Candidate {
            local_package: true,
            build: self.build,
            version: self.version.clone(),
            size: self.length,
            published_at: None,
            severity: feed::Severity::Normal,
            notes: self.notes.clone(),
        }
    }
    pub fn release(&self) -> feed::Release {
        feed::Release {
            build: self.build,
            version: self.version.clone(),
            tag: String::new(),
            source_tree_sha256: self.source_tree_sha256.clone(),
            architecture: self.architecture.clone(),
            minimum_windows_build: self.minimum_windows_build,
            protocol_baseline: 1,
            minimum_updater_schema: 1,
            channel: feed::Channel::Stable,
            severity: feed::Severity::Normal,
            notes: self.notes.clone().unwrap_or_else(|| {
                feed::ReleaseNotes::from_json(br#"{"en":"Local package"}"#).expect("static transaction notes")
            }),
            package: feed::Package {
                url: String::new(),
                length: self.length,
                sha256: self.sha256.clone(),
                key_id: String::new(),
                ed25519: String::new(),
            },
        }
    }
}
pub const MAX_EMBEDDED_NOTES: usize = 128 * 1024;
/// The app signature or approved MSI hash authenticates this file. Feed policy
/// stays separate; generated in-house notes may contain only the English fallback.
pub fn embedded_notes(bytes: &[u8]) -> Result<feed::ReleaseNotes, Error> {
    #[derive(Deserialize)]
    struct Embedded {
        schema: u32,
        notes: feed::ReleaseNotes,
    }
    if bytes.len() > MAX_EMBEDDED_NOTES {
        return Err(Error::CorruptPackage);
    }
    let embedded: Embedded = feed::strict_json(bytes).map_err(|_| Error::CorruptPackage)?;
    if embedded.schema != 1 {
        return Err(Error::CorruptPackage);
    }
    Ok(embedded.notes)
}
pub fn hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn package() -> Package {
        Package {
            product: PRODUCT.into(),
            version: "0.2.1-local".into(),
            build: 30,
            architecture: "x64".into(),
            source_tree_sha256: "a".repeat(64),
            minimum_windows_build: 19045,
            length: 123,
            sha256: "b".repeat(64),
            notes: None,
        }
    }
    #[test]
    fn embedded_notes_validate_languages_and_older_packages_have_no_list() {
        let notes = embedded_notes(br#"{"schema":1,"kind":"inhouse","previous":null,"notes":{"en":"A fix"}}"#).unwrap();
        assert_eq!(notes.for_language("ja"), "A fix");
        let mut package = package();
        assert!(package.candidate().notes.is_none());
        package.notes = Some(notes);
        assert_eq!(package.candidate().notes.unwrap().for_language("en"), "A fix");
        for bad in [
            br#"{"schema":2,"notes":{"en":"A fix"}}"#.as_slice(),
            br#"{"schema":1,"notes":{}}"#,
            br#"{"schema":1,"notes":{"en":""}}"#,
        ] {
            assert_eq!(embedded_notes(bad), Err(Error::CorruptPackage));
        }
    }
    #[test]
    fn allows_same_or_newer_build_with_local_or_public_labels() {
        for build in [29, 30] {
            assert_eq!(package().validate("0.2.1", build, "x64"), Ok(()));
        }
        assert_eq!(package().release().package.sha256, package().sha256);
    }
    #[test]
    fn inhouse_build_labels_upgrade_installed_commit_labels_and_order_by_build() {
        let mut p = package();
        p.version = "0.3.0-inhouse.44".into();
        p.build = 44;
        for (version, build) in [
            ("0.3.0-inhouse.0f806215", 41),
            ("0.3.0-inhouse.00000012", 42),
            ("0.3.0-inhouse.43", 43),
            ("0.3.0-inhouse.44", 44),
        ] {
            assert_eq!(p.validate(version, build, "x64"), Ok(()));
        }
        assert_eq!(p.candidate().version, "0.3.0-inhouse.44");
        assert_eq!(p.validate("0.3.0-inhouse.45", 45, "x64"), Err(Error::Downgrade));
        assert_eq!(p.validate("0.4.0-inhouse.43", 43, "x64"), Err(Error::Downgrade));
        for suffix in ["0", "044", "-44", "44a", "2147483648"] {
            p.version = format!("0.3.0-inhouse.{suffix}");
            assert_eq!(p.validate("0.3.0", 42, "x64"), Err(Error::CorruptPackage));
        }
    }
    #[test]
    fn inhouse_numeric_commit_labels_do_not_need_semver_normalization() {
        let mut p = package();
        p.version = "0.2.1-inhouse.00000012".into();
        assert_eq!(p.validate("0.2.1-inhouse.00000011", 30, "x64"), Ok(()));
        assert_eq!(p.candidate().version, "0.2.1-inhouse.00000012");
        p.version = "0.2.1-inhouse.not-a-sha".into();
        assert_eq!(p.validate("0.2.1", 30, "x64"), Err(Error::CorruptPackage));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn only_codesigned_local_dmg_validation_allows_an_absent_package_hash() {
        let mut p = package();
        p.sha256.clear();
        assert_eq!(p.validate("0.2.1", 29, "x64"), Err(Error::CorruptPackage));
        assert_eq!(p.validate_codesigned_dmg("0.2.1", 29, "x64"), Ok(()));
        p.sha256 = "bad".into();
        assert_eq!(
            p.validate_codesigned_dmg("0.2.1", 29, "x64"),
            Err(Error::CorruptPackage)
        );
        p.sha256.clear();
        p.source_tree_sha256.clear();
        assert_eq!(
            p.validate_codesigned_dmg("0.2.1", 29, "x64"),
            Err(Error::CorruptPackage)
        );
    }
    #[test]
    fn rejects_downgrade_product_architecture_and_corruption_specifically() {
        assert_eq!(package().validate("0.2.1", 31, "x64"), Err(Error::Downgrade));
        assert_eq!(package().validate("0.2.2", 29, "x64"), Err(Error::Downgrade));
        assert_eq!(package().validate("0.2.1", 29, "arm64"), Err(Error::WrongArchitecture));
        let mut p = package();
        p.product = "other".into();
        assert_eq!(p.validate("0.2.1", 29, "x64"), Err(Error::WrongProduct));
        p = package();
        p.sha256 = "bad".into();
        assert_eq!(p.validate("0.2.1", 29, "x64"), Err(Error::CorruptPackage));
    }
}
