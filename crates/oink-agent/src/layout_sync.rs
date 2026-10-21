//! UI layout revision/source projections. ConfigModule is the sole
//! persistence and conflict-resolution owner; no separate layout store exists.
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// 布局的来源（与界面的 `LayoutSource` 对应）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Default,
    Manual,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Manual => "manual",
        }
    }
}

impl TryFrom<oink_protocol::v1::dto::LayoutSource> for Source {
    type Error = oink_protocol::v1::Error;
    fn try_from(source: oink_protocol::v1::dto::LayoutSource) -> Result<Self, Self::Error> {
        match source {
            oink_protocol::v1::dto::LayoutSource::Default => Ok(Self::Default),
            oink_protocol::v1::dto::LayoutSource::Manual => Ok(Self::Manual),
            _ => Err(oink_protocol::v1::ErrorCode::UnsupportedValue.into()),
        }
    }
}
impl From<Source> for oink_protocol::v1::dto::LayoutSource {
    fn from(source: Source) -> Self {
        match source {
            Source::Default => Self::Default,
            Source::Manual => Self::Manual,
        }
    }
}

/// 布局的版本。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub revision: u64,
    pub source: Source,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl crate::node::Node {
    pub(crate) async fn reset_initial_layout(&self) -> crate::api::ApiResult<crate::api::Layout> {
        let current = self.layout().0;
        let mut devices = Vec::new();
        for device in current
            .screens
            .iter()
            .map(|s| s.id.device)
            .collect::<std::collections::BTreeSet<_>>()
        {
            let displays = self
                .device_facts(device)
                .and_then(|facts| facts.displays().ok())
                .map(|facts| {
                    facts
                        .0
                        .iter()
                        .filter_map(crate::agent_api::display_fact)
                        .collect::<Vec<_>>()
                })
                .ok_or_else(|| crate::api::ApiError::Unavailable {
                    message: "Display facts are unavailable".into(),
                })?;
            devices.push((device, displays));
        }
        let mut initial = oink_display::layout::Layout::auto(&devices);
        initial.settings = current.settings;
        self.edit_layout(crate::layout_sync::Source::Manual, |layout| *layout = initial)
            .await
            .map_err(|message| crate::api::ApiError::Unavailable { message })?;
        crate::agent_api::layout_api(self)
    }
}
