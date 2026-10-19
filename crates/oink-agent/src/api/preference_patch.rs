//! Partial local commands. Snapshots remain complete; omitted fields are never edits.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::marker::PhantomData;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent, bound = "")]
pub struct PreferencePatch<T> {
    fields: Map<String, Value>,
    #[serde(skip)]
    marker: PhantomData<T>,
}
impl<T: Serialize> From<T> for PreferencePatch<T> {
    fn from(value: T) -> Self {
        Self {
            fields: serde_json::to_value(value)
                .expect("preference serialization")
                .as_object()
                .expect("preference object")
                .clone(),
            marker: PhantomData,
        }
    }
}
impl<T: Serialize + DeserializeOwned> PreferencePatch<T> {
    pub(crate) fn value(&self) -> Value {
        Value::Object(self.fields.clone())
    }
    pub(crate) fn apply(&self, current: &T) -> super::ApiResult<T> {
        let mut merged = serde_json::to_value(current).expect("preference serialization");
        merge(&mut merged, &self.value());
        let result: T = serde_json::from_value(merged).map_err(|_| invalid())?;
        if !known_keys(
            &self.value(),
            &serde_json::to_value(&result).expect("preference serialization"),
        ) {
            return Err(invalid());
        }
        Ok(result)
    }
}
fn invalid() -> super::ApiError {
    super::ApiError::InvalidArgument {
        message: "Invalid preference patch".into(),
    }
}
fn merge(current: &mut Value, patch: &Value) {
    if let (Some(current), Some(patch)) = (current.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            merge(current.entry(key.clone()).or_insert(Value::Null), value);
        }
    } else {
        *current = patch.clone();
    }
}
fn known_keys(patch: &Value, applied: &Value) -> bool {
    match (patch, applied) {
        (Value::Object(patch), Value::Object(applied)) => patch
            .iter()
            .all(|(key, value)| applied.get(key).is_some_and(|known| known_keys(value, known))),
        (Value::Array(patch), Value::Array(applied)) => {
            patch.iter().zip(applied).all(|(value, known)| known_keys(value, known))
        }
        _ => true,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::{api::UiPreferences, config::AgentConfig};
    use serde_json::json;
    #[test]
    fn explicit_default_is_pinned_while_omitted_defaults_remain_unset() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        let owner = config.owner().clone();
        let patch: PreferencePatch<UiPreferences> = serde_json::from_value(json!({"confirm_quit":true})).unwrap();
        let applied = patch.apply(&UiPreferences::from(&config.settings)).unwrap();
        config.update_local(&patch.value(), |s| applied.apply(s)).unwrap();
        let raw: Value = serde_json::from_slice(&std::fs::read(config.settings_path()).unwrap()).unwrap();
        assert_eq!(raw["local"], json!({"confirm_quit":true}));
        assert_eq!(*config.owner(), owner);
        assert!(AgentConfig::load_from(dir.path()).unwrap().settings.confirm_quit);
    }
    #[test]
    fn nullable_audio_value_and_nested_patch_preserve_other_fields() {
        let audio = oink_audio::Settings {
            receive_output: Some("output".into()),
            ..Default::default()
        };
        let patch: PreferencePatch<oink_audio::Settings> =
            serde_json::from_value(json!({"receive_output":null})).unwrap();
        let applied = patch.apply(&audio).unwrap();
        assert_eq!(applied.receive_output, None);
        assert_eq!(applied.keep_local_playback, audio.keep_local_playback);
        let current = UiPreferences::from(&crate::config::Settings::default());
        let patch: PreferencePatch<UiPreferences> =
            serde_json::from_value(json!({"theme_accent":{"dark":"#112233"}})).unwrap();
        let applied = patch.apply(&current).unwrap();
        assert_eq!(applied.theme_accent.light, current.theme_accent.light);
        assert_eq!(applied.theme_accent.dark.as_str(), "#112233");
    }
    #[test]
    fn invalid_and_unknown_supplied_fields_are_rejected() {
        let current = UiPreferences::from(&crate::config::Settings::default());
        for value in [
            json!({"confirm_quit":null}),
            json!({"log_max_mb":0}),
            json!({"future":true}),
            json!({"layout_projection_enabled":false}),
            json!({"theme_accent":{"future":true}}),
        ] {
            let patch: PreferencePatch<UiPreferences> = serde_json::from_value(value).unwrap();
            assert!(patch.apply(&current).is_err());
        }
    }
}
