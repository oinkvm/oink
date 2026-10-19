//! Fixed component inventory and Windows path validation. Archive extraction must
//! use a newly created private directory and no-follow handles on Windows.
use crate::{
    Error, PRODUCT,
    feed::{MAX_UNPACKED, hex, strict_json},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Files {
    pub schema: u32,
    pub product: String,
    pub build: u64,
    pub source_tree_sha256: String,
    pub files: Vec<Component>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Component {
    pub path: String,
    pub length: u64,
    pub sha256: String,
    pub origin: Origin,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Product,
    Dependency,
}
impl Files {
    pub fn parse(
        bytes: &[u8],
        build: u64,
        source_tree_sha256: &str,
        allowed: &BTreeSet<String>,
    ) -> Result<Self, Error> {
        if bytes.len() > 1024 * 1024 {
            return Err(Error::Limit);
        }
        let result: Self = strict_json(bytes)?;
        if result.schema != 1
            || result.product != PRODUCT
            || result.build != build
            || result.source_tree_sha256 != source_tree_sha256
            || result.files.is_empty()
            || result.files.len() > 4096
        {
            return Err(Error::Metadata);
        }
        let mut paths = BTreeSet::new();
        let mut length = 0u64;
        for f in &result.files {
            validate_path(&f.path)?;
            if !allowed.contains(&f.path)
                || !paths.insert(f.path.to_ascii_lowercase())
                || f.sha256.len() != 64
                || !f
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::Metadata);
            }
            length = length.checked_add(f.length).ok_or(Error::Limit)?;
            if length > MAX_UNPACKED {
                return Err(Error::Limit);
            }
        }
        if paths.len() != allowed.len() {
            return Err(Error::Metadata);
        }
        Ok(result)
    }
}
impl Component {
    pub fn verify(&self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() as u64 != self.length || hex(&Sha256::digest(bytes)) != self.sha256 {
            return Err(Error::Signature);
        }
        Ok(())
    }
}
pub fn validate_path(path: &str) -> Result<(), Error> {
    // Product resource names are deliberately ASCII. This excludes Unicode aliasing
    // and normalization collisions rather than guessing NTFS normalization rules.
    if path.is_empty() || path.len() > 240 || !path.is_ascii() || path.contains('\\') || path.contains(':') {
        return Err(Error::Metadata);
    }
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with('.')
            || part.ends_with(' ')
            || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(Error::Metadata);
        }
        let stem = part.split('.').next().unwrap().to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(Error::Metadata);
        }
    }
    if path == "files.json" || path.starts_with("maintenance/") || path == "installed.json" {
        return Err(Error::Metadata);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_windows_path_aliases() {
        for path in [
            "../a",
            "/a",
            "C:/a",
            "a:stream",
            "a\\b",
            "CON.txt",
            "com1",
            "a/../b",
            "a.",
            "a ",
            "a//b",
            "é.exe",
            "maintenance/policy.json",
            "installed.json",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
        assert!(validate_path("resources/en-US.json").is_ok());
    }
}
