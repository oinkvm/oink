//! Read-only kernel notifications; no firewall-policy enumeration on idle ticks.
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE, WAIT_OBJECT_0},
        System::{
            Registry::*,
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::{HSTRING, Result},
};

const POLICY_STORES: [&str; 2] = [
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy",
    r"SOFTWARE\Policies\Microsoft\WindowsFirewall",
];

pub(super) struct Notifications(Vec<Watch>);
impl Notifications {
    pub(super) fn new() -> Self {
        Self(
            POLICY_STORES
                .into_iter()
                .filter_map(|path| match Watch::new(path) {
                    Ok(watch) => Some(watch),
                    Err(error) => {
                        tracing::warn!(
                            native_code = error.code().0,
                            "firewall change notification unavailable; retaining startup state"
                        );
                        None
                    }
                })
                .collect(),
        )
    }

    #[cfg(test)]
    pub(super) fn registered_count(&self) -> usize {
        self.0.len()
    }

    pub(super) fn changed(&mut self) -> bool {
        let mut changed = false;
        self.0.retain_mut(|watch| {
            // This is a nonblocking wait on an OS-signaled event, not a policy
            // read/poll. The shared platform observer already wakes for sensors.
            // SAFETY: this owner retains the event through the wait.
            if unsafe { WaitForSingleObject(watch.event, 0) } != WAIT_OBJECT_0 {
                return true;
            }
            // RegNotifyChangeKeyValue is one-shot. Open/arm before the expensive
            // query so a write during that query is retained for the next turn.
            // Reopening also handles deletion/recreation of a watched store.
            match Watch::new(&watch.target) {
                Ok(next) => {
                    // An ancestor registration also sees unrelated policy writes.
                    // Invalidate only if the target existed or now exists; while
                    // it stays absent, just keep listening for its creation.
                    changed |= watch.target_exists || next.target_exists;
                    *watch = next;
                    true
                }
                Err(error) => {
                    changed |= watch.target_exists;
                    tracing::warn!(
                        native_code = error.code().0,
                        "firewall notification rearm failed; retaining last observed state"
                    );
                    false
                }
            }
        });
        changed
    }
}

struct Watch {
    key: HKEY,
    event: HANDLE,
    target: String,
    target_exists: bool,
}
impl Watch {
    fn new(target: &str) -> Result<Self> {
        // A GPO store may not exist yet. Watch its closest existing ancestor's
        // subtree so creation is also notified, then narrow on the next event.
        // Only KEY_NOTIFY is requested; no registry/security settings are changed.
        let mut path = target;
        let key = loop {
            let mut key = HKEY::default();
            // SAFETY: local fixed registry path, query-only rights and valid output.
            let result = unsafe {
                RegOpenKeyExW(
                    HKEY_LOCAL_MACHINE,
                    &HSTRING::from(path),
                    None,
                    KEY_NOTIFY | KEY_WOW64_64KEY,
                    &mut key,
                )
            };
            if result.is_ok() {
                break key;
            }
            if result != ERROR_FILE_NOT_FOUND && result != ERROR_PATH_NOT_FOUND {
                return Err(windows::core::Error::from_hresult(result.to_hresult()));
            }
            let Some((parent, _)) = path.rsplit_once('\\') else {
                return Err(windows::core::Error::from_hresult(result.to_hresult()));
            };
            path = parent;
        };
        // SAFETY: unnamed manual-reset event, no inheritance, owned below.
        let event = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(event) => event,
            Err(error) => {
                // SAFETY: release the successfully opened key on event failure.
                unsafe {
                    let _ = RegCloseKey(key);
                }
                return Err(error);
            }
        };
        let watch = Self {
            key,
            event,
            target: target.into(),
            target_exists: path == target,
        };
        // RegNotifyChangeKeyValue documents subtree NAME/LAST_SET events for
        // creation/deletion and value writes. Cover BOTH local and GPO stores.
        // Failure means startup-only/last-known state, never a polling fallback.
        // SAFETY: persistent observer thread owns the key/event until Drop.
        unsafe {
            RegNotifyChangeKeyValue(
                key,
                true,
                REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                Some(event),
                true,
            )
            .ok()?;
        }
        Ok(watch)
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: closing the key cancels notification before its event is closed.
        unsafe {
            let _ = RegCloseKey(self.key);
            let _ = CloseHandle(self.event);
        }
    }
}
