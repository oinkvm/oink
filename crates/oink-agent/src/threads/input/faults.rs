//! Native owners report only observed resource outcomes. Preference choices,
//! focus, peer availability, and cursor visibility never manufacture a fault.
use oink_foundation::input::PermissionState;
use oink_protocol::v1::dto::ResourceFaultCode;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceFaults {
    pub screen: Option<ResourceFaultCode>,
    pub mouse: Option<ResourceFaultCode>,
    pub keyboard: Option<ResourceFaultCode>,
}

pub(crate) struct NativeFaults([AtomicU32; 2]);
impl Default for NativeFaults {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU32::new(u32::MAX)))
    }
}
impl NativeFaults {
    pub fn screen(&self, code: ResourceFaultCode) -> bool {
        self.set(0, code)
    }
    pub fn mouse(&self, code: ResourceFaultCode) -> bool {
        self.set(1, code)
    }
    fn set(&self, index: usize, code: ResourceFaultCode) -> bool {
        let code = code.number();
        self.0[index].swap(code, Ordering::AcqRel) != code
    }
    pub fn snapshot(&self) -> ResourceFaults {
        let read = |index: usize| {
            let code = self.0[index].load(Ordering::Acquire);
            (code != u32::MAX).then(|| ResourceFaultCode::from_number(code))
        };
        ResourceFaults {
            screen: read(0),
            mouse: read(1),
            keyboard: None,
        }
    }
}

pub(crate) fn native_result(result: &Result<(), oink_input::InputError>) -> ResourceFaultCode {
    match result {
        Ok(()) => ResourceFaultCode::None,
        Err(oink_input::InputError::PermissionDenied(_)) => ResourceFaultCode::PermissionDenied,
        Err(oink_input::InputError::Unsupported) => ResourceFaultCode::Unsupported,
        Err(oink_input::InputError::NotStarted) => ResourceFaultCode::CaptureUnavailable,
        Err(oink_input::InputError::Platform(_)) => ResourceFaultCode::BackendFailed,
    }
}

pub(crate) fn combine(a: Option<ResourceFaultCode>, b: Option<ResourceFaultCode>) -> Option<ResourceFaultCode> {
    a.filter(|v| *v != ResourceFaultCode::None).or(b).or(a)
}

pub(crate) fn permission_fault(states: &[PermissionState]) -> Option<ResourceFaultCode> {
    if states.contains(&PermissionState::Denied) {
        Some(ResourceFaultCode::PermissionDenied)
    } else if states.iter().any(|state| !crate::node::granted(*state)) {
        Some(ResourceFaultCode::PermissionRequired)
    } else {
        None
    }
}
