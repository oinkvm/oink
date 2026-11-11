//! Feed v2: one signed catalog per platform, architecture and channel. Its
//! ordered rules route each installed build to a target release and say how
//! the update is offered. The Windows service and the macOS backend both
//! select through `VerifiedCatalog::plan`.
use crate::{
    Error, PRODUCT,
    feed::{self, Channel, Package, ReleaseNotes, Trust, Watermark, strict_json},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeSet;

pub const SCHEMA: u32 = 2;
/// Highest minimumUpdaterSchema a v2 client installs by itself.
pub const UPDATER_CAPABILITY: u32 = 3;
pub const MAX_RELEASES: usize = 32;
pub const MAX_RULES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Windows,
    Mac,
}
impl Platform {
    fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Mac => "mac",
        }
    }
}

/// How an update is offered. Local packages are always `Normal`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Normal,
    /// Cannot be skipped; carries an optional notice shown before the notes.
    Critical,
    /// Shown with its notes but never downloaded: the user installs it from
    /// the releases page.
    Manual,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    None,
    Normal,
    Critical,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Latest,
    Build(u64),
}
impl Serialize for Target {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Latest => serializer.serialize_str("latest"),
            Self::Build(build) => serializer.serialize_u64(*build),
        }
    }
}
impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(s) if s == "latest" => Ok(Self::Latest),
            serde_json::Value::Number(n) => n
                .as_u64()
                .filter(|&build| build > 0)
                .map(Self::Build)
                .ok_or_else(|| serde::de::Error::custom("invalid target build")),
            _ => Err(serde::de::Error::custom("target must be \"latest\" or a build")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    /// Inclusive installed-build range `[min, max]`.
    pub installed: [u64; 2],
    pub kind: RuleKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<ReleaseNotes>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Release {
    pub build: u64,
    pub version: String,
    pub tag: String,
    pub channel: Channel,
    pub source_tree_sha256: String,
    /// `10.0.<build>` on Windows, `14.2` style on macOS.
    pub minimum_os: String,
    pub protocol_baseline: u32,
    pub minimum_updater_schema: u32,
    pub notes: ReleaseNotes,
    pub package: Package,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalog {
    pub schema: u32,
    pub product: String,
    pub platform: Platform,
    pub key_epoch: String,
    pub architecture: String,
    pub channel: Channel,
    pub sequence: u64,
    pub generated_at: String,
    pub expires_at: String,
    pub releases: Vec<Release>,
    pub rules: Vec<Rule>,
}

/// What this installation asks for: the catalog must be exactly that one.
#[derive(Clone, Copy, Debug)]
pub struct Expected<'a> {
    pub platform: Platform,
    pub architecture: &'a str,
    pub channel: Channel,
}
/// Path of a catalog under a source origin.
pub fn path(platform: Platform, epoch: &str, architecture: &str, channel: Channel) -> String {
    format!(
        "{}/v2/{epoch}/{architecture}/{}/manifest.json",
        platform.as_str(),
        channel.as_str()
    )
}

/// Major/minor/patch operating-system version; missing parts compare as zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OsVersion(pub [u32; 3]);
impl OsVersion {
    pub fn parse(s: &str) -> Result<Self, Error> {
        let mut parts = [0; 3];
        let mut count = 0;
        for part in s.split('.') {
            if count == 3 || part.is_empty() || part.len() > 9 || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Metadata);
            }
            parts[count] = part.parse().map_err(|_| Error::Metadata)?;
            count += 1;
        }
        if count < 2 {
            return Err(Error::Metadata);
        }
        Ok(Self(parts))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Automatic,
    Manual,
}
#[derive(Debug)]
pub struct Selection<'a> {
    pub installed_build: u64,
    pub successful_build: u64,
    pub quarantine: &'a BTreeSet<u64>,
    pub channel: Channel,
    pub os: OsVersion,
    pub skipped_build: Option<u64>,
    pub check: Check,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub release: Release,
    pub kind: Kind,
    pub notice: Option<ReleaseNotes>,
}
/// Why no update is offered, when the reason is worth showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// A release this installation is routed to needs a newer operating system.
    OsUnsupported,
}

