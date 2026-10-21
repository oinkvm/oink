use super::{check, online_display_ids};
use crate::{
    DisplayError,
    arrangement::{ArrangementError, NativeConfiguration, NativeDisplay, NativeOrigin},
};
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayOrigin, CGConfigureOption, CGDirectDisplayID, CGDisplayConfigRef, CGDisplayCopyDisplayMode,
    CGDisplayMirrorsDisplay, CGDisplayMode,
};
use std::collections::BTreeMap;

fn read() -> Result<(NativeConfiguration, BTreeMap<String, CGDirectDisplayID>), DisplayError> {
    let ids = online_display_ids()?;
    // Keep the arrangement writer entirely on display-configuration/query APIs.
    // Product names and private CoreDisplay metadata are unnecessary here.
    let keys = ids
        .iter()
        .map(|id| {
            super::info::display_uuid(*id)
                .map(|key| (*id, key))
                .ok_or(DisplayError::Unsupported)
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut native = NativeConfiguration { displays: Vec::new() };
    let mut handles = BTreeMap::new();
    for (id, key) in &keys {
        if handles.insert(key.clone(), *id).is_some() {
            return Err(ArrangementError::Invalid.into());
        }
        let mirror = CGDisplayMirrorsDisplay(*id);
        let mirror_of = if mirror == 0 {
            None
        } else {
            Some(keys.get(&mirror).ok_or(ArrangementError::Changed)?.clone())
        };
        let mode = CGDisplayCopyDisplayMode(*id).ok_or(ArrangementError::Changed)?;
        let bounds = objc2_core_graphics::CGDisplayBounds(*id);
        native.displays.push(NativeDisplay {
            key: key.clone(),
            bounds: oink_foundation::Rect::new(bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height),
            primary: objc2_core_graphics::CGDisplayIsMain(*id),
            mirror_of,
            mode: format!(
                "{}:{}:{}:{}",
                CGDisplayMode::io_display_mode_id(Some(&mode)),
                CGDisplayMode::pixel_width(Some(&mode)),
                CGDisplayMode::pixel_height(Some(&mode)),
                objc2_core_graphics::CGDisplayRotation(*id)
            ),
        });
    }
    native.canonicalize();
    Ok((native, handles))
}

pub(super) fn configuration() -> Result<NativeConfiguration, DisplayError> {
    read().map(|(c, _)| c)
}

trait TransactionApi {
    fn begin(&mut self) -> Result<(), DisplayError>;
    fn origin(&mut self, display: CGDirectDisplayID, x: i32, y: i32) -> Result<(), DisplayError>;
    fn complete(&mut self) -> Result<(), DisplayError>;
    fn cancel(&mut self);
}

struct Quartz(CGDisplayConfigRef);
impl TransactionApi for Quartz {
    fn begin(&mut self) -> Result<(), DisplayError> {
        // SAFETY: the out-pointer is valid and owned by this transaction.
        check(
            unsafe { CGBeginDisplayConfiguration(&mut self.0) },
            "CGBeginDisplayConfiguration",
        )
    }
    fn origin(&mut self, display: CGDirectDisplayID, x: i32, y: i32) -> Result<(), DisplayError> {
        // SAFETY: begin succeeded; the display handle came from this fresh snapshot.
        check(
            unsafe { CGConfigureDisplayOrigin(self.0, display, x, y) },
            "CGConfigureDisplayOrigin",
        )
    }
    fn complete(&mut self) -> Result<(), DisplayError> {
        let config = std::mem::replace(&mut self.0, std::ptr::null_mut());
        // SAFETY: completing consumes the transaction exactly once. Session scope
        // keeps an ordinary app exit from silently undoing the user's arrangement.
        check(
            unsafe { CGCompleteDisplayConfiguration(config, CGConfigureOption::ForSession) },
            "CGCompleteDisplayConfiguration",
        )
    }
    fn cancel(&mut self) {
        if !self.0.is_null() {
            // SAFETY: only a successfully begun, unconsumed transaction is cancelled.
            unsafe { CGCancelDisplayConfiguration(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}
impl Drop for Quartz {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn execute(api: &mut impl TransactionApi, calls: &[(CGDirectDisplayID, i32, i32)]) -> Result<(), DisplayError> {
    api.begin()?;
    for &(display, x, y) in calls {
        if let Err(error) = api.origin(display, x, y) {
            api.cancel();
            return Err(error);
        }
    }
    api.complete()
}

pub(super) fn apply(
    expected: &NativeConfiguration,
    origins: &[NativeOrigin],
) -> Result<NativeConfiguration, DisplayError> {
    let (current, handles) = read()?;
    if &current != expected {
        return Err(ArrangementError::Changed.into());
    }
    let target = current.with_origins(origins)?;
    if target == current {
        return Ok(current);
    }
    // Why this shape: Quartz normalizes gaps/overlaps and moving a mirrored
    // display can remove it from its mirror set. Require connected rectangles,
    // reject mirrored writes, preserve mode/primary and verify the actual result.
    if current.displays.iter().any(|d| d.mirror_of.is_some()) {
        return Err(DisplayError::Unsupported);
    }
    let calls = target
        .sources()
        .map(|d| {
            Ok((
                *handles.get(&d.key).ok_or(ArrangementError::Changed)?,
                d.bounds.x as i32,
                d.bounds.y as i32,
            ))
        })
        .collect::<Result<Vec<_>, DisplayError>>()?;
    execute(&mut Quartz(std::ptr::null_mut()), &calls)?;
    let observed = configuration()?;
    if observed.displays.len() != current.displays.len()
        || observed
            .displays
            .iter()
            .zip(&current.displays)
            .any(|(a, b)| a.key != b.key || a.mode != b.mode || a.primary != b.primary || a.mirror_of != b.mirror_of)
    {
        return Err(ArrangementError::Changed.into());
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        fail: bool,
    }
    impl TransactionApi for Fake {
        fn begin(&mut self) -> Result<(), DisplayError> {
            self.calls.push("begin".into());
            Ok(())
        }
        fn origin(&mut self, display: u32, x: i32, y: i32) -> Result<(), DisplayError> {
            self.calls.push(format!("origin:{display}:{x}:{y}"));
            if self.fail {
                Err(DisplayError::Platform("fixture".into()))
            } else {
                Ok(())
            }
        }
        fn complete(&mut self) -> Result<(), DisplayError> {
            self.calls.push("complete-session".into());
            Ok(())
        }
        fn cancel(&mut self) {
            self.calls.push("cancel".into());
        }
    }
    #[test]
    fn constructs_one_transaction_and_cancels_staging_failure() {
        let mut api = Fake::default();
        execute(&mut api, &[(1, 0, 0), (2, -800, 200)]).unwrap();
        assert_eq!(
            api.calls,
            ["begin", "origin:1:0:0", "origin:2:-800:200", "complete-session"]
        );
        let mut api = Fake {
            fail: true,
            ..Default::default()
        };
        assert!(execute(&mut api, &[(1, 0, 0), (2, 100, 0)]).is_err());
        assert_eq!(api.calls, ["begin", "origin:1:0:0", "cancel"]);
    }
}
