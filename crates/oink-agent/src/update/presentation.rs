//! Persist only presentation intent across an updater replacing its UI process.
//! This user-owned record can show a window; it never authorizes installation.
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

const FILE: &str = "update-window.json";
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u32,
    from_build: u64,
    expected_build: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Failed,
}
pub fn begin(directory: &Path, from_build: u64, expected_build: u64) -> io::Result<()> {
    if expected_build <= from_build {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let bytes = serde_json::to_vec(&Intent {
        schema: 1,
        from_build,
        expected_build,
    })?;
    oink_foundation::storage::atomic_write(&directory.join(FILE), &bytes)
}
pub fn outcome(directory: &Path, current_build: u64) -> Option<Outcome> {
    let bytes = std::fs::read(directory.join(FILE)).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let intent: Intent = serde_json::from_slice(&bytes).ok()?;
    if intent.schema != 1 || intent.expected_build <= intent.from_build {
        return None;
    }
    if current_build == intent.expected_build {
        Some(Outcome::Committed)
    } else if current_build == intent.from_build {
        Some(Outcome::Failed)
    } else {
        None
    }
}
pub fn clear(directory: &Path) {
    if let Err(error) = std::fs::remove_file(directory.join(FILE))
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "could not clear update window intent");
    }
}
pub fn show_main(normal_launch: bool, update: Option<Outcome>) -> bool {
    normal_launch || update.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn committed_relaunch_shows_main_even_if_the_original_launch_was_at_login() {
        let directory = tempfile::tempdir().unwrap();
        begin(directory.path(), 33, 34).unwrap();
        assert_eq!(outcome(directory.path(), 34), Some(Outcome::Committed));
        assert!(show_main(false, outcome(directory.path(), 34)));
        assert_eq!(outcome(directory.path(), 33), Some(Outcome::Failed));
        assert!(show_main(false, outcome(directory.path(), 33)));
        assert_eq!(outcome(directory.path(), 35), None);
        clear(directory.path());
        assert!(!show_main(false, outcome(directory.path(), 34)));
    }
    #[test]
    fn window_intent_is_finite_and_cannot_request_an_install_or_supply_a_path() {
        let directory = tempfile::tempdir().unwrap();
        assert!(begin(directory.path(), 34, 33).is_err());
        for bytes in [
            br#"{"schema":2,"from_build":33,"expected_build":34}"#.as_slice(),
            br#"{"schema":1,"from_build":33,"expected_build":34,"path":"payload.msi"}"#.as_slice(),
        ] {
            std::fs::write(directory.path().join(FILE), bytes).unwrap();
            assert_eq!(outcome(directory.path(), 34), None);
        }
    }
}
