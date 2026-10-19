//! A durable clipboard identity high-water mark, independent of clearable content.
use serde_json::{Map, Value};
use std::{io, path::PathBuf};

pub(super) struct Clock {
    path: PathBuf,
    fields: Map<String, Value>,
    value: u64,
}
impl Clock {
    pub(super) fn load(path: PathBuf) -> io::Result<Self> {
        let fields = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                .map_err(io::Error::other)?
                .as_object()
                .cloned()
                .ok_or_else(invalid)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Map::from_iter([("schema".into(), Value::from(1)), ("clock".into(), Value::from("0"))])
            }
            Err(error) => return Err(error),
        };
        if fields.get("schema").and_then(Value::as_u64) != Some(1) {
            return Err(invalid());
        }
        let text = fields.get("clock").and_then(Value::as_str).ok_or_else(invalid)?;
        let value = text.parse::<u64>().map_err(|_| invalid())?;
        if value.to_string() != text {
            return Err(invalid());
        }
        Ok(Self { path, fields, value })
    }
    pub(super) fn value(&self) -> u64 {
        self.value
    }
    pub(super) fn save(&mut self, value: u64) -> io::Result<()> {
        if value <= self.value {
            return Ok(());
        }
        let mut fields = self.fields.clone();
        fields.insert("clock".into(), value.to_string().into());
        oink_foundation::storage::atomic_write(&self.path, &serde_json::to_vec(&fields).map_err(io::Error::other)?)?;
        self.fields = fields;
        self.value = value;
        Ok(())
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard clock")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn highwater_is_explicit_atomic_and_retains_unknown_fields() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("clock.json");
        std::fs::write(
            &path,
            br#"{"schema":1,"clock":"18446744073709551614","future":{"value":[1,2]}}"#,
        )
        .unwrap();
        let mut clock = Clock::load(path.clone()).unwrap();
        clock.save(u64::MAX).unwrap();
        clock.save(1).unwrap();
        assert_eq!(Clock::load(path.clone()).unwrap().value(), u64::MAX);
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["future"]["value"], serde_json::json!([1, 2]));
    }
    #[test]
    fn unreadable_clock_is_never_treated_as_new_identity() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("clock.json");
        for bytes in [
            "123",
            r#"{"schema":2,"clock":"123"}"#,
            r#"{"schema":1,"clock":123}"#,
            r#"{"schema":1,"clock":"01"}"#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(Clock::load(path.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        }
        std::fs::remove_file(&path).unwrap();
        let mut clock = Clock::load(path.clone()).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(clock.save(10).is_err());
        assert_eq!(clock.value(), 0, "failed writes do not advance durable highwater");
    }
}
