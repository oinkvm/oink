//! Leniency belongs only to local preferences, never to IPC or group state.
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::{Settings, SettingsIssue, SettingsIssueKind};

#[derive(Default)]
pub(crate) struct Reader {
    pub issues: Vec<SettingsIssue>,
}

fn child(path: &str, key: &str) -> String {
    format!("{path}.{key}")
}

impl Reader {
    fn issue(&mut self, path: String, kind: SettingsIssueKind) {
        self.issues.push(SettingsIssue { path, kind });
    }

    /// Serde supplies paths without copying its error text (which can contain values).
    fn entry<T: DeserializeOwned + Serialize>(&mut self, value: Value, path: &str) -> Result<T, String> {
        let result = serde_path_to_error::deserialize(value.clone());
        match result {
            Ok(value_typed) => {
                // Comparing structural keys also covers internally tagged enum fields,
                // where Serde buffers content and ignored-field callbacks cannot see it.
                self.unknown_keys(&value, &serde_json::to_value(&value_typed).unwrap(), path);
                Ok(value_typed)
            }
            Err(error) => {
                let key = error.path().to_string();
                Err(if key == "." { path.into() } else { child(path, &key) })
            }
        }
    }

    fn unknown_keys(&mut self, supplied: &Value, understood: &Value, path: &str) {
        match (supplied, understood) {
            (Value::Object(supplied), Value::Object(understood)) => {
                for (key, value) in supplied {
                    let path = child(path, key);
                    if let Some(known) = understood.get(key) {
                        self.unknown_keys(value, known, &path);
                    } else {
                        self.issue(path, SettingsIssueKind::UnknownKey);
                    }
                }
            }
            (Value::Array(supplied), Value::Array(understood)) => {
                for (index, (value, known)) in supplied.iter().zip(understood).enumerate() {
                    self.unknown_keys(value, known, &format!("{path}[{index}]"));
                }
            }
            _ => {}
        }
    }

    /// Validate each field against a complete typed default object. This also runs
    /// field-level validators and avoids keeping a second table of scalar defaults.
    pub(crate) fn object<T: Serialize + DeserializeOwned>(&mut self, value: Value, default: T, path: &str) -> T {
        let Value::Object(mut supplied) = value else {
            self.issue(path.into(), SettingsIssueKind::Defaulted);
            return default;
        };
        let baseline = serde_json::to_value(&default).expect("settings defaults serialize");
        let mut accepted = baseline.as_object().expect("settings object").clone();
        for (key, fallback) in baseline.as_object().unwrap() {
            let Some(value) = supplied.remove(key) else { continue };
            let field_path = child(path, key);
            let value = self.nested(value, &field_path);
            let mut candidate = baseline.as_object().unwrap().clone();
            candidate.insert(key.clone(), value.clone());
            // Only one supplied field in this candidate; failures cannot affect siblings.
            match self.entry::<T>(Value::Object(candidate), path) {
                Ok(_) => {
                    accepted.insert(key.clone(), value);
                }
                Err(error_path) => {
                    self.issue(error_path, SettingsIssueKind::Defaulted);
                    accepted.insert(key.clone(), fallback.clone());
                }
            }
        }
        for key in supplied.keys() {
            self.issue(child(path, key), SettingsIssueKind::UnknownKey);
        }
        serde_json::from_value(Value::Object(accepted)).expect("validated settings fields")
    }

    fn optional_object<T: Serialize + DeserializeOwned>(&mut self, value: Value, default: T, path: &str) -> Value {
        if value.is_null() {
            return value;
        }
        // A malformed optional object falls back to None, rather than enabling it.
        if !value.is_object() {
            self.issue(path.into(), SettingsIssueKind::Defaulted);
            return Value::Null;
        }
        serde_json::to_value(self.object(value, default, path)).unwrap()
    }

    fn entries<T: Serialize + DeserializeOwned>(&mut self, value: Value, path: &str) -> Value {
        let Value::Array(entries) = value else { return value };
        Value::Array(
            entries
                .into_iter()
                .enumerate()
                .filter_map(
                    |(index, value)| match self.entry::<T>(value, &format!("{path}[{index}]")) {
                        Ok(entry) => Some(serde_json::to_value(entry).unwrap()),
                        Err(error_path) => {
                            self.issue(error_path, SettingsIssueKind::DroppedEntry);
                            None
                        }
                    },
                )
                .collect(),
        )
    }

    fn nested(&mut self, value: Value, path: &str) -> Value {
        match path {
            "settings.routing" => {
                serde_json::to_value(self.object(value, oink_routing::RoutingSettings::default(), path)).unwrap()
            }
            "settings.audio" => {
                serde_json::to_value(self.object(value, oink_audio::Settings::default(), path)).unwrap()
            }
            "settings.theme_accent" => {
                serde_json::to_value(self.object(value, super::ThemeAccent::default(), path)).unwrap()
            }
            "settings.clipboard" => self.optional_object(value, crate::clip::default_settings(), path),
            "settings.routing.damping" => {
                serde_json::to_value(self.object(value, oink_routing::damping::DampingConfig::default(), path)).unwrap()
            }
            "settings.hotkeys" => self.entries::<crate::api::Hotkey>(value, path),
            "settings.routing.damping.rules" => self.entries::<oink_routing::damping::DampingRule>(value, path),
            _ => value,
        }
    }

