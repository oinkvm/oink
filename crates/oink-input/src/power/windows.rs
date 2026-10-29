use super::State;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            Power::*,
            Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0},
        },
    },
    core::PWSTR,
};
pub fn observe() -> Result<State, String> {
    oink_winsvc::client::power_snapshot()
        .map(|s| State {
            display: s.display,
            system: s.system,
            activity: false,
        })
        .map_err(|e| e.to_string())
}
#[derive(Debug, Default)]
pub struct Hold {
    display: Option<HANDLE>,
    system: Option<HANDLE>,
}
impl Hold {
    pub fn apply(&mut self, state: State) -> Result<(), String> {
        for (slot, on, kind) in [
            (&mut self.display, state.display, PowerRequestDisplayRequired),
            (&mut self.system, state.system, PowerRequestSystemRequired),
        ] {
            if !on && let Some(handle) = slot.take() {
                // SAFETY: each owned handle has exactly one set request; closing
                // the object also retires it if clearing fails during shutdown.
                unsafe {
                    let _ = PowerClearRequest(handle, kind);
                    let _ = CloseHandle(handle);
                }
            }
        }
        for (slot, on, kind) in [
            (&mut self.display, state.display, PowerRequestDisplayRequired),
            (&mut self.system, state.system, PowerRequestSystemRequired),
        ] {
            if on && slot.is_none() {
                let mut reason: Vec<u16> = "Oink mirroring group devices".encode_utf16().chain(Some(0)).collect();
                let context = REASON_CONTEXT {
                    Version: 0,
                    Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                    Reason: REASON_CONTEXT_0 {
                        SimpleReasonString: PWSTR(reason.as_mut_ptr()),
                    },
                };
                // SAFETY: reason string and context live through synchronous creation.
                let handle = unsafe { PowerCreateRequest(&context) }.map_err(|e| e.to_string())?;
                // SAFETY: newly owned valid power request handle.
                if let Err(error) = unsafe { PowerSetRequest(handle, kind) } {
                    // SAFETY: no request was set; close the newly owned handle.
                    unsafe {
                        let _ = CloseHandle(handle);
                    }
                    return Err(error.to_string());
                }
                *slot = Some(handle);
            }
        }
        Ok(())
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        let _ = self.apply(State::default());
    }
}
