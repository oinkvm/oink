//! Verify the exact signed bytes before interpreting any selection metadata.
use crate::policy::Channel;
use crate::{Error, PRODUCT, SCHEMA};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

pub const MAX_FEED: usize = 1024 * 1024;
pub const MAX_PACKAGE: u64 = 512 * 1024 * 1024;
pub const MAX_UNPACKED: u64 = 1024 * 1024 * 1024;
pub const MAX_NOTES: usize = 16 * 1024;

/// Both translations travel together; consumers choose by system language and
/// fall back to English. Text is never interpreted as HTML or Markdown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseNotes {
    #[serde(rename = "zh-Hans")]
    pub zh_hans: String,
    pub en: String,
}
impl ReleaseNotes {
    pub fn validate(&self) -> Result<(), Error> {
        for text in [&self.zh_hans, &self.en] {
            if text.trim().is_empty()
                || text.len() > MAX_NOTES
                || text.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err(Error::Metadata);
            }
        }
        Ok(())
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        // JSON may escape every text byte as six ASCII bytes. Bound the parse
        // itself before checking each decoded UTF-8 translation's own limit.
        if bytes.len() > 12 * MAX_NOTES + 64 {
            return Err(Error::Limit);
        }
        let notes: Self = strict_json(bytes)?;
        notes.validate()?;
        Ok(notes)
    }

    pub fn for_language(&self, language: &str) -> &str {
        let language = language.replace('_', "-").to_ascii_lowercase();
        if language == "zh"
            || language == "zh-hans"
            || language.starts_with("zh-hans-")
            || matches!(language.as_str(), "zh-cn" | "zh-sg")
        {
            &self.zh_hans
        } else {
            &self.en
        }
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
    pub repository: &'a str,
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
    pub channel: Channel,
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
        if selection.channel != self.manifest.channel {
            return Err(Error::Policy);
        }
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
                    && (selection.channel == Channel::Beta || r.channel == Channel::Stable)
            })
            .max_by_key(|r| r.build))
    }
}

pub fn verify_manifest(
    bytes: &[u8],
    trust: &Trust<'_>,
    channel: Channel,
    seen: &Watermark,
    now: i64,
) -> Result<VerifiedManifest, Error> {
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
        || manifest.channel != channel
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
            || release.notes.validate().is_err()
            || !is_hex(&release.source_tree_sha256, 64)
            || release.package.key_id != trust.epoch
            || release.package.length == 0
            || release.package.length > MAX_PACKAGE
            || !is_hex(&release.package.sha256, 64)
            || STANDARD
                .decode(&release.package.ed25519)
                .map_or(true, |s| s.len() != 64)
            || (channel == Channel::Stable && release.channel != Channel::Stable)
        {
            return Err(Error::Metadata);
        }
        validate_asset_url(&release.package.url, trust.repository, &release.tag)?;
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
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Url);
    }
    Ok(url)
}
pub fn validate_asset_url(s: &str, repository: &str, tag: &str) -> Result<(), Error> {
    let url = https_url(s)?;
    let safe = |s: &str| {
        !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    let repo: Vec<_> = repository.split('/').collect();
    let segments: Vec<_> = url.path().split('/').skip(1).collect();
    if repo.len() != 2 || !repo.iter().all(|s| safe(s)) || !safe(tag) || url.host_str() != Some("github.com") || url.query().is_some()
        || segments.len() != 6 || segments[..2] != repo[..] || segments[2..4] != ["releases", "download"] || segments[4] != tag || !safe(segments[5])
        // Reject normalization tricks before parsing, including escaped dot segments.
        || s.contains('%') || s.contains('\\') || s.contains("/../") || s.contains("/./")
    {
        return Err(Error::Url);
    }
    Ok(())
}
/// Called on every redirect; the original GitHub URL must already be verified.
pub fn validate_redirect(s: &str, hop: usize) -> Result<(), Error> {
    let url = https_url(s)?;
    if hop > 5
        || !matches!(
            url.host_str(),
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
        )
    {
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