    pub fn settings(&mut self, value: Value) -> Settings {
        self.object(value, Settings::default(), "settings")
    }
}

/// Apply only an explicit typed change to the retained JSON. Getter defaults
/// and rejected entries are not a reason to rewrite untouched raw content.
pub(crate) fn patch_json(raw: &mut Value, before: &Value, after: &Value) {
    if before == after {
        return;
    }
    // Tagged policies and consent are complete typed records. In particular a
    // consent's policy version must never follow a later release's default.
    if after.get("kind").is_some() || (after.get("choice").is_some() && after.get("policy_version").is_some()) {
        *raw = after.clone();
        return;
    }
    if before.is_object() && after.is_object() && !raw.is_object() {
        *raw = serde_json::json!({});
    }
    match (raw, before, after) {
        (Value::Object(raw), Value::Object(before), Value::Object(after)) => {
            for (key, value) in after {
                if before.get(key) == Some(value) {
                    continue;
                }
                // A chord is one value. Explicit edits are already present in
                // raw when persist replays this patch; retaining primitive key
                // names individually would append the replacement key twice.
                if key == "keys" && after.contains_key("action") {
                    raw.insert(key.clone(), value.clone());
                    continue;
                }
                if let Some(old) = before.get(key) {
                    let raw = raw.entry(key.clone()).or_insert_with(|| {
                        if old.is_object() && value.is_object() {
                            serde_json::json!({})
                        } else {
                            old.clone()
                        }
                    });
                    patch_json(raw, old, value);
                } else {
                    raw.insert(key.clone(), value.clone());
                }
            }
            for key in before.keys() {
                if !after.contains_key(key) {
                    raw.remove(key);
                }
            }
        }
        (Value::Array(raw), Value::Array(before), Value::Array(after)) => {
            // Hotkeys have stable action/target identity. Unknown actions survive edits
            // to known bindings, including explicit removal and reordering.
            let keyed = before.iter().chain(after).all(|v| v.get("action").is_some());
            if keyed {
                let mut updated = Vec::new();
                for new in after {
                    let old = before.iter().find(|v| hotkey_identity(v, new));
                    let mut retained = raw
                        .iter()
                        .find(|v| hotkey_identity(v, new))
                        .cloned()
                        .unwrap_or_else(|| new.clone());
                    if let Some(old) = old {
                        patch_json(&mut retained, old, new);
                    }
                    updated.push(retained);
                }
                updated.extend(
                    raw.iter()
                        .filter(|v| !before.iter().any(|old| hotkey_identity(old, v)))
                        .cloned(),
                );
                *raw = updated;
            } else {
                let mut updated = Vec::new();
                for (index, new) in after.iter().enumerate() {
                    let old_index = before
                        .iter()
                        .position(|old| old == new)
                        .or((index < before.len()).then_some(index));
                    let mut retained = old_index
                        .and_then(|i| raw.iter().find(|value| known_equal(value, &before[i])))
                        .cloned()
                        .unwrap_or_else(|| new.clone());
                    if let Some(i) = old_index {
                        patch_json(&mut retained, &before[i], new);
                    }
                    updated.push(retained);
                }
                updated.extend(
                    raw.iter()
                        .filter(|value| !before.iter().any(|old| known_equal(value, old)))
                        .cloned(),
                );
                *raw = updated;
            }
        }
        (raw, _, after) => *raw = after.clone(),
    }
}
/// Retain only supplied local leaves, taking their validated, canonical values.
/// A collection is one explicit preference; omitted object siblings stay unset.
pub(crate) fn explicit_json(raw: &mut Value, patch: &Value, canonical: &Value) {
    if let (Value::Object(patch), Value::Object(canonical)) = (patch, canonical) {
        if !raw.is_object() {
            *raw = serde_json::json!({});
        }
        let raw = raw.as_object_mut().unwrap();
        for (key, value) in patch {
            if let Some(known) = canonical.get(key) {
                explicit_json(raw.entry(key.clone()).or_insert(Value::Null), value, known);
            }
        }
    } else if let (Some(raw), Some(known)) = (raw.as_array_mut(), canonical.as_array()) {
        if known.iter().all(|entry| entry.get("action").is_some()) {
            for entry in known {
                if let Some(retained) = raw.iter_mut().find(|old| hotkey_identity(old, entry)) {
                    explicit_json(retained, entry, entry);
                } else {
                    raw.push(entry.clone());
                }
            }
        } else {
            *raw = known.clone();
        }
    } else {
        *raw = canonical.clone();
    }
}
fn known_equal(raw: &Value, known: &Value) -> bool {
    match (raw, known) {
        (Value::Object(raw), Value::Object(known)) => known
            .iter()
            .all(|(key, value)| raw.get(key).is_some_and(|raw| known_equal(raw, value))),
        (Value::Array(raw), Value::Array(known)) => {
            raw.len() == known.len() && raw.iter().zip(known).all(|(raw, known)| known_equal(raw, known))
        }
        _ => raw == known,
    }
}

fn hotkey_identity(a: &Value, b: &Value) -> bool {
    a.get("action") == b.get("action")
        && a.get("device").filter(|v| !v.is_null()) == b.get("device").filter(|v| !v.is_null())
}
