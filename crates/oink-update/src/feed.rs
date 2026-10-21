//! Verify the exact signed bytes before interpreting any selection metadata.
use crate::{Error, PRODUCT, SCHEMA};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

pub const MAX_FEED: usize = 1024 * 1024;
pub const MAX_PACKAGE: u64 = 512 * 1024 * 1024;
pub const MAX_UNPACKED: u64 = 1024 * 1024 * 1024;
pub const MAX_NOTES: usize = 16 * 1024;

// Preserve the published feed schema while accepting only stable metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    #[default]
    Stable,
}

pub fn manifest_url() -> String {
    format!(
        "{}/windows/v1/{}/stable/manifest.json",
        feed_origin(),
        crate::WINDOWS_EPOCH
    )
}
fn feed_origin() -> &'static str {
    #[cfg(feature = "test-feed")]
    {
        concat!(env!("OINK_WINDOWS_UPDATE_TEST_FEED"), "/oink-update-qualification")
    }
    #[cfg(not(feature = "test-feed"))]
    {
        "https://updates.oink.hachiwii.com"
    }
}

/// Extensible plain-text notes. Language matching is case-insensitive RFC-style
/// lookup: exact tag, progressively shorter parent tags, then English.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ReleaseNotes(BTreeMap<String, String>);

pub const MAX_LANGUAGES: usize = 32;

impl<'de> Deserialize<'de> for ReleaseNotes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Notes;
        impl<'de> Visitor<'de> for Notes {
            type Value = ReleaseNotes;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a validated release-note language map")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut input: M) -> Result<Self::Value, M::Error> {
                let mut map = BTreeMap::new();
                while let Some((tag, text)) = input.next_entry::<String, String>()? {
                    if map.insert(tag, text).is_some() || map.len() > MAX_LANGUAGES {
                        return Err(de::Error::custom("duplicate or excessive release-note languages"));
                    }
                }
                let notes = ReleaseNotes(map);
                notes.validate().map_err(de::Error::custom)?;
                Ok(notes)
            }
        }
        deserializer.deserialize_map(Notes)
    }
}