/// Only obtained after signature, identity, freshness, replay and structure checks.
#[derive(Clone, Debug)]
pub struct VerifiedCatalog {
    catalog: Catalog,
    watermark: Watermark,
    expires_unix: i64,
}
impl VerifiedCatalog {
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
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
    pub fn plan(&self, s: &Selection<'_>) -> Result<Plan, Outcome> {
        let rule = self
            .catalog
            .rules
            .iter()
            .find(|rule| (rule.installed[0]..=rule.installed[1]).contains(&s.installed_build))
            .ok_or(Outcome::UpToDate)?;
        let kind = match rule.kind {
            RuleKind::None => return Err(Outcome::UpToDate),
            RuleKind::Normal => Kind::Normal,
            RuleKind::Critical => Kind::Critical,
            RuleKind::Manual => Kind::Manual,
        };
        let eligible = |r: &Release, any_os: bool| {
            r.build > s.installed_build.max(s.successful_build)
                && !s.quarantine.contains(&r.build)
                && r.channel <= s.channel
                && r.protocol_baseline == 1
                // The user installs a manual target from its release page.
                && (kind == Kind::Manual || r.minimum_updater_schema <= UPDATER_CAPABILITY)
                && (any_os || OsVersion::parse(&r.minimum_os).is_ok_and(|minimum| minimum <= s.os))
        };
        let pick = |any_os: bool| {
            let mut releases = self.catalog.releases.iter().filter(|r| eligible(r, any_os));
            match rule.target {
                Some(Target::Build(build)) => releases.find(|r| r.build == build),
                _ => releases.max_by_key(|r| r.build),
            }
        };
        let Some(release) = pick(false) else {
            return Err(if pick(true).is_some() {
                Outcome::OsUnsupported
            } else {
                Outcome::UpToDate
            });
        };
        if kind == Kind::Normal && s.check == Check::Automatic && s.skipped_build == Some(release.build) {
            return Err(Outcome::UpToDate);
        }
        Ok(Plan {
            release: release.clone(),
            kind,
            notice: rule.notice.clone(),
        })
    }
}

pub fn verify(
    bytes: &[u8],
    trust: &Trust<'_>,
    expected: &Expected<'_>,
    seen: &Watermark,
    now: i64,
) -> Result<VerifiedCatalog, Error> {
    let payload = feed::signed_payload(bytes, trust)?;
    let catalog: Catalog = strict_json(&payload)?;
    if catalog.schema != SCHEMA
        || catalog.product != PRODUCT
        || catalog.platform != expected.platform
        || catalog.key_epoch != trust.epoch
        || catalog.architecture != expected.architecture
        || catalog.channel != expected.channel
        || catalog.sequence == 0
    {
        return Err(Error::Policy);
    }
    let (watermark, expires_unix) = feed::freshness(
        &payload,
        catalog.sequence,
        &catalog.generated_at,
        &catalog.expires_at,
        seen,
        now,
    )?;
    if catalog.releases.len() > MAX_RELEASES || catalog.rules.len() > MAX_RULES {
        return Err(Error::Limit);
    }
    let mut builds = BTreeSet::new();
    for release in &catalog.releases {
        if release.build == 0
            || !builds.insert(release.build)
            || release.version.is_empty()
            || release.version.len() > 128
            || release.tag.is_empty()
            || release.tag.len() > 128
            || release.channel > catalog.channel
            || !feed::is_hex(&release.source_tree_sha256, 64)
        {
            return Err(Error::Metadata);
        }
        OsVersion::parse(&release.minimum_os)?;
        feed::validate_package(&release.package, trust)?;
    }
    if catalog.rules.is_empty() {
        return Err(Error::Metadata);
    }
    for rule in &catalog.rules {
        let target_ok = match (rule.kind, rule.target) {
            (RuleKind::None, None) => true,
            (RuleKind::None, Some(_)) | (_, None) => false,
            (_, Some(Target::Latest)) => true,
            (_, Some(Target::Build(build))) => builds.contains(&build),
        };
        if rule.installed[0] > rule.installed[1]
            || !target_ok
            || (rule.notice.is_some() && rule.kind != RuleKind::Critical)
        {
            return Err(Error::Metadata);
        }
    }
    Ok(VerifiedCatalog {
        catalog,
        watermark,
        expires_unix,
    })
}

#[cfg(test)]
mod tests;
