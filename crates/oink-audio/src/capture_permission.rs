//! Prompt-free system-output capture preflight. No microphone permission queries.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CapturePreflight {
    Authorized,
    Denied,
    #[default]
    NotDetermined,
    /// SPI absent, failed, or returned a value we do not understand.
    Unavailable(String),
}

#[cfg(any(target_os = "macos", test))]
impl CapturePreflight {
    fn from_raw(result: Result<u64, String>) -> Self {
        match result {
            Ok(0) => Self::Denied,
            Ok(1) => Self::NotDetermined,
            Ok(2) => Self::Authorized,
            Ok(value) => Self::Unavailable(format!("TCC preflight returned unknown value {value}")),
            Err(reason) => Self::Unavailable(reason),
        }
    }
}

#[cfg(target_os = "macos")]
pub fn capture_permission_preflight() -> CapturePreflight {
    query(c"tcc_authorization_preflight")
}

#[cfg(target_os = "macos")]
fn query(symbol: &std::ffi::CStr) -> CapturePreflight {
    unsafe extern "C" {
        fn oink_audio_capture_preflight(
            symbol: *const std::ffi::c_char,
            result: *mut u64,
            error: *mut std::ffi::c_char,
            error_size: usize,
        ) -> bool;
    }
    let mut result = 0;
    let mut error = [0 as std::ffi::c_char; 512];
    // SAFETY: valid NUL-terminated symbol and writable out parameters; native
    // bridge writes a bounded NUL-terminated error on every failure path.
    let ok = unsafe { oink_audio_capture_preflight(symbol.as_ptr(), &mut result, error.as_mut_ptr(), error.len()) };
    let preflight = CapturePreflight::from_raw(if ok {
        Ok(result)
    } else {
        // SAFETY: zero-initialized buffer, bounded snprintf in native bridge.
        Err(unsafe { std::ffi::CStr::from_ptr(error.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    });
    let state = match &preflight {
        CapturePreflight::Authorized => "authorized",
        CapturePreflight::Denied => "denied",
        CapturePreflight::NotDetermined | CapturePreflight::Unavailable(_) => "unknown",
    };
    tracing::info!(raw_right = ?ok.then_some(result), state, detail = ?preflight,
        provider = "self_authorization", "system audio capture permission preflight");
    preflight
}

/// Request system-audio access from the responsible backend, only on user action.
#[cfg(target_os = "macos")]
pub fn request_capture_permission() -> Result<(), String> {
    unsafe extern "C" {
        fn oink_audio_capture_request(completion: extern "C" fn(u64)) -> i32;
    }
    extern "C" fn completed(result: u64) {
        tracing::info!(
            raw_right = result,
            decision = ?CapturePreflight::from_raw(Ok(result)),
            process_id = std::process::id(),
            "system audio TCC decision completed"
        );
        // No cached grant: the independent observer reads current authorization,
        // including revocation, and publishes the normal Audio snapshot.
    }
    // SAFETY: static callback remains valid until TCC completes; native code
    // owns the block and keeps the framework loaded while an alert is pending.
    let status = unsafe { oink_audio_capture_request(completed) };
    tracing::info!(
        status,
        process_id = std::process::id(),
        "system audio TCC request submitted"
    );
    if status == 0 {
        Ok(())
    } else {
        Err("System audio authorization request API unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_preflight_values_and_failures() {
        assert_eq!(CapturePreflight::from_raw(Ok(2)), CapturePreflight::Authorized);
        assert_eq!(CapturePreflight::from_raw(Ok(0)), CapturePreflight::Denied);
        assert_eq!(CapturePreflight::from_raw(Ok(1)), CapturePreflight::NotDetermined);
        for value in [3, 4, u64::MAX] {
            assert!(matches!(
                CapturePreflight::from_raw(Ok(value)),
                CapturePreflight::Unavailable(_)
            ));
        }
        assert_eq!(
            CapturePreflight::from_raw(Err("call failed".into())),
            CapturePreflight::Unavailable("call failed".into())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_symbol_is_explicitly_unavailable_without_querying_tcc() {
        let result = query(c"OinkDeliberatelyMissingTCCPreflightSymbol");
        assert!(matches!(result, CapturePreflight::Unavailable(reason) if reason.contains("dlsym failed")));
    }
}