impl ReleaseNotes {
    pub fn validate(&self) -> Result<(), Error> {
        if !(1..=MAX_LANGUAGES).contains(&self.0.len()) || !self.0.contains_key("en") {
            return Err(Error::Metadata);
        }
        let mut seen = BTreeSet::new();
        for (tag, text) in &self.0 {
            let mut parts = tag.split('-');
            let language = parts.next().unwrap_or_default();
            if !(2..=3).contains(&language.len())
                || !language.bytes().all(|b| b.is_ascii_alphabetic())
                || !parts.all(|part| (2..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric()))
                || !seen.insert(tag.to_ascii_lowercase())
                || text.trim().is_empty()
                || text.len() > MAX_NOTES
                || text.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err(Error::Metadata);
            }
        }
        Ok(())
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_FEED {
            return Err(Error::Limit);
        }
        strict_json(bytes)
    }
    pub fn for_language(&self, language: &str) -> &str {
        let mut language = language.replace('_', "-");
        loop {
            if let Some((_, text)) = self.0.iter().find(|(tag, _)| tag.eq_ignore_ascii_case(&language)) {
                return text;
            }
            match language.rfind('-') {
                Some(i) => language.truncate(i),
                None => break,
            }
        }
        &self.0["en"]
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    envelope_schema: u32,
    payload: String,
    signatures: Vec<EnvelopeSignature>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvelopeSignature {
    key_id: String,
    ed25519: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub product: String,
    pub key_epoch: String,
    pub channel: Channel,
    pub sequence: u64,
    pub generated_at: String,
    pub expires_at: String,
    pub releases: Vec<Release>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Release {
    pub build: u64,
    pub version: String,
    pub tag: String,
    pub source_tree_sha256: String,
    pub architecture: String,
    pub minimum_windows_build: u32,
    pub protocol_baseline: u32,
    pub minimum_updater_schema: u32,
    #[serde(default)]
    pub channel: Channel,
    pub severity: Severity,
    pub notes: ReleaseNotes,
    pub package: Package,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Normal,
    Critical,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Package {
    pub url: String,
    pub length: u64,
    pub sha256: String,
    pub key_id: String,
    pub ed25519: String,
}

/// Constructed only from compiled release configuration by the service/executor.
#[derive(Debug)]
pub struct Trust<'a> {
    pub epoch: &'a str,
    pub key: &'a [u8; 32],
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Watermark {
    pub sequence: u64,
    pub payload_sha256: String,
    pub last_verified_unix: i64,
}
#[derive(Debug)]
pub struct Selection<'a> {
    pub windows_build: u32,
    pub architecture: &'a str,
    pub installed_build: u64,
    pub successful_build: u64,
    pub quarantine: &'a BTreeSet<u64>,
}
/// Only obtained after signature, freshness, identity and replay validation.
#[derive(Clone, Debug)]
pub struct VerifiedManifest {
    manifest: Manifest,
    watermark: Watermark,
    expires_unix: i64,
}
impl VerifiedManifest {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn watermark(&self) -> &Watermark {
        &self.watermark
    }
    pub fn ensure_fresh(&self, now: i64) -> Result<(), Error> {
        if now < self.watermark.last_verified_unix - 300 {
            return Err(Error::Clock);
        }
        if now >= self.expires_unix {
            return Err(Error::ExpiredFeed);
        }
        Ok(())
    }
    pub fn select(&self, selection: &Selection<'_>) -> Result<Option<&Release>, Error> {
        Ok(self
            .manifest
            .releases
            .iter()
            .filter(|r| {
                r.build > selection.installed_build.max(selection.successful_build)
                    && !selection.quarantine.contains(&r.build)
                    && r.architecture == selection.architecture
                    && r.minimum_windows_build <= selection.windows_build
                    && r.minimum_updater_schema <= SCHEMA
                    && r.protocol_baseline == 1
            })
            .max_by_key(|r| r.build))
    }
}

pub fn verify_manifest(bytes: &[u8], trust: &Trust<'_>, seen: &Watermark, now: i64) -> Result<VerifiedManifest, Error> {
    if bytes.len() > MAX_FEED {
        return Err(Error::Limit);
    }
    let envelope: Envelope = strict_json(bytes)?;
    if envelope.envelope_schema != SCHEMA || envelope.signatures.is_empty() || envelope.signatures.len() > 4 {
        return Err(Error::Metadata);
    }
    let payload = STANDARD.decode(&envelope.payload).map_err(|_| Error::Metadata)?;
    let mut ids = BTreeSet::new();
    for signature in &envelope.signatures {
        if !ids.insert(&signature.key_id) {
            return Err(Error::Metadata);
        }
    }
    let signature = envelope
        .signatures
        .iter()
        .find(|s| s.key_id == trust.epoch)
        .ok_or(Error::Signature)?;
    verify_signature(&payload, &signature.ed25519, trust.key)?;
    let manifest: Manifest = strict_json(&payload)?;
    if manifest.schema != SCHEMA
        || manifest.product != PRODUCT
        || manifest.key_epoch != trust.epoch
        || manifest.sequence == 0
    {
        return Err(Error::Policy);
    }
    let generated = timestamp(&manifest.generated_at)?;
    let expires = timestamp(&manifest.expires_at)?;
    if expires <= generated || expires - generated > 30 * 86400 {
        return Err(Error::Metadata);
    }
    if now < generated - 300 || now < seen.last_verified_unix - 300 {
        return Err(Error::Clock);
    }
    if now >= expires {
        return Err(Error::ExpiredFeed);
    }
    let digest = hex(&Sha256::digest(&payload));
    if manifest.sequence < seen.sequence || (manifest.sequence == seen.sequence && digest != seen.payload_sha256) {
        return Err(Error::Replay);
    }
    if manifest.releases.len() > 32 {
        return Err(Error::Limit);
    }
    let mut builds = BTreeSet::new();
    for release in &manifest.releases {
        if release.build == 0
            || !builds.insert(release.build)
            || release.version.is_empty()
            || release.version.len() > 128
            || !is_hex(&release.source_tree_sha256, 64)
            || release.package.key_id != trust.epoch
            || release.package.length == 0
            || release.package.length > MAX_PACKAGE
            || !is_hex(&release.package.sha256, 64)
            || STANDARD
                .decode(&release.package.ed25519)
                .map_or(true, |s| s.len() != 64)
        {
            return Err(Error::Metadata);
        }
        validate_package_url(&release.package.url)?;
    }
    let watermark = Watermark {
        sequence: manifest.sequence,
        payload_sha256: digest,
        last_verified_unix: now.max(seen.last_verified_unix),
    };
    Ok(VerifiedManifest {
        manifest,
        watermark,
        expires_unix: expires,
    })
}
pub fn verify_package(bytes: &[u8], package: &Package, trust: &Trust<'_>) -> Result<(), Error> {
    if package.key_id != trust.epoch || bytes.len() as u64 != package.length || package.length > MAX_PACKAGE {
        return Err(Error::Metadata);
    }
    if hex(&Sha256::digest(bytes)) != package.sha256 {
        return Err(Error::Signature);
    }
    verify_signature(bytes, &package.ed25519, trust.key)
}
/// Product build is the primary monotonic counter; a higher build must also
/// retain or increase the human version. Build metadata has no precedence.
pub fn verify_version(candidate: &str, installed: &str) -> Result<(), Error> {
    let candidate = semver::Version::parse(candidate).map_err(|_| Error::Metadata)?;
    let installed = semver::Version::parse(installed).map_err(|_| Error::Metadata)?;
    if candidate.cmp_precedence(&installed).is_lt() {
        Err(Error::Replay)
    } else {
        Ok(())
    }
}
pub fn verify_signature(bytes: &[u8], signature: &str, key: &[u8; 32]) -> Result<(), Error> {
    let decoded = STANDARD.decode(signature).map_err(|_| Error::Signature)?;
    let signature = Signature::from_slice(&decoded).map_err(|_| Error::Signature)?;
    VerifyingKey::from_bytes(key)
        .map_err(|_| Error::Signature)?
        .verify_strict(bytes, &signature)
        .map_err(|_| Error::Signature)
}
pub fn timestamp(s: &str) -> Result<i64, Error> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map(|t| t.unix_timestamp())
        .map_err(|_| Error::Metadata)
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn https_url(s: &str) -> Result<Url, Error> {
    let url = Url::parse(s).map_err(|_| Error::Url)?;
    // Inspect raw authority too: URL normalization erases empty userinfo (@).
    let authority = s
        .get(8..)
        .ok_or(Error::Url)?
        .split(['/', '\\', '?', '#'])
        .next()
        .ok_or(Error::Url)?;
    if url.scheme() != "https"
        || !s.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || authority.contains('@')
    {
        return Err(Error::Url);
    }
    Ok(url)
}
/// Package hosts/paths are selected by the signed manifest, not compiled pins.
/// The exact downloaded bytes must still pass verify_package before staging/use.
pub fn validate_package_url(s: &str) -> Result<(), Error> {
    #[cfg(feature = "test-feed")]
    {
        let prefix = format!("{}/packages/", feed_origin());
        if let Some(name) = s.strip_prefix(&prefix)
            && name.ends_with(".msi")
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Ok(());
        }
    }
    https_url(s).map(|_| ())
}
/// Every redirect hop must retain HTTPS without userinfo, to any host.
pub fn validate_redirect(s: &str, hop: usize) -> Result<(), Error> {
    https_url(s)?;
    if hop > 5 {
        return Err(Error::Url);
    }
    Ok(())
}

/// serde_json normally accepts duplicate map keys. Reject duplicates recursively
/// before typed parsing, including duplicates inside future nested objects.
pub fn strict_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    struct Unique;
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(UniqueVisitor)
        }
    }
    struct UniqueVisitor;
    impl<'de> Visitor<'de> for UniqueVisitor {
        type Value = Unique;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("JSON with unique keys")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Unique, M::Error> {
            let mut keys = BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(de::Error::custom("duplicate key"));
                }
                map.next_value::<Unique>()?;
            }
            Ok(Unique)
        }
        fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Unique, S::Error> {
            while seq.next_element::<Unique>()?.is_some() {}
            Ok(Unique)
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_none<E: de::Error>(self) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
            Ok(Unique)
        }
    }
    serde_json::from_slice::<Unique>(bytes).map_err(|_| Error::Metadata)?;
    serde_json::from_slice(bytes).map_err(|_| Error::Metadata)
}
