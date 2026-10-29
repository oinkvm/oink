//! AppKit's accepted operation is separate from File Provider source deletion.
use super::*;

// Retain old catalog tombstones; no current drag writes this state.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Pending,
    Observed,
    Removed,
}
impl Registration {
    /// Observe the target-local MOVE without deleting or revoking its source.
    pub fn move_completed(&self) -> io::Result<()> {
        // Finder ends the AppKit session before copying a File Provider URL.
        // Field traces on macOS 14.7.8 and 26.6.2 show fetchContents starting
        // only after this callback. Deleting here, or waiting for existing
        // readers to drain, revokes content before Finder's first read.
        // A native probe on macOS 26.6.2 confirms EOF precedes deleteItem on
        // MOVE; an ordinary COPY reads the same bytes and keeps the source.
        // Finder's native deleteItem/trash operation owns received-copy removal;
        // neither this accepted operation nor consumption proves that deletion.
        // The remote original and Windows MOVE feedback are unaffected.
        Ok(())
    }
}
