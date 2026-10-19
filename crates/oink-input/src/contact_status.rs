use serde::{Deserialize, Serialize};

/// Non-private failure categories; no system error text or machine identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ContactInitError {
    #[error("platform unsupported")]
    PlatformUnsupported = 2,
    #[error("user32 unavailable")]
    User32Unavailable,
    #[error("CreateSyntheticPointerDevice2 unavailable")]
    CreateApiUnavailable,
    #[error("InjectSyntheticPointerInput unavailable")]
    InjectApiUnavailable,
    #[error("InjectTouchpadAction unavailable")]
    ActionApiUnavailable,
    #[error("DestroySyntheticPointerDevice unavailable")]
    DestroyApiUnavailable,
    #[error("CreateSyntheticPointerDevice2 rejected PT_TOUCHPAD")]
    DeviceCreationRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactInjectionStatus {
    /// Usable now, including the current input desktop and runtime health.
    pub supported: bool,
    /// The startup device creation succeeded (retained after runtime failure).
    pub initialized: bool,
    pub max_contacts: Option<u8>,
    pub init_error_kind: Option<ContactInitError>,
}

impl ContactInjectionStatus {
    pub fn from_capacity(capacity: Option<u8>) -> Self {
        Self {
            supported: capacity == Some(5),
            initialized: capacity.is_some(),
            max_contacts: capacity,
            init_error_kind: capacity.is_none().then_some(ContactInitError::PlatformUnsupported),
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn platform_status_preserves_initialization_and_error_kind() {
        let unsupported = ContactInjectionStatus::from_capacity(None);
        assert!(!unsupported.initialized && !unsupported.supported);
        assert_eq!(unsupported.init_error_kind, Some(ContactInitError::PlatformUnsupported));
        assert_eq!(unsupported.max_contacts, None);
        let ready = ContactInjectionStatus::from_capacity(Some(5));
        assert!(ready.initialized && ready.supported);
        assert_eq!(ready.init_error_kind, None);
    }
}
